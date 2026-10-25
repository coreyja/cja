//! The durable worker: drives rig's sans-I/O `AgentRun` one step at a time,
//! persisting after every state transition.
//!
//! The runner (`AgentRunner`) cannot hand its `AgentRun` back, so this module
//! replays the runner's driving protocol itself (mirroring
//! `rig-agent/src/agent/engine.rs`): `next_step()` → `CallModel` (prepare,
//! advertise, `model.call`, `model_response`) → `CallTools` (execute via the
//! built agent's toolset, `run.answer`) → `Done`. Every `Err` return from the
//! job goes through [`Durable::fail_retryable`] so the lease is released and
//! cja's backoff can retry immediately.
//!
//! No panics or unwraps anywhere on this path: a panic is caught by cja's
//! worker and retried 20× into dead-letter on a permanent condition,
//! orphaning the run.

#[cfg(feature = "testing")]
use std::sync::atomic::Ordering;
use std::time::Duration;

use cja::jobs::CancellationToken;
use rig_agent::run::prepare::{PrepareError, prepare_request};
use rig_agent::run::response::PromptError;
use rig_agent::run::{AgentRun, AgentRunStep, ModelTurn, ModelTurnOutcome, PendingToolCall};
use rig_core::completion::Message;
use rig_core::completion::ModelRef;
use rig_core::completion::request::ProviderCapabilities;
use rig_core::providers::openai::wire::OpenAiWire;
use rig_core::tool::{Tool as _, ToolContext, ToolExecutionError, ToolOutput, ToolResult};
use tracing::Instrument;
use uuid::Uuid;

use super::ask::{Ask, AskArgs};
use super::db::{self, ClaimedRun};
use super::{AgentKind, Durable, FailReason, RunId, RunStatus, ToolCallKey, replace_nuls};

/// How long any single lease-holding DB write may take before ownership is
/// treated as unprovable (an operator transaction holding the `agent_runs`
/// row would otherwise pin the lease indefinitely).
const WRITE_BOUND: Duration = Duration::from_secs(5);

/// Bound on the best-effort lease release, sized to land inside cja's
/// default 2s `shutdown_drain_timeout`.
const RELEASE_BOUND: Duration = Duration::from_secs(1);

/// The outcome of one step for the outer driver loop.
enum StepOutcome {
    /// Call `next_step()` again.
    Continue,
    /// The job attempt ends successfully (terminal, suspended, cancelled, or
    /// ownership lost — nothing more for this attempt to do).
    Done,
    /// The job attempt fails: cja backs off and retries.
    Retry(cja::color_eyre::Report),
    /// Testing-only injected "process death" after a committed persist.
    /// Propagates straight out of the job — no lease release, no error
    /// classification — exactly what a killed process looks like.
    #[cfg(feature = "testing")]
    Injected(cja::color_eyre::Report),
}

/// The outcome of [`Durable::claim_and_validate`]: a ready run, or nothing
/// left to do (with `leased_by_other` distinguishing back-off from no-op).
enum Claim {
    Nothing {
        leased_by_other: bool,
    },
    Ready {
        row: db::ClaimedRun,
        run: Box<AgentRun>,
        kind: &'static AgentKind,
    },
}

/// What [`Attempt::with_lease_guards`] produced: the guarded future's
/// output, or a decision to stop driving this run.
#[derive(Debug)]
enum Guarded<T> {
    Done(T),
    Stop,
}

/// The run state and counters a terminal or persistence write needs.
struct RunSnapshot {
    run_state: serde_json::Value,
    turns: i32,
    input_tokens: i64,
    output_tokens: i64,
}

impl RunSnapshot {
    fn of(run: &AgentRun) -> cja::color_eyre::Result<Self> {
        let usage = run.usage();
        Ok(Self {
            run_state: serde_json::to_value(run)
                .map_err(|e| cja::color_eyre::eyre::eyre!("failed to serialize run state: {e}"))?,
            turns: i32::try_from(run.turn()).unwrap_or(i32::MAX),
            input_tokens: i64::try_from(usage.input_tokens.unwrap_or(0)).unwrap_or(i64::MAX),
            output_tokens: i64::try_from(usage.output_tokens.unwrap_or(0)).unwrap_or(i64::MAX),
        })
    }

    /// From the claimed row, for terminal paths that do not advance the run
    /// (cancel-at-claim, unknown kind, corrupt state).
    fn from_row(row: &ClaimedRun) -> Self {
        Self {
            run_state: row.run_state.clone(),
            turns: row.turns,
            input_tokens: row.input_tokens,
            output_tokens: row.output_tokens,
        }
    }
}

/// Total tokens reported so far.
fn total_tokens(run: &AgentRun) -> u64 {
    let usage = run.usage();
    usage.input_tokens.unwrap_or(0) + usage.output_tokens.unwrap_or(0)
}

/// Map a rig [`PromptError`] to a typed failure before it touches the
/// database, a log, or an eyre report: `PromptError` is NOT content-safe
/// (`MaxTurns` carries the whole conversation; provider variants print
/// bodies), so only the variant classification survives.
fn sanitize_prompt_error(error: &PromptError) -> (FailReason, &'static str) {
    match error {
        PromptError::MaxTurns { .. } => (FailReason::TurnBudget, "turn budget exhausted"),
        PromptError::Cancelled { .. } => (FailReason::Protocol, "run cancelled"),
        PromptError::UnknownToolCall { .. } => (
            FailReason::Protocol,
            "model called an unknown or disallowed tool",
        ),
        PromptError::Provider(_) => (FailReason::Protocol, "provider rejected the turn"),
        PromptError::Report(_) => (FailReason::Protocol, "structured effect failure"),
        PromptError::Memory { .. } => (FailReason::Protocol, "conversation memory failure"),
        _ => (FailReason::Protocol, "run failed"),
    }
}

/// Classify a transport-classified cja-llm error: retryable (release the
/// lease, error the job, let cja's backoff retry it) or terminal (fail the
/// run now — no retry storm). `Timeout` is deliberately NOT retryable: the
/// attempt may have been billed upstream, and re-calling would double-bill
/// and fork the transcript.
fn is_retryable(error: &crate::Error) -> bool {
    match error {
        crate::Error::Connect { .. } => true,
        crate::Error::Upstream { status, .. } => *status == 429 || *status >= 500,
        _ => false,
    }
}

