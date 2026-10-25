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
use rig_core::tool::{Tool as _, ToolContext, ToolExecutionError, ToolResult};
use tracing::Instrument;
use uuid::Uuid;

use super::ask::{Ask, AskArgs};
use super::db::{self, ClaimedRun};
use super::{AgentKind, Durable, FailReason, RunId, RunStatus, ToolCallKey};

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

/// What [`Attempt::with_lease_guards`] produced: the guarded future's
/// output, or a decision to stop driving this run.
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
            input_tokens: usage.input_tokens.unwrap_or(0).clamp(0, i64::MAX as u64) as i64,
            output_tokens: usage.output_tokens.unwrap_or(0).clamp(0, i64::MAX as u64) as i64,
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

/// The transcript role of a rig message (its serde tag is `role`).
fn message_role(message: &Message) -> &'static str {
    match message {
        Message::System { .. } => "system",
        Message::User { .. } => "user",
        Message::Assistant(_) => "assistant",
    }
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
}

impl Durable {
    /// Advance one run as a cja job attempt. Never in the request path;
    /// `shutdown` is the worker's shutdown token (deploys), a different
    /// signal from user cancellation.
    pub(crate) async fn advance_run(
        &self,
        db: &sqlx::PgPool,
        run_id: Uuid,
        shutdown: CancellationToken,
    ) -> cja::color_eyre::Result<()> {
        let me = Uuid::new_v4().to_string();
        let row = match tokio::time::timeout(
            WRITE_BOUND,
            db::try_claim(db, run_id, &me, self.lease_takeover),
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
            return match db::current_status(db, run_id).await? {
                Some(s) if s == RunStatus::Running.as_str() => {
                    Err(cja::color_eyre::eyre::eyre!(
                        "run {run_id} leased by another worker; backing off"
                    ))
                }
                _ => Ok(()),
            };
        };

        // User cancel may have landed before we claimed the row: finish
        // cancelled without advancing, preserving the transcript.
        if row.cancel_requested {
            let snap = RunSnapshot::from_row(&row);
            self.finish_run(db, run_id, &me, RunStatus::Cancelled, None, None, Some("cancelled by request".to_owned()), snap)
                .await?;
            return Ok(());
        }

        // Unknown kind is permanent: every retry would re-read the same row
        // forever (and eventually dead-letter the job, orphaning the run).
        // Fail the run, complete the job.
        let Some(kind) = self.kind_named(&row.kind) else {
            let snap = RunSnapshot::from_row(&row);
            self.finish_run(
                db,
                run_id,
                &me,
                RunStatus::Failed,
                Some(FailReason::UnknownKind),
                None,
                Some(format!("unknown agent kind `{}`", row.kind)),
                snap,
            )
            .await?;
            return Ok(());
        };

        // Corrupt run state (rig version skew, RUN_FORMAT mismatch) is
        // permanent for the same reason: fail the run, keep the transcript.
        let run: AgentRun = match serde_json::from_value(row.run_state.clone()) {
            Ok(run) => run,
            Err(e) => {
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
                    &me,
                    RunStatus::Failed,
                    Some(FailReason::CorruptState),
                    None,
                    Some(message.to_owned()),
                    snap,
                )
                .await?;
                return Ok(());
            }
        };
        let mut run = run;

        // Rebuild the per-attempt artifacts from the kind. The model id
        // comes from the ROW (written by `start` from `kind.model`), so the
        // builder and the wire call can never disagree.
        let agent = (kind.build)(&self.llm, &row.model).build();
        let mut spec = agent.run_spec();
        // The AgentBuilder has no setter for this (rig exposes one only on
        // the runner); the durable engine applies the kind's policy over
        // the built spec, exactly as `start` does.
        spec.unhandled_invalid_tool_call = kind.unhandled_invalid_tool_call;
        let toolset = agent.tool_server_handle().toolset();
        let model = self.llm.client().completion(row.model.clone());
        let capabilities = model.capabilities().completion;
        let attempt = Attempt {
            durable: self,
            db,
            run_id,
            me: me.clone(),
            kind,
            model_name: row.model.clone(),
            spec,
            toolset,
            model,
            capabilities,
            shutdown,
        };

