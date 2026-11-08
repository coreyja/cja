use std::{collections::HashMap, time::Duration};

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use tokio_util::sync::CancellationToken;

use crate::app_state::AppState as AS;

use super::registry::{CronRegistry, TickError, recoverable};

/// Observable summary of one tick, for tests. `Worker::run` discards it;
/// every failure inside a tick is logged, never propagated.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TickOutcome {
    /// The `crons` map read failed; this tick was skipped and no cron ran.
    pub(crate) map_read_failed: bool,
    /// Registered crons whose tick returned `Err` (logged, then skipped).
    pub(crate) failed: usize,
}

/// Worker that executes cron jobs on a schedule
///
/// The worker runs cron jobs based on their configured schedules. For cron expressions,
/// the timezone parameter determines when the cron expression is evaluated. For example,
/// a cron expression "0 9 * * *" (9 AM daily) will run at 9 AM in the configured timezone.
///
/// Interval-based jobs ignore the timezone and run based on elapsed time since last run.
pub struct Worker<AppState: AS> {
    id: uuid::Uuid,
    state: AppState,
    registry: CronRegistry<AppState>,
    pub(crate) started_at: DateTime<Utc>,
    timezone: Tz,
    sleep_duration: Duration,
}

impl<AppState: AS> Worker<AppState> {
    /// Create a new Worker with UTC as the default timezone
    pub fn new(state: AppState, registry: CronRegistry<AppState>) -> Self {
        Self::new_with_timezone(state, registry, chrono_tz::UTC, Duration::from_mins(1))
    }

    /// Create a new Worker with a specific timezone
    pub fn new_with_timezone(
        state: AppState,
        registry: CronRegistry<AppState>,
        timezone: Tz,
        sleep_duration: Duration,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4(),
            state,
            registry,
            started_at: Utc::now(),
            timezone,
            sleep_duration,
        }
    }

    /// Run the cron worker until the shutdown token is cancelled.
    ///
    /// The worker will execute cron jobs on their configured schedules until shutdown
    /// is requested. When the `shutdown_token` is cancelled:
    /// - The worker stops scheduling new cron job executions
    /// - Any currently running tick completes before shutdown
    /// - The sleep between ticks can be interrupted for faster shutdown
    ///
    /// Tick errors are logged, never propagated: a cron whose callback or
    /// claim fails does not end the loop, and the next tick runs normally.
    /// `run` therefore returns only on shutdown. A panicking callback still
    /// ends the process via the Supervisor — and, because the claim already
    /// committed, recurs every interval.
    ///
    /// # Arguments
    ///
    /// * `shutdown_token` - Cancellation token for graceful shutdown
    ///
    /// # Example
    ///
    /// ```ignore
    /// use tokio_util::sync::CancellationToken;
    ///
    /// let shutdown_token = CancellationToken::new();
    /// let worker_token = shutdown_token.clone();
    ///
    /// tokio::spawn(async move {
    ///     worker.run(worker_token).await;
    /// });
    ///
    /// // Later, trigger shutdown
    /// shutdown_token.cancel();
    /// ```
    pub async fn run(self, shutdown_token: CancellationToken) {
        tracing::debug!(cron_worker_id = %self.id, "Starting Cron loop");
        while !shutdown_token.is_cancelled() {
            // Dropping tick on cancellation can interrupt a direct callback
            // after its side effect but before last_run_at is persisted.
            // Finish the current tick, including its error or completion write.
            // A tick failure is logged inside `tick` and never ends the loop.
            self.tick().await;

            tokio::select! {
                () = tokio::time::sleep(self.sleep_duration) => {}
                () = shutdown_token.cancelled() => {
                    tracing::info!(cron_worker_id = %self.id, "Cron worker shutdown during sleep");
                    break;
                }
            }
        }

        tracing::info!(cron_worker_id = %self.id, "Cron worker shutdown complete");
    }

    async fn last_enqueue_map(&self) -> Result<HashMap<String, DateTime<Utc>>, sqlx::Error> {
        let last_runs = sqlx::query!("SELECT name, last_run_at FROM Crons")
            .fetch_all(self.state.db())
            .await?;

        let last_run_map: HashMap<String, DateTime<Utc>> = last_runs
            .iter()
            .map(|row| (row.name.clone(), row.last_run_at))
            .collect();

        Ok(last_run_map)
    }

    #[tracing::instrument(name = "cron.tick", level = "trace", skip_all, fields(cron_worker.id = %self.id))]
    pub(crate) async fn tick(&self) -> TickOutcome {
        let mut outcome = TickOutcome::default();
        let last_enqueue_map = match self.last_enqueue_map().await {
            Ok(map) => map,
            Err(error) => {
                outcome.map_read_failed = true;
                if recoverable(&error) {
                    tracing::warn!(
                        cron_worker.id = %self.id,
                        operation = "last_enqueue_map",
                        sqlstate = ?error.as_database_error().and_then(sqlx::error::DatabaseError::code),
                        cause = %error,
                        "Cron tick skipped"
                    );
                } else {
                    tracing::error!(
                        cron_worker.id = %self.id,
                        operation = "last_enqueue_map",
                        sqlstate = ?error.as_database_error().and_then(sqlx::error::DatabaseError::code),
                        cause = %error,
                        "Cron tick failed"
                    );
                }
                return outcome;
            }
        };
        for job in self.registry.jobs.values() {
            if let Err(error) = job
                .tick(
                    self.state.clone(),
                    &last_enqueue_map,
                    self.started_at,
                    self.timezone,
                )
                .await
            {
                outcome.failed += 1;
                let sqlstate = match &error {
                    TickError::SqlxError(err) => err
                        .as_database_error()
                        .and_then(sqlx::error::DatabaseError::code),
                    TickError::JobError(_) => None,
                };
                tracing::error!(
                    cron.name = job.name,
                    cron_worker.id = %self.id,
                    sqlstate = ?sqlstate,
                    cause = %error,
                    "Cron tick failed; continuing to next cron"
                );
            }
        }
        outcome
    }
}
