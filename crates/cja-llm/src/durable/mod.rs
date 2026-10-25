//! Durable agent runs: rig's `AgentRun` persisted in Postgres, advanced by
//! cja background jobs.
//!
//! See the crate README's "Durable agent runs" section for the full model:
//! kinds, leases, ask/answer suspension, failure classification, and the
//! enqueue/wake contract.

pub(crate) mod db;

pub use db::run_migrations;

use std::fmt;
use std::str::FromStr;

/// The identity of one durable agent run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
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
/// `FromStr` in `RunView`'s manual `FromRow` — one parsing path — and is
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