/// Whether any string in a JSON tool output carries a NUL.
fn json_has_nul(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(text) => text.contains('\0'),
        serde_json::Value::Array(items) => items.iter().any(json_has_nul),
        serde_json::Value::Object(map) => {
            map.keys().any(|key| key.contains('\0')) || map.values().any(json_has_nul)
        }
        _ => false,
    }
}

fn sanitize_json_nuls(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(text) => {
            serde_json::Value::String(replace_nuls(&text).into_owned())
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(sanitize_json_nuls).collect())
        }
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.into_iter()
                .map(|(key, value)| (replace_nuls(&key).into_owned(), sanitize_json_nuls(value)))
                .collect(),
        ),
        other => other,
    }
}

/// Replace U+0000 with U+FFFD in a tool result BEFORE it enters the run
/// (`run.answer`): Postgres cannot store a NUL (JSONB rejects the escape,
/// TEXT the byte), so an unsanitized NUL would make the run's state
/// unstorable — and because a tool whose result was not persisted
/// re-executes on every resume, the job would retry the same poison
/// payload into dead-letter and orphan the run. Sanitizing before the
/// result enters the run keeps the in-memory run and the stored transcript
/// identical, which crash-resume identity depends on. Single text and
/// single JSON blocks cover every ordinary tool output; multimodal /
/// multi-block outputs pass through, and a NUL there fails the run
/// `unstorable_content` at the next persist instead.
fn sanitize_tool_result(result: ToolResult) -> ToolResult {
    // Decide first (the borrows end here), then rebuild: `with_output`
    // consumes the result.
    let replacement = if let Some(text) = result.output().as_text() {
        if text.contains('\0') {
            Some(ToolOutput::text(replace_nuls(text).into_owned()))
        } else {
            None
        }
    } else if let Some(json) = result.output().as_json() {
        json_has_nul(json).then(|| ToolOutput::json(sanitize_json_nuls(json.clone())))
    } else {
        None
    };
    match replacement {
        Some(output) => result.with_output(output),
        None => result,
    }
}

/// Compute the transcript rows not yet written for `run`: everything from
/// the stored high-water mark on, via the one shared projection
/// ([`db::project_rows`]) so the worker's persists and `answer`'s write
/// cannot drift apart.
async fn project_messages(
    db: &sqlx::PgPool,
    run_id: Uuid,
    run: &AgentRun,
) -> cja::color_eyre::Result<Vec<(i64, String, serde_json::Value)>> {
    let written = db::next_message_seq(db, run_id)
        .await
        .map_err(cja::color_eyre::Report::from)?;
    db::project_rows(written, run).map_err(cja::color_eyre::Report::from)
}

/// Everything one attempt's step helpers need, built once per attempt.
struct Attempt<'a> {
    durable: &'a Durable,
    db: &'a sqlx::PgPool,
    run_id: Uuid,
    me: String,
    kind: &'static AgentKind,
    model_name: String,
    spec: rig_agent::run::RunSpec,
    toolset: rig_agent::tool::ToolSet,
    model: rig_core::Model<OpenAiWire>,
    capabilities: ProviderCapabilities,
    shutdown: CancellationToken,
    /// Whether this attempt has persisted an ADVANCING step (a model
    /// response or tool answer committed), set by [`Attempt::persist`].
    /// A later retryable error then hands the run to a fresh job row
    /// ([`Durable::retry_or_requeue`]) instead of letting cja's backoff
    /// escalate on the run's only job: errors separated by progress must
    /// never add up to a dead-letter. The pre-call checkpoint persist is
    /// deliberately NOT progress — a failure before any step completed has
    /// made none.
    progress: std::sync::atomic::AtomicBool,
}

impl Durable {
    /// Claim the run and validate its permanent preconditions (unknown kind,
    /// corrupt state, cancel-at-claim). One helper so `advance_run` stays
    /// under the line budget: everything here either yields a ready run or
    /// has already completed the attempt's terminal work.
    async fn claim_and_validate(
        &self,
        db: &sqlx::PgPool,
        run_id: Uuid,
        me: &str,
    ) -> cja::color_eyre::Result<Claim> {
        let row = match tokio::time::timeout(
            WRITE_BOUND,
            db::try_claim(db, run_id, me, self.lease_takeover),
        )
        .await
        {
            Err(_) => {
                return Err(cja::color_eyre::eyre::eyre!("run claim timed out"));
            }
            Ok(Err(e)) => return Err(e.into()),
            Ok(Ok(row)) => row,
        };
        let Some(row) = row else {
            // Not claimable. Running under a fresh foreign lease: back off
            // (Err), so a duplicate job can never delete the job the live
            // owner depends on. Anything else (missing, terminal, waiting)
            // is a successful no-op.
            let leased_by_other = matches!(
                db::current_status(db, run_id).await?,
                Some(s) if s == RunStatus::Running.as_str()
            );
            return Ok(Claim::Nothing { leased_by_other });
        };

        // User cancel may have landed before we claimed the row: finish
        // cancelled without advancing, preserving the transcript.
        if row.cancel_requested {
            let snap = RunSnapshot::from_row(&row);
            self.finish_run(
                db,
                run_id,
                me,
                RunStatus::Cancelled,
                None,
                None,
                Some("cancelled by request".to_owned()),
                snap,
                None,
            )
            .await?;
            return Ok(Claim::Nothing {
                leased_by_other: false,
            });
        }

        // Unknown kind is permanent: every retry would re-read the same row
        // forever (and eventually dead-letter the job, orphaning the run).
        // Fail the run, complete the job.
        let Some(kind) = self.kind_named(&row.kind) else {
            let snap = RunSnapshot::from_row(&row);
            self.finish_run(
                db,
                run_id,
                me,
                RunStatus::Failed,
                Some(FailReason::UnknownKind),
                None,
                Some(format!("unknown agent kind `{}`", row.kind)),
                snap,
                None,
            )
            .await?;
            return Ok(Claim::Nothing {
                leased_by_other: false,
            });
        };

        // Corrupt run state (rig version skew, RUN_FORMAT mismatch) is
        // permanent for the same reason: fail the run, keep the transcript.
        let run: AgentRun = match serde_json::from_value(row.run_state.clone()) {
            Ok(run) => run,
            Err(e) => {
                // Content-free: the classification and position only —
                // serde type-mismatch messages quote the offending value,
                // and run_state is the whole conversation.
                let message = format!(
                    "run state failed to deserialize ({:?} at line {} column {})",
                    e.classify(),
                    e.line(),
                    e.column()
                );
                let snap = RunSnapshot::from_row(&row);
                self.finish_run(
                    db,
                    run_id,
                    me,
                    RunStatus::Failed,
                    Some(FailReason::CorruptState),
                    None,
                    Some(message),
                    snap,
                    None,
                )
                .await?;
                return Ok(Claim::Nothing {
                    leased_by_other: false,
                });
            }
        };
        Ok(Claim::Ready {
            row,
            run: Box::new(run),
            kind,
        })
    }

