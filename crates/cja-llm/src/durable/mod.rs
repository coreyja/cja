//! Durable agent runs: rig's `AgentRun` persisted in Postgres, advanced by
//! cja background jobs.
//!
//! See the crate README's "Durable agent runs" section for the full model:
//! kinds, leases, ask/answer suspension, failure classification, and the
//! enqueue/wake contract.

pub(crate) mod ask;
pub(crate) mod db;

pub use db::run_migrations;

pub use ask::{Ask, AskArgs};

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::Llm;
use crate::config::LlmConfig;

/// The identity of one durable agent run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RunId(pub uuid::Uuid);

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl From<uuid::Uuid> for RunId {
    fn from(id: uuid::Uuid) -> Self {
        Self(id)
    }
}

/// The run lifecycle, constrained by the database CHECK on `agent_runs.status`.
///
/// No derived `sqlx::Type`: the TEXT column decodes through `String` +
/// `FromStr` in [`RunView`]'s manual `FromRow` — one parsing path — and is
/// bound into SQL only via [`RunStatus::as_str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Queued,
    Running,
    Waiting,
    Succeeded,
    Failed,
    Cancelled,
}

impl RunStatus {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

impl fmt::Display for RunStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// [`FromStr`] failed: the stored value is not a known [`RunStatus`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunStatusParseError;

impl fmt::Display for RunStatusParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("unknown run status")
    }
}

impl std::error::Error for RunStatusParseError {}

impl FromStr for RunStatus {
    type Err = RunStatusParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "waiting" => Ok(Self::Waiting),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            _ => Err(RunStatusParseError),
        }
    }
}

/// Machine-readable failure vocabulary, stored in `agent_runs.fail_reason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailReason {
    TurnBudget,
    TokenBudget,
    UnknownKind,
    CorruptState,
    Gateway,
    Protocol,
}

impl FailReason {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::TurnBudget => "turn_budget",
            Self::TokenBudget => "token_budget",
            Self::UnknownKind => "unknown_kind",
            Self::CorruptState => "corrupt_state",
            Self::Gateway => "gateway",
            Self::Protocol => "protocol",
        }
    }
}

impl fmt::Display for FailReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// [`FromStr`] failed: the stored value is not a known [`FailReason`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailReasonParseError;

impl fmt::Display for FailReasonParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("unknown fail reason")
    }
}

impl std::error::Error for FailReasonParseError {}

impl FromStr for FailReason {
    type Err = FailReasonParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "turn_budget" => Ok(Self::TurnBudget),
            "token_budget" => Ok(Self::TokenBudget),
            "unknown_kind" => Ok(Self::UnknownKind),
            "corrupt_state" => Ok(Self::CorruptState),
            "gateway" => Ok(Self::Gateway),
            "protocol" => Ok(Self::Protocol),
            _ => Err(FailReasonParseError),
        }
    }
}

/// The suspended ask, decoded from `pending_call_id` + `pending_question`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAsk {
    pub call_id: String,
    pub question: String,
}

/// Everything the durable engine needs besides the database: the wired
/// [`Llm`], the registered kinds, and the lease window.
///
/// `lease_takeover` (default 90s) MUST stay below cja's
/// `JobWorkerConfig::reclaim_window` (default 120s): after an ungraceful
/// death, no job is runnable until cja reclaims, so a takeover window past
/// the reclaim window would only delay recovery further. Heartbeats fire
/// every `lease_takeover / 3`.
#[derive(Debug, Clone)]
pub struct Durable {
    llm: Llm,
    app_name: String,
    kinds: Vec<&'static AgentKind>,
    lease_takeover: Duration,
    #[cfg(feature = "testing")]
    fail_after_persist: std::sync::atomic::AtomicUsize,
    #[cfg(feature = "testing")]
    persist_count: std::sync::atomic::AtomicUsize,
}