        loop {
            // Between-steps shutdown boundary (deploy): abort, release, Err
            // — cja retries at backoff and the run resumes at the last
            // persist. A shutdown never marks the run cancelled or failed.
            if attempt.shutdown.is_cancelled() {
                return self
                    .fail_retryable(
                        db,
                        run_id,
                        &me,
                        cja::color_eyre::eyre::eyre!("shutdown requested between steps"),
                    )
                    .await;
            }

            let step = match run.next_step() {
                Ok(step) => step,
                Err(error) => {
                    let (reason, message) = sanitize_prompt_error(&error);
                    attempt.fail_terminal(&run, reason, message).await;
                    return Ok(());
                }
            };

            let outcome = match step {
                AgentRunStep::CallModel {
                    prompt,
                    history,
                    turn,
                } => attempt.call_model_step(&mut run, prompt, history, turn).await,
                AgentRunStep::CallTools { calls } => attempt.call_tools_step(&mut run, calls).await,
                AgentRunStep::Done(response) => {
                    let snap = RunSnapshot::of(&run)?;
                    self.finish_run(
                        db,
                        run_id,
                        &me,
                        RunStatus::Succeeded,
                        None,
                        Some(response.output()),
                        None,
                        snap,
                    )
                    .await?;
                    return Ok(());
                }
            };
            match outcome {
                StepOutcome::Continue => continue,
                StepOutcome::Done => return Ok(()),
                StepOutcome::Retry(error) => {
                    return self.fail_retryable(db, run_id, &me, error).await;
                }
                #[cfg(feature = "testing")]
                StepOutcome::Injected(error) => return Err(error),
            }
        }
    }

    /// Release the lease (best-effort, bounded) and return the error, so
    /// cja's retry can re-claim immediately instead of waiting for the
    /// takeover window. Called before every `Err` return from the job.
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
    ) -> cja::color_eyre::Result<()> {
        let wrote = tokio::time::timeout(
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
            ),
        )
        .await
        .map_err(|_| cja::color_eyre::eyre::eyre!("run finish timed out"))??;
        let _ = wrote; // false = lease lost: the run moved on without us
        Ok(())
    }
}