    /// Advance one run as a cja job attempt. Never in the request path;
    /// `shutdown` is the worker's shutdown token (deploys), a different
    /// signal from user cancellation. Takes the app state so a
    /// progress-separated retryable error can enqueue a fresh job row and
    /// wake the worker ([`Self::retry_or_requeue`]).
    pub(crate) async fn advance_run<S: super::HasDurable>(
        &self,
        state: &S,
        run_id: Uuid,
        shutdown: CancellationToken,
    ) -> cja::color_eyre::Result<()> {
        let db = state.db();
        let me = Uuid::new_v4().to_string();
        // Every Err exit from claim validation releases the lease first: the
        // terminal writes inside it run under our fresh lease and can fail.
        let (row, run, kind) = match self.claim_and_validate(db, run_id, &me).await {
            Ok(Claim::Nothing {
                leased_by_other: true,
            }) => {
                // Back off (Err), so a duplicate job can never delete the
                // job the live owner depends on. No lease is held here.
                return Err(cja::color_eyre::eyre::eyre!(
                    "run {run_id} leased by another worker; backing off"
                ));
            }
            // Missing, terminal, or waiting: nothing to advance.
            Ok(Claim::Nothing {
                leased_by_other: false,
            }) => return Ok(()),
            Ok(Claim::Ready { row, run, kind }) => (row, run, kind),
            // Zero progress by definition (no attempt was built yet).
            Err(e) => return self.fail_retryable(db, run_id, &me, e).await,
        };
        let mut run = run;

        // Rebuild the per-attempt artifacts from the kind. The model id
        // comes from the ROW (written by `start` from `kind.model`), so the
        // builder and the wire call can never disagree.
        let attempt = self.attempt(db, run_id, &me, kind, &row.model, shutdown);

        loop {
            // Between-steps shutdown boundary (deploy): abort, release, and
            // either requeue on a fresh row (progress was made) or error so
            // cja's backoff retries — the run resumes at the last persist.
            // A shutdown never marks the run cancelled or failed.
            if attempt.shutdown.is_cancelled() {
                return self
                    .retry_or_requeue(
                        state,
                        &attempt,
                        cja::color_eyre::eyre::eyre!("shutdown requested between steps"),
                    )
                    .await;
            }

            let step = match run.next_step() {
                Ok(step) => step,
                Err(error) => {
                    let (reason, message) = sanitize_prompt_error(&error);
                    match attempt.fail_terminal(&run, reason, message).await {
                        // The terminal write failed: release and let cja
                        // retry — returning Ok would delete the only job
                        // row while the run is still `running` under our
                        // lease.
                        StepOutcome::Retry(e) => {
                            return self.fail_retryable(db, run_id, &me, e).await;
                        }
                        _ => return Ok(()),
                    }
                }
            };

            let outcome = match step {
                AgentRunStep::CallModel {
                    prompt,
                    history,
                    turn,
                } => {
                    attempt
                        .call_model_step(&mut run, prompt, history, turn)
                        .await
                }
                AgentRunStep::CallTools { calls } => {
                    // One span per run step — tool executions and ask
                    // suspensions included — with the same content-free
                    // fields as the model steps.
                    let usage = run.usage();
                    let span = tracing::info_span!(
                        "agent_run.step",
                        run.id = %run_id,
                        run.kind = kind.name,
                        run.turn = run.turn(),
                        run.input_tokens = usage.input_tokens.unwrap_or(0),
                        run.output_tokens = usage.output_tokens.unwrap_or(0),
                    );
                    attempt
                        .call_tools_step(&mut run, calls)
                        .instrument(span)
                        .await
                }
                AgentRunStep::Done(response) => attempt.done_step(&run, response.output()).await,
            };
            match outcome {
                StepOutcome::Continue => {}
                StepOutcome::Done => return Ok(()),
                StepOutcome::Retry(error) => {
                    return self.retry_or_requeue(state, &attempt, error).await;
                }
                #[cfg(feature = "testing")]
                StepOutcome::Injected(error) => return Err(error),
            }
        }
    }

