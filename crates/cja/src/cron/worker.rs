use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

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
    /// `run` therefore returns only on shutdown. A panicking callback is not
    /// caught: under the Supervisor it ends the process. Its claim has
    /// already committed, so the restart does not retry it immediately, but
    /// a callback that always panics ends the process again each interval.
    ///
    /// # Arguments
    ///
    /// * `shutdown_token` - Cancellation token for graceful shutdown
    ///
    /// # Example
    ///
    /// ```ignore
    /// use cja::tasks::{ShutdownBudget, Supervisor};
    /// use tokio_util::sync::CancellationToken;
    ///
    /// let shutdown = CancellationToken::new();
    /// let mut supervisor = Supervisor::new(ShutdownBudget::from_env())?;
    ///
    /// let token = shutdown.clone();
    /// supervisor.spawn("cron", async move { worker.run(token).await; Ok(()) });
    ///
    /// // Later, trigger shutdown
    /// shutdown.cancel();
    /// ```
    pub async fn run(self, shutdown_token: CancellationToken) {
        verify_cron_pauses_schema(self.state.db()).await;
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

    async fn last_enqueue_map(
        &self,
    ) -> Result<(HashMap<String, DateTime<Utc>>, DateTime<Utc>), sqlx::Error> {
        // Tick time first, then the row snapshot. A row committed after this
        // read can carry a stamp later than `db_now`; the pre-filter passes
        // such a stamp through as due and the claim re-check — with a fresh
        // clock_timestamp() — decides.
        let db_now = sqlx::query_scalar::<_, DateTime<Utc>>("SELECT clock_timestamp()")
            .fetch_one(self.state.db())
            .await?;
        // Runtime query (not `query!`): with `DATABASE_URL` set and
        // `SQLX_OFFLINE` unset, the macro live-checks the query against that
        // database at compile time. That is exactly the state the
        // scaffolded-app run instructions create, on a database that has no
        // `crons` table until boot-time migrations run — so a macro here
        // would break compiling a freshly scaffolded app.
        let last_runs =
            sqlx::query_as::<_, (String, DateTime<Utc>)>("SELECT name, last_run_at FROM crons")
                .fetch_all(self.state.db())
                .await?;

        let last_run_map: HashMap<String, DateTime<Utc>> = last_runs.into_iter().collect();

        Ok((last_run_map, db_now))
    }

    #[tracing::instrument(name = "cron.tick", level = "trace", skip_all, fields(cron_worker.id = %self.id))]
    pub(crate) async fn tick(&self) -> TickOutcome {
        let mut outcome = TickOutcome::default();
        // Monotonic anchor for this tick, taken before the database reads:
        // the map read below can block behind another transaction's table
        // lock, and callbacks are awaited inline after the reads — in both
        // cases wall-clock time passes that `db_now` does not know about.
        // Each job's pre-filter `now` is `db_now` advanced by the time
        // elapsed since this anchor, so a stale snapshot cannot produce a
        // pre-filter false negative that `claim_due` (only reached when the
        // pre-filter passes) never sees. The anchor precedes the time read,
        // so the advanced `now` can run marginally ahead of the database
        // clock; the authoritative re-check in `claim_due` absorbs that.
        let tick_started = Instant::now();
        let (last_enqueue_map, db_now) = match self.last_enqueue_map().await {
            Ok(pair) => pair,
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
            let now = db_now
                + chrono::Duration::from_std(tick_started.elapsed())
                    .unwrap_or_else(|_| chrono::Duration::zero());
            if let Err(error) = job
                .tick(
                    self.state.clone(),
                    &last_enqueue_map,
                    now,
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

/// One-shot schema probe at worker start, like `verify_jobs_schema`
/// (crates/cja/src/jobs/worker.rs) but log-only. The ERROR can be transient
/// if another instance is still migrating. If this app keeps its own copies
/// of cja's migrations and has not copied `AddCronPauses`, every due cron's
/// claim fails on every tick with `relation "cron_pauses" does not exist`.
/// Log one ERROR naming the fix at boot instead of leaving the per-tick
/// errors as the only signal. Log-only: `run` returns `()`.
async fn verify_cron_pauses_schema(db: &sqlx::PgPool) {
    let Err(error) = sqlx::query("SELECT name, paused_until, paused_at FROM cron_pauses LIMIT 0")
        .execute(db)
        .await
    else {
        return;
    };
    let mismatch = error
        .as_database_error()
        .and_then(sqlx::error::DatabaseError::code)
        .is_some_and(|code| matches!(code.as_ref(), "42P01" | "42703"));
    if mismatch {
        tracing::error!(
            %error,
            "The cron schema is missing cja migrations (cron_pauses). Cron claims will \
             fail every tick until it is applied. If this app keeps its own copies of \
             cja's migrations, copy the missing files (`cja sync-migrations`) and run \
             them; if you use `cja::db::run_migrations(&pool)` at boot, check which \
             migrator actually ran."
        );
    } else {
        tracing::warn!(
            %error,
            "Could not verify the cron_pauses schema at startup; continuing"
        );
    }
}