impl Attempt<'_> {
    /// Fail the run terminally (no retry) with a typed reason; the attempt
    /// then ends Ok so the job completes instead of storming retries.
    async fn fail_terminal(&self, run: &AgentRun, reason: FailReason, message: &str) -> StepOutcome {
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
            )
            .await
        {
            Ok(()) => StepOutcome::Done,
            Err(e) => StepOutcome::Retry(e),
        }
    }

    /// Persist the run after a state transition, projecting any newly
    /// committed messages, then check for user cancellation.
    async fn persist(&self, run: &AgentRun) -> StepOutcome {
        let snap = match RunSnapshot::of(run) {
            Ok(snap) => snap,
            Err(e) => return StepOutcome::Retry(e),
        };
        let written = match tokio::time::timeout(
            WRITE_BOUND,
            sqlx::query_scalar::<_, i64>(
                "SELECT COALESCE(MAX(seq) + 1, 0) FROM agent_messages WHERE run_id = $1",
            )
            .bind(self.run_id)
            .fetch_one(self.db),
        )
        .await
        {
            Err(_) => {
                return StepOutcome::Retry(cja::color_eyre::eyre::eyre!(
                    "transcript projection read timed out"
                ))
            }
            Ok(Err(e)) => return StepOutcome::Retry(e.into()),
            Ok(Ok(written)) => written,
        };
        let mut new_messages = Vec::new();
        for (i, message) in run
            .messages()
            .iter()
            .enumerate()
            .skip(usize::try_from(written).unwrap_or(usize::MAX))
        {
            let body = match serde_json::to_value(&**message) {
                Ok(body) => body,
                Err(e) => {
                    return StepOutcome::Retry(cja::color_eyre::eyre::eyre!(
                        "failed to serialize transcript message {i}: {e}"
                    ))
                }
            };
            new_messages.push((written + i as i64, message_role(message).to_owned(), body));
        }
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
                return StepOutcome::Retry(cja::color_eyre::eyre::eyre!(
                    "run persist timed out"
                ))
            }
            Ok(Err(e)) => return StepOutcome::Retry(e.into()),
            Ok(Ok(outcome)) => outcome,
        };
        match wrote {
            None => StepOutcome::Done, // lease lost: stop silently, never write again
            Some(true) => {
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
                    )
                    .await
                {
                    Ok(()) => StepOutcome::Done,
                    Err(e) => StepOutcome::Retry(e),
                }
            }
            Some(false) => {
                #[cfg(feature = "testing")]
                {
                    let count = self.durable.persist_count.fetch_add(1, Ordering::SeqCst) + 1;
                    if self.durable.fail_after_persist.load(Ordering::SeqCst) == count {
                        return StepOutcome::Injected(cja::color_eyre::eyre::eyre!(
                            "injected failure after persist"
                        ));
                    }
                }
                StepOutcome::Continue
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
        // re-issued `CallModel`, which consumes no turn.
        match self.persist(run).await {
            StepOutcome::Continue => {}
            other => return other,
        }

        let policy = prepared.policy.clone();
        let request = prepared.apply(rig_core::completion::CompletionRequest::new((*prompt).clone()));
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
                let turn =
                    ModelTurn::from_policy(&response, policy);
                let mut outcome = match run.model_response(turn) {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        let (reason, message) = sanitize_prompt_error(&error);
                        return self.fail_terminal(run, reason, message).await;
                    }
                };
                loop {
                    match outcome {
                        ModelTurnOutcome::NeedsResolution(_) => {
                            // Spec policy only (no hooks here): Fail (default)
                            // or Ignore.
                            outcome = match run.resolve_unhandled_invalid_tool_call() {
                                Ok(outcome) => outcome,
                                Err(error) => {
                                    let (reason, message) = sanitize_prompt_error(&error);
                                    return self.fail_terminal(run, reason, message).await;
                                }
                            };
                        }
                        // The corrective feedback is already committed; the
                        // outer loop's `next_step()` re-issues `CallModel`.
                        // Never `continue` — that re-matches this outcome
                        // forever.
                        ModelTurnOutcome::TurnRetried | ModelTurnOutcome::Continue { .. } => {
                            break;
                        }
                    }
                }
            }
        }

        // The assistant message is now committed: persist it before anything
        // else happens to the run.
        match self.persist(run).await {
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
    async fn call_tools_step(&self, run: &mut AgentRun, calls: Vec<PendingToolCall>) -> StepOutcome {
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
                            // model, and the run continues.
                            if let Err(error) = run.answer(exec.answer(result)) {
                                let (reason, message) = sanitize_prompt_error(&error);
                                return self.fail_terminal(run, reason, message).await;
                            }
                            match self.persist(run).await {
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
                    match self.persist(run).await {
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
    async fn ask_step(
        &self,
        run: &mut AgentRun,
        exec: rig_agent::run::ExecCall,
    ) -> StepOutcome {
        match serde_json::from_value::<AskArgs>(exec.arguments()) {
            Ok(args) => {
                let snap = match RunSnapshot::of(run) {
                    Ok(snap) => snap,
                    Err(e) => return StepOutcome::Retry(e),
                };
                let wrote = match tokio::time::timeout(
                    WRITE_BOUND,
                    db::set_waiting(
                        self.db,
                        self.run_id,
                        &self.me,
                        &snap.run_state,
                        &exec.id().wire(),
                        &args.question,
                    ),
                )
                .await
                {
                    Err(_) => {
                        return StepOutcome::Retry(cja::color_eyre::eyre::eyre!(
                            "suspension write timed out"
                        ))
                    }
                    Ok(Err(e)) => return StepOutcome::Retry(e.into()),
                    Ok(Ok(wrote)) => wrote,
                };
                // `false` = lease lost: the thief continues the run; either
                // way this attempt may not write again.
                let _ = wrote;
                StepOutcome::Done
            }
            Err(_) => {
                if let Err(error) = run.answer(exec.answer(ToolResult::failed(
                    ToolExecutionError::invalid_args("ask requires a string `question`"),
                ))) {
                    let (reason, message) = sanitize_prompt_error(&error);
                    return self.fail_terminal(run, reason, message).await;
                }
                // The failed tool result is committed; the run continues.
                self.persist(run).await
            }
        }
    }

    /// Guard an in-flight step future with the lease heartbeat and the
    /// shutdown token. The future is pinned ONCE outside the loop and polled
    /// for the whole guard — never dropped, never restarted. Only three
    /// outcomes leave the loop early: the future completing, a lost/cancelled
    /// heartbeat (`Stop`), and shutdown (`Err`).
    #[allow(clippy::future_not_send)] // one task at a time; no cross-task awaits
    async fn with_lease_guards<F>(&self, run: &AgentRun, fut: F) -> cja::color_eyre::Result<Guarded<F::Output>>
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
                        // A heartbeat timeout means ownership cannot be
                        // proven: abandon the step and return Ok, never
                        // write again. Same for a failed or lost heartbeat.
                        Err(_) | Ok(Err(_)) | Ok(Ok(None)) => return Ok(Guarded::Stop),
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
                                )
                                .await?;
                            return Ok(Guarded::Stop);
                        }
                        Ok(Ok(Some(false))) => continue, // still owner: keep awaiting the SAME future
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