    /// Build the per-attempt artifacts from the kind. The model id comes
    /// from the row (written by `start` from `kind.model`), so the builder
    /// and the wire call can never disagree.
    fn attempt<'a>(
        &'a self,
        db: &'a sqlx::PgPool,
        run_id: Uuid,
        me: &str,
        kind: &'static AgentKind,
        model_name: &str,
        shutdown: CancellationToken,
    ) -> Attempt<'a> {
        let agent = (kind.build)(&self.llm, model_name).build();
        let mut spec = agent.run_spec();
        // The AgentBuilder has no setter for this (rig exposes one only on
        // the runner); the durable engine applies the kind's policy over
        // the built spec, exactly as `start` does.
        spec.unhandled_invalid_tool_call = kind.unhandled_invalid_tool_call;
        let toolset = agent.tool_server_handle().toolset();
        let model = self.llm.client().completion(model_name.to_owned());
        let capabilities = model.capabilities().completion;
        Attempt {
            durable: self,
            db,
            run_id,
            me: me.to_owned(),
            kind,
            model_name: model_name.to_owned(),
            spec,
            toolset,
            model,
            capabilities,
            shutdown,
            progress: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// A retryable error ended this attempt. If the attempt persisted at
    /// least one ADVANCING step since it claimed, hand the run to a FRESH
    /// job row instead of erroring: release + enqueue in one transaction,
    /// wake the worker, return `Ok`. cja deletes the errored row on `Ok`,
    /// and the fresh row starts at `error_count = 0` — so errors separated
    /// by successful steps never escalate cja's `2^(n+1)`s backoff and can
    /// never dead-letter the run's only job row (which would orphan the
    /// run: one `AdvanceRunJob` row exists from `start`/`answer` until the
    /// run suspends or finishes, and every `Err` return charges it).
    /// Zero-progress attempts keep the plain `Err` path (release + cja
    /// backoff): only consecutive no-progress failures may escalate. A
    /// crash between this transaction's commit and cja's delete of the old
    /// row leaves a duplicate — already handled by the claim protocol: the
    /// duplicate backs off on `Leased` or no-ops on `NothingToDo`.
    async fn retry_or_requeue<S: super::HasDurable>(
        &self,
        state: &S,
        attempt: &Attempt<'_>,
        error: cja::color_eyre::Report,
    ) -> cja::color_eyre::Result<()> {
        if attempt.progress.load(Ordering::SeqCst)
            && self.requeue_with_fresh_job(state, attempt).await
        {
            return Ok(());
        }
        self.fail_retryable(state.db(), attempt.run_id, &attempt.me, error)
            .await
    }

    /// Release the lease and enqueue a fresh [`super::AdvanceRunJob`] in
    /// ONE transaction, then wake the worker (cja deletes the errored row
    /// on the caller's `Ok`). Bounded by the lease-write bound; `false` on
    /// any failure or timeout so the caller falls back to the plain
    /// release + `Err` path — recovery is then bounded by lease takeover,
    /// never an orphan.
    async fn requeue_with_fresh_job<S: super::HasDurable>(
        &self,
        state: &S,
        attempt: &Attempt<'_>,
    ) -> bool {
        let run_id = attempt.run_id;
        let me = attempt.me.as_str();
        let context = format!("agent-run:{}", attempt.kind.name);
        let requeue = async {
            let mut tx = state.db().begin().await?;
            db::release_lease(&mut *tx, run_id, me).await?;
            super::enqueue_on_tx::<S>(&mut tx, run_id, context).await?;
            tx.commit().await?;
            Ok::<(), cja::color_eyre::Report>(())
        };
        match tokio::time::timeout(WRITE_BOUND, requeue).await {
            Ok(Ok(())) => {
                // Not a transaction-commit hook: wake only after commit.
                state.job_enqueued();
                true
            }
            _ => false,
        }
    }

    /// Release the lease (best-effort, bounded) and return the error, so
    /// cja's retry can re-claim immediately instead of waiting for the
    /// takeover window. Called before every zero-progress `Err` return from
    /// the job (progress-separated errors go through
    /// [`Self::retry_or_requeue`] instead).
    async fn fail_retryable(
        &self,
        db: &sqlx::PgPool,
        run_id: Uuid,
        me: &str,
        error: cja::color_eyre::Report,
    ) -> cja::color_eyre::Result<()> {
        let _ = tokio::time::timeout(RELEASE_BOUND, db::release_lease(db, run_id, me)).await;
        Err(error)
    }

    /// Fail a run whose persisted state cannot be stored (SQLSTATE
    /// 22P05/22021/54000 — a U+0000 in content the sanitizers cannot
    /// rewrite): keep the last good `run_state` and transcript (the
    /// failing write already rolled back; this writes neither) and mark
    /// the run `failed` with `unstorable_content`. Permanent by design:
    /// the attempt then ends `Ok`, so the job completes instead of
    /// retrying a poison payload into dead-letter and orphaning the run.
    /// `Ok(false)` (lease lost) is a silent stop, as everywhere.
    async fn fail_unstorable(
        &self,
        db: &sqlx::PgPool,
        run_id: Uuid,
        me: &str,
    ) -> cja::color_eyre::Result<()> {
        match tokio::time::timeout(WRITE_BOUND, db::fail_unstorable(db, run_id, me)).await {
            Err(_) => Err(cja::color_eyre::eyre::eyre!(
                "unstorable-content failure write timed out"
            )),
            Ok(Err(e)) => Err(e.into()),
            Ok(Ok(_)) => Ok(()),
        }
    }

    /// Terminal status write from `running` under our lease; bounded, and
    /// `Ok(false)` (lease lost) is a silent stop, not an error.
    #[allow(clippy::too_many_arguments)]
    async fn finish_run(
        &self,
        db: &sqlx::PgPool,
        run_id: Uuid,
        me: &str,
        status: RunStatus,
        fail_reason: Option<FailReason>,
        output: Option<String>,
        error: Option<String>,
        snap: RunSnapshot,
        run: Option<&AgentRun>,
    ) -> cja::color_eyre::Result<()> {
        // rig commits turns inside `next_step`, so a terminal write can be
        // the first write after a commit: project what is missing.
        let new_messages = match run {
            Some(run) => project_messages(db, run_id, run).await?,
            None => Vec::new(),
        };
        let wrote = match tokio::time::timeout(
            WRITE_BOUND,
            db::finish(
                db,
                run_id,
                me,
                status,
                fail_reason.map(|reason| reason.as_str()),
                output.as_deref(),
                error.as_deref(),
                &snap.run_state,
                snap.turns,
                snap.input_tokens,
                snap.output_tokens,
                &new_messages,
            ),
        )
        .await
        {
            Err(_) => {
                return Err(cja::color_eyre::eyre::eyre!("run finish timed out"));
            }
            Ok(Err(e)) => {
                // The finish's own payload can be unstorable too (e.g. a
                // model output carrying a NUL in the TEXT `output`
                // column): permanent — keep the last good state.
                if db::unstorable_content(&e) {
                    return self.fail_unstorable(db, run_id, me).await;
                }
                return Err(e.into());
            }
            Ok(Ok(wrote)) => wrote,
        };
        // `false` = lease lost: the run moved on without us — a silent stop,
        // never an error. Safe by the same orphan invariant as `persist`:
        // the taker needed a job row to claim through. A future path that
        // clears `locked_by` without enqueueing a job must not exist.
        let _ = wrote;
        Ok(())
    }
}