impl Durable {
    /// Wire the [`Llm`] and adopt the config's app name. Kinds register
    /// afterward with [`Durable::kind`].
    pub fn new(config: &LlmConfig) -> crate::Result<Self> {
        let takeover = DEFAULT_LEASE_TAKEOVER;
        debug_assert!(takeover > Duration::ZERO && takeover / 3 > Duration::ZERO);
        Ok(Self {
            llm: Llm::new(config)?,
            app_name: {
                config.app_header_value()?; // validate (never empty / invalid)
                config.app_name.clone()
            },
            kinds: Vec::new(),
            lease_takeover: takeover,
            #[cfg(feature = "testing")]
            fail_after_persist: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(feature = "testing")]
            persist_count: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    /// Register an agent kind; a duplicate name is rejected
    /// ([`RunError::DuplicateKind`]).
    pub fn kind(mut self, kind: &'static AgentKind) -> Result<Self, RunError> {
        if self.kind_named(kind.name).is_some() {
            return Err(RunError::DuplicateKind(kind.name.to_owned()));
        }
        self.kinds.push(kind);
        Ok(self)
    }

    /// Override the lease takeover window (default 90s; heartbeats fire at
    /// `window / 3`). Keep it below the cja worker's `reclaim_window`.
    #[must_use]
    pub fn lease_takeover(mut self, window: Duration) -> Self {
        assert!(window > Duration::ZERO && window / 3 > Duration::ZERO);
        self.lease_takeover = window;
        self
    }

    /// The wired [`Llm`] this engine shares.
    #[must_use]
    pub fn llm(&self) -> &Llm {
        &self.llm
    }

    #[must_use]
    pub(crate) fn kind_named(&self, name: &str) -> Option<&'static AgentKind> {
        self.kinds.iter().copied().find(|kind| kind.name == name)
    }

    /// One-shot crash injection for the durability test: the `n`-th persist
    /// call fails AFTER writing (simulating process death — no lease
    /// release, no error return).
    #[cfg(feature = "testing")]
    pub fn fail_after_persist(&self, n: usize) {
        self.fail_after_persist
            .store(n, std::sync::atomic::Ordering::SeqCst);
    }

    /// How many persist calls have run on this engine (the reference run's
    /// count for the crash-at-every-step test).
    #[cfg(feature = "testing")]
    #[must_use]
    pub fn persist_count(&self) -> usize {
        self.persist_count.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// The run-lease takeover window: how stale a `running` lease must be before
/// another worker may claim the run. Must stay below cja's 120s job
/// `reclaim_window`.
pub const DEFAULT_LEASE_TAKEOVER: Duration = Duration::from_secs(90);

/// App state that carries the [`Durable`] engine (the cja-passkey
/// `HasPasskeyConfig` pattern for reaching crate state from an app's
/// `AppState`).
pub trait HasDurable: cja::app_state::AppState {
    /// The durable agent-run engine.
    fn durable(&self) -> &Durable;
}

/// Static agent recipe. `build` receives `kind.model` — the single source of
/// truth for the model id (row, wire call, and builder all use it).
///
/// Kinds are static: preamble, static tools, and static context documents
/// only. `dynamic_tools`/`dynamic_context` (RAG) and typed output
/// (`output_schema`) are unsupported on the durable path; [`start`](fn@start)
/// rejects a kind whose built agent carries an `output_schema`.
#[derive(Debug)]
pub struct AgentKind {
    /// The kind's registered name.
    pub name: &'static str,
    /// The model id, e.g. [`crate::models::DEFAULT`].
    pub model: &'static str,
    /// Per-run caps.
    pub limits: RunLimits,
    /// Policy for a model tool call no hook resolves (rig runner default:
    /// `Fail`). The `AgentBuilder` has no setter for this — rig exposes it
    /// only on the runner — so the durable engine applies it over the built
    /// agent's `run_spec()`.
    pub unhandled_invalid_tool_call: rig_agent::run::UnhandledInvalidToolCall,
    /// Build the rig agent (with its tools) for a run of this kind.
    pub build: fn(&Llm, &str) -> rig_agent::AgentBuilder,
}

/// Per-kind run caps. Defaults: 20 turns, 200 000 total tokens.
#[derive(Debug, Clone, Copy)]
pub struct RunLimits {
    /// Total model-call budget per run.
    pub max_turns: usize,
    /// Total token budget (input + output) per run.
    pub max_total_tokens: u64,
}

impl Default for RunLimits {
    fn default() -> Self {
        Self {
            max_turns: 20,
            max_total_tokens: 200_000,
        }
    }
}

/// The idempotency key handed to every tool execution: which run, and which
/// tool call. Provider call ids are unique only per conversation, so the run
/// id scopes them.
///
/// A *receipt*, not execution deduplication: exactly-once tools must persist
/// their own attempt record keyed by this value before side effects. A tool
/// whose result was not persisted re-executes on resume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallKey {
    /// The run the tool call belongs to.
    pub run_id: RunId,
    /// The provider's tool call id.
    pub call_id: String,
}

impl rig_core::tool::ContextValue for ToolCallKey {
    const KEY: &'static str = "durable_tool_call_key";
}

/// Everything the durable engine can go wrong with, for app code.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RunError {
    /// No registered kind carries this name.
    #[error("unknown agent kind `{0}`")]
    UnknownKind(String),
    /// The kind uses a feature the durable path does not support.
    #[error("agent kind uses an unsupported feature: {0}")]
    UnsupportedKind(&'static str),
    /// Two kinds registered the same name.
    #[error("duplicate agent kind `{0}`")]
    DuplicateKind(String),
    /// No run row carries this id.
    #[error("run {0} not found")]
    NotFound(RunId),
    /// [`answer`](fn@answer) on a run that is not suspended.
    #[error("run {0} is not waiting (status: {1:?})")]
    NotWaiting(RunId, RunStatus),
    /// The requested status transition is not one the lifecycle allows.
    #[error("run {run} cannot move {from} -> {to}")]
    InvalidTransition {
        /// The run.
        run: RunId,
        /// The run's current status.
        from: RunStatus,
        /// The rejected target status.
        to: RunStatus,
    },
    /// The stored run state names an ask no open call matches, or otherwise
    /// disagrees with the run: refuse rather than guess.
    ///
    /// Deliberately no `#[source]`: serde type-mismatch messages quote the
    /// offending value, and the run state is the whole conversation. Render
    /// only the serde classification plus line/column.
    #[error("stored run state failed to deserialize at line {1} column {2} ({0})")]
    Corrupt(&'static str, u32, u32),
    /// The database layer failed.
    #[error(transparent)]
    Storage(#[from] sqlx::Error),
}

/// A run row, read side. Decodes `status`/`fail_reason`/`pending_*` as TEXT
/// and parses them (`RunStatus::from_str`, [`FailReason::from_str`],
/// `PendingAsk`); an unknown value means corruption or forward-version skew
/// and fails the read with [`RunError::Storage`] — there is exactly one
/// parsing path.
#[derive(Debug, Clone)]
pub struct RunView {
    pub id: uuid::Uuid,
    pub kind: String,
    pub status: RunStatus,
    pub model: String,
    pub app: String,
    pub turns: i32,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub output: Option<String>,
    pub fail_reason: Option<FailReason>,
    pub error: Option<String>,
    pub pending_ask: Option<PendingAsk>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for RunView {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;

        fn decode_error(column: &str, message: String) -> sqlx::Error {
            sqlx::Error::ColumnDecode {
                index: column.to_owned(),
                source: message.into(),
            }
        }

        let status: String = row.try_get("status")?;
        let status = RunStatus::from_str(&status)
            .map_err(|_| decode_error("status", format!("unknown run status `{status}`")))?;
        let fail_reason: Option<String> = row.try_get("fail_reason")?;
        let fail_reason = fail_reason
            .map(|raw| {
                FailReason::from_str(&raw).map_err(|_| {
                    decode_error("fail_reason", format!("unknown fail reason `{raw}`"))
                })
            })
            .transpose()?;
        let pending_call_id: Option<String> = row.try_get("pending_call_id")?;
        let pending_question: Option<String> = row.try_get("pending_question")?;
        let pending_ask = match (pending_call_id, pending_question) {
            (Some(call_id), Some(question)) => Some(PendingAsk { call_id, question }),
            (None, None) => None,
            (call_id, _) => {
                return Err(decode_error(
                    "pending_call_id",
                    format!(
                        "waiting-ask columns disagree (pending_call_id is {call_id:?})"
                    ),
                ));
            }
        };

        Ok(Self {
            id: row.try_get("id")?,
            kind: row.try_get("kind")?,
            status,
            model: row.try_get("model")?,
            app: row.try_get("app")?,
            turns: row.try_get("turns")?,
            input_tokens: row.try_get("input_tokens")?,
            output_tokens: row.try_get("output_tokens")?,
            output: row.try_get("output")?,
            fail_reason,
            error: row.try_get("error")?,
            pending_ask,
            created_at: row.try_get("created_at")?,
            started_at: row.try_get("started_at")?,
            finished_at: row.try_get("finished_at")?,
        })
    }
}

/// One committed transcript message: rig's serde `Message` as stored JSONB.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RunMessage {
    /// The message's sequence number within the run.
    pub seq: i64,
    /// `system`, `user`, or `assistant`.
    pub role: String,
    /// The rig `Message` body.
    pub message: serde_json::Value,
    /// When the row was written.
    pub created_at: chrono::DateTime<chrono::Utc>,
}
