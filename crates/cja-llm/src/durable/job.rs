//! The generic advancement job.

use cja::jobs::{CancellationToken, Job};
use uuid::Uuid;

use super::HasDurable;

/// Advance one agent run. Payload is just the run id: kind, input, and model
/// are read from the row. Registered in an app's `impl_job_registry!` as the
/// plain struct — no type parameter.
///
/// A queued payload can survive a deploy: any field added later must land
/// with `#[serde(default)]`, or old rows fail to deserialize, retry
/// deterministically into dead-letter, and orphan their runs. Keep [`Self::NAME`]
/// stable — renaming it strands in-flight rows as permanent `Unknown job type`s.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AdvanceRunJob {
    /// The run to advance.
    pub run_id: Uuid,
}

impl AdvanceRunJob {
    /// The registered job name. Single source of truth; the `Job` impl uses
    /// it. An app test asserting `Jobs::job_names().contains(&AdvanceRunJob::NAME)`
    /// catches registration drift, which compiles fine and fails only at
    /// runtime.
    pub const NAME: &'static str = "AdvanceRunJob";
}

#[async_trait::async_trait]
impl<S: HasDurable> Job<S> for AdvanceRunJob {
    const NAME: &'static str = Self::NAME;

    async fn run(&self, state: S) -> cja::color_eyre::Result<()> {
        self.run_with_cancellation(state, CancellationToken::new())
            .await
    }

    /// `token` is the worker's shutdown token (`run_job` forwards it): what
    /// the worker's lease guards select on. Deploys abort the in-flight
    /// step and release the run for the post-deploy worker; they never mark
    /// the run cancelled or failed.
    async fn run_with_cancellation(
        &self,
        state: S,
        cancellation_token: CancellationToken,
    ) -> cja::color_eyre::Result<()> {
        state
            .durable()
            .advance_run(state.db(), self.run_id, cancellation_token)
            .await
    }
}