impl Attempt<'_> {
    /// The `Done` step: one final projection pass (the last assistant
    /// message is committed by `next_step`'s turn processing, after the
    /// step-level persist), then the terminal `succeeded` write. Split from
    /// the driver loop to keep `advance_run` small; a failed terminal write
    /// is a `Retry` so the caller routes it through `fail_retryable`
    /// (release + Err) instead of deleting the only job row.
    async fn done_step(&self, run: &AgentRun, output: String) -> StepOutcome {
        match self.persist(run, true).await {
            StepOutcome::Continue => {}
            other => return other,
        }
        let snap = match RunSnapshot::of(run) {
            Ok(snap) => snap,
            Err(e) => return StepOutcome::Retry(e),
        };
        match self
            .durable
            .finish_run(
                self.db,
                self.run_id,
                &self.me,
                RunStatus::Succeeded,
                None,
                Some(output),
                None,
                snap,
                Some(run),
            )
            .await
        {
            Ok(()) => StepOutcome::Done,
            Err(e) => StepOutcome::Retry(e),
        }
    }

    /// Fail the run terminally (no retry) with a typed reason; the attempt
    /// then ends Ok so the job completes instead of storming retries.
    async fn fail_terminal(
        &self,
        run: &AgentRun,
        reason: FailReason,
        message: &str,
    ) -> StepOutcome {
        let snap = match RunSnapshot::of(run) {
            Ok(snap) => snap,
            Err(e) => return StepOutcome::Retry(e),
        };
        match self
            .durable
            .finish_run(
                self.db,
                self.run_id,
                &self.me,
                RunStatus::Failed,
                Some(reason),
                None,
                Some(message.to_owned()),
                snap,
                Some(run),
            )
            .await
        {
            Ok(()) => StepOutcome::Done,
            Err(e) => StepOutcome::Retry(e),
        }
    }

    /// Persist the run after a state transition, projecting any newly
    /// committed messages, then check for user cancellation. When `advanced`
    /// is set (this persist follows a committed model response or tool
    /// answer — NOT the pre-call checkpoint) and the write lands, the
    /// attempt is marked as having made progress.
    async fn persist(&self, run: &AgentRun, advanced: bool) -> StepOutcome {
        // Crash injection (testing only): die at persist ENTRY, before the
        // DB write — simulating process death between persists, so the
        // on-disk state is exactly the previous persist's. One-shot: re-armed
        // only by the test. Without the reset, a crash before the model call
        // would loop forever (every resume repeats that same persist).
        #[cfg(feature = "testing")]
        {
            let count = self.durable.persist_count.fetch_add(1, Ordering::SeqCst) + 1;
            let armed = self.durable.fail_after_persist.load(Ordering::SeqCst);
            if armed != 0 && armed == count {
                // Disarm only on fire (one-shot): a `swap` here would
                // disarm on the first non-matching check and the injection
                // would never fire for n > 1.
                self.durable.fail_after_persist.store(0, Ordering::SeqCst);
                return StepOutcome::Injected(cja::color_eyre::eyre::eyre!(
                    "injected failure before persist"
                ));
            }
        }
        let snap = match RunSnapshot::of(run) {
            Ok(snap) => snap,
            Err(e) => return StepOutcome::Retry(e),
        };
        let new_messages = match project_messages(self.db, self.run_id, run).await {
            Ok(messages) => messages,
            Err(e) => return StepOutcome::Retry(e),
        };
        let wrote = match tokio::time::timeout(
            WRITE_BOUND,
            db::persist_step(
                self.db,
                self.run_id,
                &self.me,
                &snap.run_state,
                snap.turns,
                snap.input_tokens,
                snap.output_tokens,
                &new_messages,
            ),
        )
        .await
        {
            Err(_) => {
                return StepOutcome::Retry(cja::color_eyre::eyre::eyre!("run persist timed out"));
            }
            Ok(Err(e)) => {
                // A NUL in committed content the sanitizers cannot rewrite
                // (model-produced text) is PERMANENT: fail the run keeping
                // the last good state, and complete the job. A retryable
                // classification here would re-run tools and re-commit the
                // same unstorable payload until dead-letter, orphaning the
                // run.
                if db::unstorable_content(&e) {
                    return match self
                        .durable
                        .fail_unstorable(self.db, self.run_id, &self.me)
                        .await
                    {
                        Ok(()) => StepOutcome::Done,
                        Err(e) => StepOutcome::Retry(e),
                    };
                }
                return StepOutcome::Retry(e.into());
            }
            Ok(Ok(outcome)) => outcome,
        };
        match wrote {
            // Lease lost: stop silently, never write again. Safe ONLY
            // because a lost lease is provable (zero rows): whoever took it
            // claimed through a job row, so the run keeps an advancing job
            // (or is already terminal). Any future path that clears
            // `locked_by` without enqueueing a job would turn this Ok into
            // an orphan; such a path must enqueue.
            None => StepOutcome::Done,
            Some(cancel_requested) => {
                if advanced {
                    self.progress.store(true, Ordering::SeqCst);
                }
                if cancel_requested {
                    // User cancel at a step boundary: mark cancelled, keep the
                    // transcript, end the attempt.
                    match self
                        .durable
                        .finish_run(
                            self.db,
                            self.run_id,
                            &self.me,
                            RunStatus::Cancelled,
                            None,
                            None,
                            Some("cancelled by request".to_owned()),
                            snap,
                            Some(run),
                        )
                        .await
                    {
                        Ok(()) => StepOutcome::Done,
                        Err(e) => StepOutcome::Retry(e),
                    }
                } else {
                    StepOutcome::Continue
                }
            }
        }
    }

    /// One `CallModel` step: prepare + advertise + persist-before-call +
    /// guarded model call + `model_response` + persist + token budget.
    async fn call_model_step(
        &self,
        run: &mut AgentRun,
        prompt: std::sync::Arc<Message>,
        history: Vec<std::sync::Arc<Message>>,
        turn: usize,
    ) -> StepOutcome {
        let usage = run.usage();
        let span = tracing::info_span!(
            "agent_run.step",
            run.id = %self.run_id,
            run.kind = %self.kind.name,
            run.turn = turn,
            // Cumulative totals known before this step's call. Eyes reads
            // span-creation fields only; the post-call totals go in an event.
            run.input_tokens = usage.input_tokens.unwrap_or(0),
            run.output_tokens = usage.output_tokens.unwrap_or(0),
        );
        self.call_model_step_inner(run, prompt, history, turn)
            .instrument(span)
            .await
    }

    async fn call_model_step_inner(
        &self,
        run: &mut AgentRun,
        prompt: std::sync::Arc<Message>,
        history: Vec<std::sync::Arc<Message>>,
        turn: usize,
    ) -> StepOutcome {
        // Prepare the request exactly as the runner does: capabilities of the
        // model we will call, the toolset's definitions, the committed
        // output tool, no patch (hooks do not run on this path).
        let prepared = match prepare_request(
            &self.spec,
            &self.capabilities,
            &history,
            self.toolset.tool_definitions(),
            run.output_tool_name(),
            None,
        ) {
            Ok(prepared) => prepared,
            Err(PrepareError::Request(message)) => {
                // Built from the spec and tool names, never from model
                // output — safe to store.
                return self
                    .fail_terminal(
                        run,
                        FailReason::Protocol,
                        &format!("request preparation failed: {message}"),
                    )
                    .await;
            }
        };
        // What this request advertises becomes run data, so a resumed run
        // (or another driver) can re-pair the calls that come back.
        run.advertise_tools(turn, prepared.tools.clone());
        // Only issued attempts advance routing history, including provider
        // errors.
        run.set_previous_model(ModelRef::new(self.model_name.clone()));

        // Persist BEFORE the model call: a crash here resumes with a
        // re-issued `CallModel`, which consumes no turn. Not progress: no
        // step has completed yet.
        match self.persist(run, false).await {
            StepOutcome::Continue => {}
            other => return other,
        }

        let policy = prepared.policy.clone();
        let request = prepared.apply(rig_core::completion::CompletionRequest::new(
            (*prompt).clone(),
        ));
        match self.with_lease_guards(run, self.model.call(request)).await {
            Err(e) => return StepOutcome::Retry(e),
            Ok(Guarded::Stop) => return StepOutcome::Done,
            Ok(Guarded::Done(Err(provider_error))) => {
                let error = crate::Error::from_provider_error(provider_error);
                if is_retryable(&error) {
                    return StepOutcome::Retry(cja::color_eyre::eyre::eyre!(
                        "gateway error (retryable): {error}"
                    ));
                }
                // `Error`'s Display is content-safe.
                return self
                    .fail_terminal(run, FailReason::Gateway, &error.to_string())
                    .await;
            }
            Ok(Guarded::Done(Ok(response))) => {
                let turn = ModelTurn::from_policy(&response, policy);
                let mut outcome = match run.model_response(turn) {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        let (reason, message) = sanitize_prompt_error(&error);
                        return self.fail_terminal(run, reason, message).await;
                    }
                };
                while let ModelTurnOutcome::NeedsResolution(_) = outcome {
                    // Spec policy only (no hooks here): Fail (default) or
                    // Ignore.
                    outcome = match run.resolve_unhandled_invalid_tool_call() {
                        Ok(outcome) => outcome,
                        Err(error) => {
                            let (reason, message) = sanitize_prompt_error(&error);
                            return self.fail_terminal(run, reason, message).await;
                        }
                    };
                }
                // `TurnRetried` and `Continue` fall through: the corrective
                // feedback is already committed; the outer loop's
                // `next_step()` re-issues `CallModel`. (Never `continue` in
                // a match on the outcome — it would re-match forever.)
            }
        }

        // The assistant message is now committed: persist it before anything
        // else happens to the run.
        match self.persist(run, true).await {
            StepOutcome::Continue => {}
            other => return other,
        }

        let usage = run.usage();
        tracing::info!(
            run.turn = turn,
            run.input_tokens = usage.input_tokens.unwrap_or(0),
            run.output_tokens = usage.output_tokens.unwrap_or(0),
            "agent turn committed"
        );

        if total_tokens(run) > self.kind.limits.max_total_tokens {
            return self
                .fail_terminal(run, FailReason::TokenBudget, "token budget exceeded")
                .await;
        }
        StepOutcome::Continue
    }

    /// One `CallTools` step: intercept `ask` (suspend), execute everything
    /// else through the built agent's toolset with the idempotency key in
    /// its context, answer malformed calls with corrective feedback.
    async fn call_tools_step(
        &self,
        run: &mut AgentRun,
        calls: Vec<PendingToolCall>,
    ) -> StepOutcome {
        for call in calls {
            match call {
                PendingToolCall::Execute(exec) if exec.name().as_str() == Ask::NAME => {
                    return self.ask_step(run, exec).await;
                }
                PendingToolCall::Execute(exec) => {
                    let mut ctx = ToolContext::new();
                    if let Err(e) = ctx.insert(ToolCallKey {
                        run_id: RunId(self.run_id),
                        call_id: exec.id().wire().to_string(),
                    }) {
                        return StepOutcome::Retry(cja::color_eyre::eyre::eyre!(
                            "tool context insert failed: {e}"
                        ));
                    }
                    // Exactly what the runner passes (`engine.rs:1327`).
                    let args = exec.tool_call().function.arguments_json();
                    let name = exec.name().as_str().to_owned();
                    match self
                        .with_lease_guards(run, self.toolset.execute(name.as_str(), args, &mut ctx))
                        .await
                    {
                        Err(e) => return StepOutcome::Retry(e),
                        Ok(Guarded::Stop) => return StepOutcome::Done,
                        Ok(Guarded::Done(result)) => {
                            // `ToolSet::execute` is infallible: an unknown
                            // tool is a failed ToolResult fed back to the
                            // model, and the run continues. NULs in the
                            // output are sanitized BEFORE the result enters
                            // the run, so the run stays storable and
                            // crash-resume identity holds.
                            let result = sanitize_tool_result(result);
                            if let Err(error) = run.answer(exec.answer(result)) {
                                let (reason, message) = sanitize_prompt_error(&error);
                                return self.fail_terminal(run, reason, message).await;
                            }
                            match self.persist(run, true).await {
                                StepOutcome::Continue => {}
                                other => return other,
                            }
                        }
                    }
                }
                PendingToolCall::Malformed(malformed) => {
                    // No-hook default: corrective feedback, bounded by the
                    // spec's malformed-call budget. Never executed.
                    if let Err(error) = run.answer(malformed.answer(None)) {
                        let (reason, message) = sanitize_prompt_error(&error);
                        return self.fail_terminal(run, reason, message).await;
                    }
                    match self.persist(run, true).await {
                        StepOutcome::Continue => {}
                        other => return other,
                    }
                }
                _ => {
                    // `PendingToolCall` is `#[non_exhaustive]`; this build's
                    // rig only defines Execute and Malformed.
                    return StepOutcome::Retry(cja::color_eyre::eyre::eyre!(
                        "unsupported pending tool call variant"
                    ));
                }
            }
        }
        StepOutcome::Continue
    }

    /// An `ask` call: parse its arguments; on success suspend the run
    /// (waiting + relational ask identity + lease cleared, atomically) and
    /// end the attempt Ok — a waiting run holds no worker and no job. An
    /// ask whose arguments don't parse never suspends: answer it with a
    /// corrective failure and continue the run.
    async fn ask_step(&self, run: &mut AgentRun, exec: rig_agent::run::ExecCall) -> StepOutcome {
        let Ok(args) = serde_json::from_value::<AskArgs>(exec.arguments()) else {
            // An ask that doesn't parse never suspends: answer the call with
            // a corrective failure and continue the run. Never park a run
            // with no question.
            if let Err(error) = run.answer(exec.answer(ToolResult::failed(
                ToolExecutionError::invalid_args("ask requires a string `question`"),
            ))) {
                let (reason, message) = sanitize_prompt_error(&error);
                return self.fail_terminal(run, reason, message).await;
            }
            // The failed tool result is committed; the run continues.
            return self.persist(run, true).await;
        };
        let snap = match RunSnapshot::of(run) {
            Ok(snap) => snap,
            Err(e) => return StepOutcome::Retry(e),
        };
        let new_messages = match project_messages(self.db, self.run_id, run).await {
            Ok(messages) => messages,
            Err(e) => return StepOutcome::Retry(e),
        };
        // Belt and braces: the question lives inside the already-committed
        // assistant message (a NUL there fails the run `unstorable_content`
        // below), so a storable message implies a storable question — but
        // the TEXT column is bound directly, so sanitize anyway.
        let question = replace_nuls(&args.question).into_owned();
        let wrote = match tokio::time::timeout(
            WRITE_BOUND,
            db::set_waiting(
                self.db,
                self.run_id,
                &self.me,
                &snap.run_state,
                &exec.id().wire(),
                &question,
                &new_messages,
            ),
        )
        .await
        {
            Err(_) => {
                return StepOutcome::Retry(cja::color_eyre::eyre::eyre!(
                    "suspension write timed out"
                ));
            }
            Ok(Err(e)) => {
                // Unstorable suspension payload (a NUL in the assistant
                // message's projection): permanent, like every
                // unstorable write — never park on a poison payload.
                if db::unstorable_content(&e) {
                    return match self
                        .durable
                        .fail_unstorable(self.db, self.run_id, &self.me)
                        .await
                    {
                        Ok(()) => StepOutcome::Done,
                        Err(e) => StepOutcome::Retry(e),
                    };
                }
                return StepOutcome::Retry(e.into());
            }
            Ok(Ok(wrote)) => wrote,
        };
        // `None` = lease lost (the thief continues the run), `Some(true)` =
        // cancelled by request (never parked behind the question): either
        // way this attempt may not write again, and a suspension always
        // ends the attempt.
        let _ = wrote;
        StepOutcome::Done
    }

    /// Guard an in-flight step future with the lease heartbeat and the
    /// shutdown token. The future is pinned ONCE outside the loop and polled
    /// for the whole guard — never dropped, never restarted. Only three
    /// outcomes leave the loop early: the future completing, a provably
    /// lost/cancelled heartbeat (`Stop`), and shutdown or an unprovable
    /// heartbeat (`Err`, so cja retries — never a silent job completion
    /// while the lease may still be ours).
    ///
    /// Orphan invariant: returning `Ok` on a lost lease is safe ONLY
    /// because a lost lease is provable (zero rows): whoever took it needed
    /// a job row to claim through, so the run keeps an advancing job (or is
    /// already terminal). Any future path that clears `locked_by` without
    /// enqueueing a job — a lease sweeper, an operator tool — would
    /// reintroduce orphans here; such a path must enqueue.
    #[allow(clippy::future_not_send)] // one task at a time; no cross-task awaits
    async fn with_lease_guards<F>(
        &self,
        run: &AgentRun,
        fut: F,
    ) -> cja::color_eyre::Result<Guarded<F::Output>>
    where
        F: Future + Send,
        F::Output: Send,
    {
        let period = self.durable.lease_takeover / 3;
        let mut fut = std::pin::pin!(fut);
        // NOT a bare `interval()`: its first tick fires immediately. `Delay`
        // prevents burst catch-up after a slow step.
        let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                outcome = &mut fut => return Ok(Guarded::Done(outcome)),
                _ = tick.tick() => {
                    match tokio::time::timeout(
                        WRITE_BOUND,
                        db::heartbeat(self.db, self.run_id, &self.me),
                    )
                    .await
                    {
                        // A heartbeat TIMEOUT is not proof of a lost lease
                        // (e.g. an operator transaction holding the row
                        // lock past WRITE_BOUND): error the attempt so the
                        // caller routes it through `fail_retryable` (bounded
                        // release + cja backoff). Completing the job here
                        // would delete the only job row while the run may
                        // still be `running` under our lease — an orphan.
                        // No further writes after this.
                        Err(_) => {
                            return Err(cja::color_eyre::eyre::eyre!(
                                "lease heartbeat timed out; ownership unproven",
                            ));
                        }
                        // Zero rows = the lease was provably taken over: the
                        // taker needed a job row to claim through (or the run
                        // is terminal). Abandon the step and return Ok, never
                        // write again.
                        Ok(Ok(None)) => return Ok(Guarded::Stop),
                        // A DB error is NOT proof of a lost lease either:
                        // same treatment as the timeout — Err (release +
                        // cja backoff), never a silent completion.
                        Ok(Err(e)) => return Err(cja::color_eyre::Report::from(e)),
                        Ok(Ok(Some(true))) => {
                            // User cancel mid-step: finish cancelled, keep
                            // the transcript.
                            let snap = RunSnapshot::of(run)?;
                            self.durable
                                .finish_run(
                                    self.db,
                                    self.run_id,
                                    &self.me,
                                    RunStatus::Cancelled,
                                    None,
                                    None,
                                    Some("cancelled by request".to_owned()),
                                    snap,
                                    Some(run),
                                )
                                .await?;
                            return Ok(Guarded::Stop);
                        }
                        // Still owner, no cancel: keep awaiting the SAME future.
                        Ok(Ok(Some(false))) => {}
                    }
                }
                () = self.shutdown.cancelled() => {
                    // Deploy shutdown: drop the in-flight future, release the
                    // lease bounded at 1s (inside cja's 2s drain), and Err —
                    // the caller routes through `fail_retryable`, whose
                    // release is then a conditional no-op. Returning Ok
                    // here would hit cja's delete-on-Ok and orphan the run.
                    let _ = tokio::time::timeout(
                        RELEASE_BOUND,
                        db::release_lease(self.db, self.run_id, &self.me),
                    )
                    .await;
                    return Err(cja::color_eyre::eyre::eyre!("shutdown requested mid-step"));
                }
            }
        }
    }
}

#[cfg(all(test, feature = "durable"))]
mod tests {
    //! `with_lease_guards` pin-once loop structure: a step future that
    //! outlives several heartbeat periods is neither dropped nor restarted,
    //! heartbeats keep the lease fresh while it runs, and the guard returns
    //! its output intact.

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use cja::jobs::CancellationToken;
    use uuid::Uuid;

    use super::*;
    use crate::LlmConfig;
    use crate::durable::RunLimits;

    static GUARD_KIND: AgentKind = AgentKind {
        name: "guard_probe",
        model: "m",
        limits: RunLimits {
            max_turns: 20,
            max_total_tokens: 200_000,
        },
        unhandled_invalid_tool_call: rig_agent::run::UnhandledInvalidToolCall::Fail,
        build: |llm, model| llm.agent(model),
    };

    async fn test_db() -> sqlx::PgPool {
        let (pool, guard) = cja::testing::test_db::create_test_database("cja_llm_durable_w_")
            .await
            .expect("create test database");
        sqlx::migrate!("../cja/migrations")
            .run(&pool)
            .await
            .expect("cja migrations");
        crate::durable::run_migrations(&pool)
            .await
            .expect("durable migrations");
        std::mem::forget(guard);
        pool
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_completes_long_step_future() {
        let db = test_db().await;
        // A dead loopback URL: the guard test never calls the model.
        let config =
            LlmConfig::new("http://127.0.0.1:9".parse().expect("url")).app_name("guard-test");
        let durable = Durable::new(&config)
            .expect("durable")
            .kind(&GUARD_KIND)
            .expect("kind")
            // takeover 90ms -> heartbeat period 30ms.
            .lease_takeover(Duration::from_millis(90));

        // A running row leased by `me`.
        let run_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO agent_runs (id, kind, status, input, model, app, run_state) \
             VALUES ($1, 'guard_probe', 'running', 'in', 'm', 'a', '{}'::jsonb)",
        )
        .bind(run_id)
        .execute(&db)
        .await
        .expect("run row");
        let me = "me".to_owned();
        sqlx::query("UPDATE agent_runs SET locked_by = $2, locked_at = NOW() WHERE id = $1")
            .bind(run_id)
            .bind(&me)
            .execute(&db)
            .await
            .expect("lease");

        let attempt = durable.attempt(&db, run_id, &me, &GUARD_KIND, "m", CancellationToken::new());
        let run = AgentRun::new(rig_core::message::Message::user("probe"));

        // A step future that sleeps ~3.3 heartbeat periods before finishing.
        let started = Arc::new(AtomicUsize::new(0));
        let started_clone = started.clone();
        let step = async move {
            started_clone.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(100)).await;
            7u32
        };

        // While the guarded future runs, count distinct locked_at values:
        // every heartbeat rewrites it, so >= 2 distinct values prove the
        // heartbeats actually ticked over the window (deadline-bounded poll,
        // never a fixed sleep + snapshot).
        let watch_db = db.clone();
        let watch = tokio::spawn(async move {
            let mut distinct = std::collections::HashSet::new();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while distinct.len() < 2 && tokio::time::Instant::now() < deadline {
                if let Ok(Some(ts)) = sqlx::query_scalar::<_, String>(
                    "SELECT locked_at::text FROM agent_runs WHERE id = $1",
                )
                .bind(run_id)
                .fetch_optional(&watch_db)
                .await
                {
                    distinct.insert(ts);
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            distinct.len()
        });

        let outcome = attempt.with_lease_guards(&run, step).await.expect("guard");
        let ticks = watch.await.expect("watcher");

        assert!(
            matches!(outcome, Guarded::Done(7)),
            "the guarded future's output: {outcome:?}"
        );
        assert_eq!(
            started.load(Ordering::SeqCst),
            1,
            "the future was neither dropped nor restarted"
        );
        assert!(ticks >= 2, "heartbeats ticked at least twice: {ticks}");

        let (locked_by, status): (Option<String>, String) =
            sqlx::query_as("SELECT locked_by, status FROM agent_runs WHERE id = $1")
                .bind(run_id)
                .fetch_one(&db)
                .await
                .expect("row");
        assert_eq!(locked_by.as_deref(), Some("me"), "still leased by me");
        assert_eq!(status, "running");
    }
}
