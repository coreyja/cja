use std::{any::Any, collections::HashSet, future::Future, pin::Pin, sync::Mutex, time::Duration};

use futures::FutureExt;
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tracing::Span;

use crate::app_state::AppState as AS;

use super::registry::JobRegistry;

/// Default maximum number of retry attempts before a job is moved to the dead letter queue.
///
/// With exponential backoff (`2^(error_count+1)` seconds), 20 retries means:
/// - First retry after 2 seconds
/// - Last retry after ~6 days
/// - Total retry window of ~12 days
pub const DEFAULT_MAX_RETRIES: i32 = 20;

/// Default heartbeat interval for an active job.
pub const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
/// Default reclaim window after the last successful heartbeat.
pub const DEFAULT_RECLAIM_WINDOW: Duration = Duration::from_mins(2);

/// Settings for the sole job worker. The reclaim window must be at least three
/// heartbeat intervals; a larger window allows more transient DB failures.
#[derive(Clone, Copy, Debug)]
pub struct JobWorkerConfig {
    /// Time between heartbeat attempts while a job runs (30 seconds by default).
    pub heartbeat_interval: Duration,
    /// Time since the last successful heartbeat before another worker may
    /// reclaim and charge the attempt (120 seconds by default).
    pub reclaim_window: Duration,
    /// Time allowed for the running job body after cancellation. Defaults to
    /// `ShutdownBudget::from_env().job_drain` (2 seconds when unset).
    pub shutdown_drain_timeout: Duration,
}

impl Default for JobWorkerConfig {
    fn default() -> Self {
        Self {
            heartbeat_interval: DEFAULT_HEARTBEAT_INTERVAL,
            reclaim_window: DEFAULT_RECLAIM_WINDOW,
            shutdown_drain_timeout: crate::tasks::ShutdownBudget::from_env().job_drain,
        }
    }
}

impl JobWorkerConfig {
    fn validate(self) -> color_eyre::Result<i64> {
        use color_eyre::eyre::eyre;
        if self.heartbeat_interval.is_zero() {
            return Err(eyre!(
                "heartbeat interval must be positive: {:?}",
                self.heartbeat_interval
            ));
        }
        if self
            .heartbeat_interval
            .checked_mul(3)
            .is_none_or(|minimum| self.reclaim_window < minimum)
        {
            return Err(eyre!(
                "reclaim window {:?} must be at least three heartbeat intervals ({:?})",
                self.reclaim_window,
                self.heartbeat_interval
            ));
        }
        let heartbeat = i64::try_from(self.heartbeat_interval.as_micros())
            .map_err(|_| eyre!("heartbeat interval exceeds PostgreSQL microsecond range"))?;
        let window = i64::try_from(self.reclaim_window.as_micros())
            .map_err(|_| eyre!("reclaim window exceeds PostgreSQL microsecond range"))?;
        if heartbeat == 0 || window == 0 || self.heartbeat_interval / 3 == Duration::ZERO {
            return Err(eyre!(
                "heartbeat interval must allow a positive query timeout"
            ));
        }
        Ok(window)
    }
}

/// Initial backoff duration after a worker tick fails (e.g. the database was
/// unreachable while trying to claim a job). Doubles on each consecutive
/// failure, capped at [`TICK_ERROR_BACKOFF_MAX`], and resets after a
/// successful tick.
const TICK_ERROR_BACKOFF_BASE: Duration = Duration::from_secs(1);

/// Maximum backoff duration between retries after consecutive tick failures.
const TICK_ERROR_BACKOFF_MAX: Duration = Duration::from_secs(30);

pub(super) type RunJobResult = Result<RunJobSuccess, JobError>;

#[derive(Debug)]
pub(super) struct RunJobSuccess(JobFromDB);

enum RunJobOutcome {
    Completed(RunJobResult),
    LeaseLost(uuid::Uuid),
}

struct ClaimedJob {
    job: JobFromDB,
    claim_sent_at: tokio::time::Instant,
    previous_locked_by: Option<String>,
    previous_error_count: i32,
}

struct LeaseTracker {
    last_refresh_sent: tokio::time::Instant,
    config: JobWorkerConfig,
}

impl LeaseTracker {
    fn deadline(&self) -> tokio::time::Instant {
        self.last_refresh_sent + self.config.reclaim_window - self.config.heartbeat_interval / 2
    }
}

#[derive(Debug, sqlx::FromRow)]
pub struct JobFromDB {
    pub job_id: uuid::Uuid,
    pub name: String,
    pub payload: serde_json::Value,
    pub priority: i32,
    pub run_at: chrono::DateTime<chrono::Utc>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub context: String,
    pub error_count: i32,
    pub last_error_message: Option<String>,
    pub last_failed_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Error)]
#[error("JobError(id:${}) ${1}", self.0.job_id)]
pub(crate) struct JobError(JobFromDB, color_eyre::Report);

struct Worker<AppState: AS, R: JobRegistry<AppState>> {
    id: uuid::Uuid,
    // Cached SQL-bind form of id; jobs.locked_by stores this exact UUID string.
    owner_token: String,
    state: AppState,
    registry: R,
    sleep_duration: Duration,
    max_retries: i32,
    cancellation_token: CancellationToken,
    config: JobWorkerConfig,
    reclaim_window_micros: i64,
    // A watchdog-abandoned row must expire and be charged, even if this worker
    // subsequently shuts down gracefully.
    abandoned_jobs: Mutex<HashSet<uuid::Uuid>>,
    #[cfg(test)]
    fail_heartbeats: std::sync::atomic::AtomicU32,
}

impl<AppState: AS, R: JobRegistry<AppState>> Worker<AppState, R> {
    fn new(
        state: AppState,
        registry: R,
        sleep_duration: Duration,
        max_retries: i32,
        cancellation_token: CancellationToken,
        config: JobWorkerConfig,
    ) -> color_eyre::Result<Self> {
        let reclaim_window_micros = config.validate()?;
        let id = uuid::Uuid::new_v4();
        Ok(Self {
            id,
            owner_token: id.to_string(),
            state,
            registry,
            sleep_duration,
            max_retries,
            cancellation_token,
            config,
            reclaim_window_micros,
            abandoned_jobs: Mutex::new(HashSet::new()),
            #[cfg(test)]
            fail_heartbeats: std::sync::atomic::AtomicU32::new(0),
        })
    }

    #[tracing::instrument(
        name = "worker.run_job",
        skip(self, job),
        fields(job.id = %job.job_id, job.name = job.name, worker.id = %self.id),
        err,
    )]
    async fn run_job(&self, job: &JobFromDB) -> color_eyre::Result<()> {
        let result = std::panic::AssertUnwindSafe(self.registry.run_job(
            job,
            self.state.clone(),
            self.cancellation_token.clone(),
        ))
        .catch_unwind()
        .await;
        match result {
            Ok(result) => result,
            Err(payload) => Err(color_eyre::eyre::eyre!(
                "Job panicked: {}",
                panic_message(payload.as_ref())
            )),
        }
    }

    #[allow(clippy::too_many_lines)] // scoped heartbeat race and ownership-safe finalization belong together
    async fn run_next_job(&self, claimed: ClaimedJob) -> color_eyre::Result<RunJobOutcome> {
        let job = claimed.job;
        let mut tracker = LeaseTracker {
            last_refresh_sent: claimed.claim_sent_at,
            config: self.config,
        };
        if tokio::time::Instant::now() >= tracker.deadline() {
            tracing::warn!(job_id = %job.job_id, "Claim response exceeded lease deadline");
            self.abandoned_jobs.lock().unwrap().insert(job.job_id);
            return Ok(RunJobOutcome::LeaseLost(job.job_id));
        }

        let job_result = {
            let mut body = std::pin::pin!(self.run_job(&job));
            let mut next_tick = claimed.claim_sent_at + self.config.heartbeat_interval;
            type HeartbeatResult = Result<
                Result<sqlx::postgres::PgQueryResult, sqlx::Error>,
                tokio::time::error::Elapsed,
            >;
            let mut heartbeat: Option<Pin<Box<dyn Future<Output = HeartbeatResult> + Send + '_>>> =
                None;
            let mut heartbeat_sent_at = claimed.claim_sent_at;
            loop {
                let deadline = tracker.deadline();
                tokio::select! {
                    biased;
                    result = &mut body => break result,
                    () = tokio::time::sleep_until(deadline) => {
                        tracing::warn!(job_id = %job.job_id, "Job lease watchdog expired");
                        self.abandoned_jobs.lock().unwrap().insert(job.job_id);
                        return Ok(RunJobOutcome::LeaseLost(job.job_id));
                    }
                    result = async { heartbeat.as_mut().expect("guarded heartbeat").await }, if heartbeat.is_some() => {
                        heartbeat = None;
                        match result {
                            Ok(Ok(result)) if result.rows_affected() == 1 => {
                                tracker.last_refresh_sent = heartbeat_sent_at;
                            }
                            Ok(Ok(_)) => {
                                tracing::warn!(job_id = %job.job_id, owner = %self.owner_token, "Job lease ownership lost");
                                return Ok(RunJobOutcome::LeaseLost(job.job_id));
                            }
                            Ok(Err(error)) => tracing::warn!(job_id = %job.job_id, %error, "Job heartbeat failed"),
                            Err(_) => tracing::warn!(job_id = %job.job_id, "Job heartbeat timed out"),
                        }
                    }
                    () = tokio::time::sleep_until(next_tick), if heartbeat.is_none() => {
                        let sent_at = tokio::time::Instant::now();
                        next_tick = sent_at + self.config.heartbeat_interval;
                        #[cfg(test)]
                        if self.fail_heartbeats.fetch_update(
                            std::sync::atomic::Ordering::SeqCst,
                            std::sync::atomic::Ordering::SeqCst,
                            |n| n.checked_sub(1),
                        ).is_ok() {
                            tracing::warn!(job_id = %job.job_id, "Synthetic heartbeat failure");
                            continue;
                        }
                        let update = sqlx::query(
                            "UPDATE jobs SET locked_at = NOW() WHERE job_id = $1 AND locked_by = $2"
                        )
                        .bind(job.job_id)
                        .bind(&self.owner_token)
                        .execute(self.state.db());
                        heartbeat_sent_at = sent_at;
                        heartbeat = Some(Box::pin(tokio::time::timeout(self.config.heartbeat_interval / 3, update)));
                    }
                }
            }
        };
        // The body and all heartbeat futures have ended before finalization.
        if let Err(error) = job_result {
            let error_message = format!("{error:#}");
            if job.error_count >= self.max_retries {
                let count = job
                    .error_count
                    .checked_add(1)
                    .ok_or_else(|| color_eyre::eyre::eyre!("job error_count overflow"))?;
                if !self.dead_letter_owned(&job, count, &error_message).await? {
                    return Ok(RunJobOutcome::LeaseLost(job.job_id));
                }
                tracing::error!(job_id = %job.job_id, error_count = count, error = %error_message,
                    "Job permanently failed - moved to dead letter queue");
            } else {
                let result = sqlx::query(
                    "UPDATE jobs SET locked_by = NULL, locked_at = NULL,
                     error_count = error_count + 1, last_error_message = $3,
                     last_failed_at = NOW(),
                     run_at = NOW() + POWER(2, error_count + 1) * interval '1 second'
                     WHERE job_id = $1 AND locked_by = $2 AND error_count < 2147483647",
                )
                .bind(job.job_id)
                .bind(&self.owner_token)
                .bind(&error_message)
                .execute(self.state.db())
                .await?;
                if result.rows_affected() != 1 {
                    tracing::warn!(job_id = %job.job_id, "Job failure finalization lost ownership");
                    return Ok(RunJobOutcome::LeaseLost(job.job_id));
                }
                tracing::warn!(job_id = %job.job_id, retry = job.error_count + 1, error = %error_message,
                    "Job failed, retry scheduled");
            }
            return Ok(RunJobOutcome::Completed(Err(JobError(job, error))));
        }

        let result = sqlx::query("DELETE FROM jobs WHERE job_id = $1 AND locked_by = $2")
            .bind(job.job_id)
            .bind(&self.owner_token)
            .execute(self.state.db())
            .await?;
        if result.rows_affected() != 1 {
            tracing::warn!(job_id = %job.job_id, "Job completion lost ownership");
            return Ok(RunJobOutcome::LeaseLost(job.job_id));
        }
        Ok(RunJobOutcome::Completed(Ok(RunJobSuccess(job))))
    }

    async fn dead_letter_owned(
        &self,
        job: &JobFromDB,
        count: i32,
        message: &str,
    ) -> color_eyre::Result<bool> {
        let mut tx = self.state.db().begin().await?;
        let owned: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM jobs WHERE job_id = $1 AND locked_by = $2 FOR UPDATE)",
        )
        .bind(job.job_id)
        .bind(&self.owner_token)
        .fetch_one(&mut *tx)
        .await?;
        if !owned {
            tracing::warn!(job_id = %job.job_id, "Dead-letter finalization lost ownership");
            return Ok(false);
        }
        sqlx::query(
            "INSERT INTO dead_letter_jobs (original_job_id, name, payload, context, priority,
              error_count, last_error_message, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(job.job_id)
        .bind(&job.name)
        .bind(&job.payload)
        .bind(&job.context)
        .bind(job.priority)
        .bind(count)
        .bind(message)
        .bind(job.created_at)
        .execute(&mut *tx)
        .await?;
        let deleted = sqlx::query("DELETE FROM jobs WHERE job_id = $1 AND locked_by = $2")
            .bind(job.job_id)
            .bind(&self.owner_token)
            .execute(&mut *tx)
            .await?;
        if deleted.rows_affected() != 1 {
            return Err(color_eyre::eyre::eyre!(
                "owned dead-letter delete affected no rows"
            ));
        }
        tx.commit().await?;
        Ok(true)
    }

    #[tracing::instrument(name = "worker.fetch_next_job", level = "trace", skip(self),
        fields(worker.id = %self.id, job.id = tracing::field::Empty,
            job.name = tracing::field::Empty, reclaim_window_secs = self.config.reclaim_window.as_secs()), err)]
    async fn fetch_next_job(&self) -> color_eyre::Result<Option<ClaimedJob>> {
        use sqlx::Row;
        let claim_sent_at = tokio::time::Instant::now();
        let row = sqlx::query(
            "WITH candidate AS MATERIALIZED (
                SELECT job_id, locked_by AS previous_locked_by,
                       error_count AS previous_error_count
                FROM jobs
                WHERE run_at <= NOW()
                  AND (locked_by IS NULL OR locked_at < NOW() - $2::bigint * interval '1 microsecond')
                  AND (locked_by IS NULL OR error_count < 2147483647)
                ORDER BY priority DESC, run_at ASC, created_at ASC
                LIMIT 1 FOR UPDATE SKIP LOCKED
             ), updated AS (
                UPDATE jobs AS j
                SET locked_by = $1, locked_at = NOW(),
                    error_count = j.error_count + CASE WHEN c.previous_locked_by IS NULL THEN 0 ELSE 1 END,
                    last_error_message = CASE WHEN c.previous_locked_by IS NULL THEN j.last_error_message
                      ELSE 'Job lease expired (previous worker: ' || c.previous_locked_by || ')' END,
                    last_failed_at = CASE WHEN c.previous_locked_by IS NULL THEN j.last_failed_at ELSE NOW() END
                FROM candidate AS c WHERE j.job_id = c.job_id
                RETURNING j.job_id, j.name, j.payload, j.priority, j.run_at, j.created_at,
                          j.context, j.error_count, j.last_error_message, j.last_failed_at
             )
             SELECT u.*, c.previous_locked_by, c.previous_error_count FROM updated u
             JOIN candidate c ON u.job_id = c.job_id"
        )
        .bind(&self.owner_token)
        .bind(self.reclaim_window_micros)
        .fetch_optional(self.state.db())
        .await?;
        let Some(row) = row else { return Ok(None) };
        let claimed = ClaimedJob {
            job: JobFromDB {
                job_id: row.try_get("job_id")?,
                name: row.try_get("name")?,
                payload: row.try_get("payload")?,
                priority: row.try_get("priority")?,
                run_at: row.try_get("run_at")?,
                created_at: row.try_get("created_at")?,
                context: row.try_get("context")?,
                error_count: row.try_get("error_count")?,
                last_error_message: row.try_get("last_error_message")?,
                last_failed_at: row.try_get("last_failed_at")?,
            },
            claim_sent_at,
            previous_locked_by: row.try_get("previous_locked_by")?,
            previous_error_count: row.try_get("previous_error_count")?,
        };
        let span = Span::current();
        span.record("job.id", claimed.job.job_id.to_string());
        span.record("job.name", &claimed.job.name);
        self.abandoned_jobs
            .lock()
            .unwrap()
            .remove(&claimed.job.job_id);
        if claimed.previous_locked_by.is_some() && claimed.previous_error_count >= self.max_retries
        {
            let message = claimed
                .job
                .last_error_message
                .as_deref()
                .unwrap_or("Job lease expired");
            if self
                .dead_letter_owned(&claimed.job, claimed.job.error_count, message)
                .await?
            {
                tracing::error!(job_id = %claimed.job.job_id,
                    previous_owner = ?claimed.previous_locked_by,
                    error_count = claimed.job.error_count,
                    "Job lease expired - moved to dead letter queue");
            }
            return Ok(None);
        }
        Ok(Some(claimed))
    }
}

fn panic_message(payload: &(dyn Any + Send)) -> &str {
    if let Some(message) = payload.downcast_ref::<String>() {
        message
    } else if let Some(message) = payload.downcast_ref::<&str>() {
        message
    } else {
        "non-string payload"
    }
}

/// Confirm the database has the schema this worker's SQL relies on.
///
/// The worker's queries are not checked at compile time against the app's
/// database, so an app that is missing cja migrations builds and boots
/// normally, then fails every `fetch_next_job` forever while cron keeps
/// enqueueing. Catching that here turns a silently dead worker into a failed
/// start.
///
/// Only a definite schema mismatch is an error. Anything else (database
/// unreachable, pool timeout) is left to the worker loop, which already
/// retries transient failures with backoff.
async fn verify_jobs_schema(db: &sqlx::PgPool) -> color_eyre::Result<()> {
    const UNDEFINED_COLUMN: &str = "42703";
    const UNDEFINED_TABLE: &str = "42P01";

    let checks = [
        "SELECT job_id, name, payload, priority, run_at, created_at, context, locked_by, \
         locked_at, error_count, last_error_message, last_failed_at FROM jobs LIMIT 0",
        "SELECT original_job_id, name, payload, context, priority, error_count, \
         last_error_message, created_at FROM dead_letter_jobs LIMIT 0",
    ];

    for check in checks {
        let Err(error) = sqlx::query(check).execute(db).await else {
            continue;
        };
        let schema_mismatch = error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::code)
            .is_some_and(|code| code == UNDEFINED_COLUMN || code == UNDEFINED_TABLE);
        if schema_mismatch {
            return Err(color_eyre::eyre::eyre!(
                "The jobs schema is missing cja migrations ({error}). The job worker cannot \
                 claim jobs until they are applied: copy the missing files from cja's \
                 `migrations/` directory (`cja sync-migrations`) and run them."
            ));
        }
        tracing::warn!(
            %error,
            "Could not verify the jobs schema at startup; continuing, the worker loop will retry"
        );
        return Ok(());
    }

    Ok(())
}

/// Release database locks held by a worker.
///
/// This should be called during graceful shutdown to immediately release any job locks
/// held by this worker, rather than waiting for lease expiry. Watchdog-abandoned
/// rows stay locked so a later claim records their failed attempt.
async fn cleanup_worker_locks<AppState: AS, R: JobRegistry<AppState>>(
    worker: &Worker<AppState, R>,
) -> color_eyre::Result<()> {
    tracing::info!(worker_id = %worker.id, "Releasing database locks");
    let abandoned: Vec<_> = worker
        .abandoned_jobs
        .lock()
        .unwrap()
        .iter()
        .copied()
        .collect();

    let result = sqlx::query(
        "UPDATE jobs
         SET locked_by = NULL, locked_at = NULL
         WHERE locked_by = $1 AND NOT (job_id = ANY($2::uuid[]))",
    )
    .bind(&worker.owner_token)
    .bind(&abandoned)
    .execute(worker.state.db())
    .await?;

    tracing::info!(
        worker_id = %worker.id,
        locks_released = result.rows_affected(),
        "Database locks released"
    );

    Ok(())
}

/// Start a job worker that processes jobs from the queue.
///
/// The worker will continuously poll for jobs and execute them using the provided registry.
/// Jobs are executed with automatic retry logic on failure.
///
/// # Arguments
///
/// * `app_state` - The application state containing database connection and configuration
/// * `registry` - The job registry that maps job names to their implementations
/// * `sleep_duration` - How long to sleep when no jobs are available
/// * `max_retries` - Maximum number of times to retry a failed job before moving to the dead letter queue (default: 20)
/// * `shutdown_token` - Cancellation token for graceful shutdown. When cancelled, the worker
///   will stop accepting new jobs and release database locks before exiting.
/// * `config` - Heartbeat interval, reclaim window, and local shutdown drain budget
///
/// # Retry Behavior
///
/// When a job fails:
/// - The error count is incremented
/// - The error message and timestamp are recorded
/// - The job is requeued with exponential backoff: delay = `2^(error_count + 1)` seconds
///   (first retry: 2s, second: 4s, third: 8s, fourth: 16s, etc.)
/// - If `error_count` >= `max_retries`, the job is moved to the dead letter queue
///
/// # Graceful Shutdown
///
/// When the `shutdown_token` is cancelled:
/// - The worker stops polling for new jobs
/// - Any currently executing job gets a bounded drain to complete
/// - Remaining owned locks are released after the drain without charging an attempt
///
/// # Transient Error Resilience
///
/// Errors from the worker loop itself (e.g. the database being unreachable while
/// trying to claim the next job, such as a pool acquire timeout during a brief
/// database stall) are NOT fatal. They are logged at ERROR level and the worker
/// retries with exponential backoff (starting at 1 second, doubling up to a
/// 30 second cap, resetting after the next successful tick). This keeps a
/// transient database hiccup from killing the worker task — and with it, apps
/// that join on all worker tasks. Job execution failures are unaffected by this:
/// they continue to use the per-job retry machinery described above.
///
/// # Schema Check
///
/// Before polling, the worker confirms the `jobs` and `dead_letter_jobs`
/// tables have the columns its queries use, and returns an error if the
/// database is missing cja migrations. Without this an out-of-date schema
/// boots fine and then fails every claim silently. A database that is merely
/// unreachable at startup is not an error; see transient error resilience
/// above.
///
/// # Lease Timing
///
/// If a worker crashes or becomes unresponsive while processing a job, the job will remain
/// locked in the database. `JobWorkerConfig` defaults to a 30-second heartbeat,
/// a 120-second reclaim window, and the shutdown budget's job drain (2 seconds
/// when unset). An expired lock is charged as a failed attempt.
///
/// # Example
///
/// ```ignore
/// use std::time::Duration;
/// use tokio_util::sync::CancellationToken;
///
/// let shutdown_token = CancellationToken::new();
/// let worker_token = shutdown_token.clone();
///
/// // Start worker with the default heartbeat, reclaim, and drain settings
/// tokio::spawn(async move {
///     cja::jobs::worker::job_worker(
///         app_state,
///         registry,
///         Duration::from_secs(60),      // poll every 60s when idle
///         20,                            // max 20 retries
///         worker_token,                  // for graceful shutdown
///         Default::default(),
///     ).await.unwrap();
/// });
///
/// // Later, trigger shutdown
/// shutdown_token.cancel();
/// ```
pub async fn job_worker<AppState: AS>(
    app_state: AppState,
    registry: impl JobRegistry<AppState>,
    sleep_duration: Duration,
    max_retries: i32,
    shutdown_token: CancellationToken,
    config: JobWorkerConfig,
) -> color_eyre::Result<()> {
    if max_retries < 0 {
        return Err(color_eyre::eyre::eyre!("max_retries must be nonnegative"));
    }
    let worker = Worker::new(
        app_state,
        registry,
        sleep_duration,
        max_retries,
        shutdown_token.clone(),
        config,
    )?;

    verify_jobs_schema(worker.state.db()).await?;

    let mut tick_error_backoff = TICK_ERROR_BACKOFF_BASE;

    loop {
        let fetched = tokio::select! {
            result = worker.fetch_next_job() => result,
            () = shutdown_token.cancelled() => break,
        };
        let result = match fetched {
            Ok(Some(job)) => {
                let mut running = std::pin::pin!(worker.run_next_job(job));
                let run_result = tokio::select! {
                    result = &mut running => result,
                    () = shutdown_token.cancelled() => {
                        match tokio::time::timeout(worker.config.shutdown_drain_timeout, &mut running).await {
                            Ok(result) => result,
                            Err(_) => break,
                        }
                    }
                };
                // Fold the infrastructure error into `result` instead of
                // `?`-ing out: a transient failure while running/finalizing a
                // job (e.g. a DB blip during completion bookkeeping) must hit
                // the same backoff-and-retry arm as a fetch failure — never
                // kill the worker, which would take down apps that join on
                // all worker tasks.
                match run_result {
                    Ok(RunJobOutcome::Completed(Ok(RunJobSuccess(job)))) => {
                        tracing::info!(worker.id = %worker.id, job_id = %job.job_id, "Job Ran");
                        Ok(())
                    }
                    Ok(RunJobOutcome::Completed(Err(job_error))) => {
                        tracing::error!(worker.id = %worker.id, job_id = %job_error.0.job_id, error_count = %job_error.0.error_count, error_msg = %job_error.1, "Job Errored");
                        Ok(())
                    }
                    Ok(RunJobOutcome::LeaseLost(job_id)) => {
                        tracing::warn!(worker.id = %worker.id, %job_id, "Job lease lost; body stopped");
                        Ok(())
                    }
                    Err(error) => Err(error),
                }
            }
            Ok(None) => tokio::select! {
                () = tokio::time::sleep(worker.sleep_duration) => Ok(()),
                () = shutdown_token.cancelled() => break,
            },
            Err(error) => Err(error),
        };
        match result {
            Ok(()) => {
                tick_error_backoff = TICK_ERROR_BACKOFF_BASE;
            }
            Err(error) => {
                // A tick error here is a transient infrastructure failure
                // (e.g. couldn't reach the database to claim a job), not a
                // job failure — jobs have their own retry machinery. Log it
                // and retry with backoff instead of killing the worker,
                // which would take down apps that join on all worker tasks.
                tracing::error!(
                    worker_id = %worker.id,
                    error = %format!("{error:#}"),
                    backoff_secs = tick_error_backoff.as_secs(),
                    "Worker tick failed; backing off before retrying"
                );

                tokio::select! {
                    () = tokio::time::sleep(tick_error_backoff) => {}
                    () = shutdown_token.cancelled() => {
                        tracing::info!(worker_id = %worker.id, "Job worker shutdown requested");
                        break;
                    }
                }

                tick_error_backoff = (tick_error_backoff * 2).min(TICK_ERROR_BACKOFF_MAX);
            }
        }
    }

    cleanup_worker_locks(&worker).await?;
    tracing::info!(worker_id = %worker.id, "Job worker shutdown complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_state::AppState;
    use crate::impl_job_registry;
    use crate::jobs::Job;
    use crate::server::cookies::CookieKey;

    #[derive(Clone)]
    struct TestAppState {
        db: sqlx::PgPool,
        cookie_key: CookieKey,
    }

    impl AppState for TestAppState {
        fn db(&self) -> &sqlx::PgPool {
            &self.db
        }

        fn version(&self) -> &'static str {
            "test"
        }

        fn cookie_key(&self) -> &CookieKey {
            &self.cookie_key
        }
    }

    #[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
    struct TestJob {
        id: String,
    }

    #[async_trait::async_trait]
    impl Job<TestAppState> for TestJob {
        const NAME: &'static str = "TestJob";

        async fn run(&self, _app_state: TestAppState) -> color_eyre::Result<()> {
            Ok(())
        }
    }

    #[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
    struct PanicJob;

    #[async_trait::async_trait]
    impl Job<TestAppState> for PanicJob {
        const NAME: &'static str = "PanicJob";

        async fn run(&self, app_state: TestAppState) -> color_eyre::Result<()> {
            sqlx::query("UPDATE jobs SET context = 'started' WHERE name = $1")
                .bind(Self::NAME)
                .execute(app_state.db())
                .await?;
            panic!("fixture panic payload");
        }
    }

    #[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
    struct NormalProbeJob;

    #[async_trait::async_trait]
    impl Job<TestAppState> for NormalProbeJob {
        const NAME: &'static str = "NormalProbeJob";

        async fn run(&self, app_state: TestAppState) -> color_eyre::Result<()> {
            sqlx::query("INSERT INTO job_probe (marker) VALUES ('ran')")
                .execute(app_state.db())
                .await?;
            Ok(())
        }
    }

    #[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
    struct LongJob;

    #[async_trait::async_trait]
    impl Job<TestAppState> for LongJob {
        const NAME: &'static str = "LongJob";

        async fn run(&self, app_state: TestAppState) -> color_eyre::Result<()> {
            sqlx::query("UPDATE jobs SET context = 'started' WHERE name = $1")
                .bind(Self::NAME)
                .execute(app_state.db())
                .await?;
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(())
        }
    }

    /// A job that treats shutdown as work to finish: on cancellation it
    /// performs a durable database write, THEN returns Ok. Used to prove the
    /// bounded drain lets a cooperative job complete its cleanup instead of
    /// being dropped mid-flight.
    #[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
    struct DrainCooperativeJob;

    #[async_trait::async_trait]
    impl Job<TestAppState> for DrainCooperativeJob {
        const NAME: &'static str = "DrainCooperativeJob";

        async fn run(&self, _app_state: TestAppState) -> color_eyre::Result<()> {
            Ok(())
        }

        async fn run_with_cancellation(
            &self,
            app_state: TestAppState,
            cancellation_token: CancellationToken,
        ) -> color_eyre::Result<()> {
            // Announce that the job body is running. A locked row only proves
            // the claim committed, not that the worker has the job yet.
            sqlx::query("UPDATE jobs SET context = 'started' WHERE name = $1")
                .bind(Self::NAME)
                .execute(app_state.db())
                .await?;
            // Park until shutdown is requested, then do the durable cleanup
            // that a dropped future would never get to run.
            cancellation_token.cancelled().await;
            sqlx::query("UPDATE jobs SET context = 'cooperatively-drained' WHERE name = $1")
                .bind(Self::NAME)
                .execute(app_state.db())
                .await?;
            Ok(())
        }
    }

    /// A job that ignores cancellation entirely and sleeps far longer than any
    /// drain. Used to prove the bounded drain expires and drops the future.
    #[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
    struct DrainStubbornJob;

    #[async_trait::async_trait]
    impl Job<TestAppState> for DrainStubbornJob {
        const NAME: &'static str = "DrainStubbornJob";

        async fn run(&self, app_state: TestAppState) -> color_eyre::Result<()> {
            sqlx::query("UPDATE jobs SET context = 'started' WHERE name = $1")
                .bind(Self::NAME)
                .execute(app_state.db())
                .await?;
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(())
        }
    }

    impl_job_registry!(
        TestAppState,
        TestJob,
        PanicJob,
        NormalProbeJob,
        LongJob,
        DrainCooperativeJob,
        DrainStubbornJob
    );

    fn test_state(db: sqlx::PgPool) -> TestAppState {
        TestAppState {
            db,
            cookie_key: CookieKey::generate(),
        }
    }

    #[test]
    fn test_worker_config_defaults_and_validation() {
        let config = JobWorkerConfig::default();
        assert_eq!(config.heartbeat_interval, Duration::from_secs(30));
        assert_eq!(config.reclaim_window, Duration::from_mins(2));
        assert_eq!(
            config.shutdown_drain_timeout,
            crate::tasks::ShutdownBudget::from_env().job_drain
        );
        assert_eq!(config.validate().unwrap(), 120_000_000);

        let invalid = JobWorkerConfig {
            heartbeat_interval: Duration::ZERO,
            ..config
        };
        assert!(format!("{:#}", invalid.validate().unwrap_err()).contains("positive"));

        let invalid = JobWorkerConfig {
            heartbeat_interval: Duration::from_secs(2),
            reclaim_window: Duration::from_secs(5),
            ..config
        };
        let message = format!("{:#}", invalid.validate().unwrap_err());
        assert!(
            message.contains("2s") && message.contains("5s"),
            "{message}"
        );

        let invalid = JobWorkerConfig {
            heartbeat_interval: Duration::MAX,
            reclaim_window: Duration::MAX,
            ..config
        };
        assert!(format!("{:#}", invalid.validate().unwrap_err()).contains("three"));

        let beyond_pg = Duration::from_micros(i64::MAX as u64 + 1);
        let invalid = JobWorkerConfig {
            heartbeat_interval: beyond_pg,
            reclaim_window: beyond_pg * 3,
            ..config
        };
        assert!(
            format!("{:#}", invalid.validate().unwrap_err()).contains("heartbeat interval exceeds")
        );

        let invalid = JobWorkerConfig {
            reclaim_window: beyond_pg,
            ..config
        };
        assert!(
            format!("{:#}", invalid.validate().unwrap_err()).contains("reclaim window exceeds")
        );

        let invalid = JobWorkerConfig {
            heartbeat_interval: Duration::from_nanos(2),
            reclaim_window: Duration::from_nanos(6),
            ..config
        };
        assert!(
            format!("{:#}", invalid.validate().unwrap_err()).contains("positive query timeout")
        );

        let tiny_valid = JobWorkerConfig {
            heartbeat_interval: Duration::from_micros(3),
            reclaim_window: Duration::from_micros(9),
            ..config
        };
        assert_eq!(tiny_valid.validate().unwrap(), 9);
    }

    #[sqlx::test]
    async fn test_invalid_worker_config_fails_before_schema_access(db: sqlx::PgPool) {
        db.close().await;
        let error = job_worker(
            test_state(db),
            Jobs,
            Duration::from_millis(20),
            20,
            CancellationToken::new(),
            JobWorkerConfig {
                heartbeat_interval: Duration::from_secs(2),
                reclaim_window: Duration::from_secs(5),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("reclaim window"));
    }

    async fn wait_for<T, F, Fut>(mut check: F) -> T
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Option<T>>,
    {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(value) = check().await {
                    return value;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("expected durable job state within 10 seconds")
    }

    /// Wait until the job body is executing. Cancelling as soon as the row is
    /// locked races the worker: the claim can be committed while the worker is
    /// still awaiting the response, and a cancel in that window correctly
    /// drops the fetch and releases the lock without ever running the job.
    async fn wait_until_started(db: &sqlx::PgPool, name: &str) {
        for _ in 0..200 {
            let context: Option<String> =
                sqlx::query_scalar("SELECT context FROM jobs WHERE name = $1")
                    .bind(name)
                    .fetch_optional(db)
                    .await
                    .unwrap();
            if context.as_deref() == Some("started") {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("job {name} never started running");
    }

    #[sqlx::test]
    async fn test_panicking_job_is_charged_and_worker_continues(db: sqlx::PgPool) {
        sqlx::query("CREATE TABLE job_probe (marker text NOT NULL)")
            .execute(&db)
            .await
            .unwrap();
        let state = test_state(db.clone());
        PanicJob
            .enqueue(state.clone(), "panic".into(), None)
            .await
            .unwrap();
        let token = CancellationToken::new();
        let handle = tokio::spawn(job_worker(
            state.clone(),
            Jobs,
            Duration::from_millis(20),
            20,
            token.clone(),
            JobWorkerConfig {
                heartbeat_interval: Duration::from_millis(300),
                reclaim_window: Duration::from_millis(1200),
                shutdown_drain_timeout: Duration::ZERO,
            },
        ));
        wait_for(|| async {
            let row: Option<(i32, Option<String>, Option<String>)> = sqlx::query_as(
                "SELECT error_count, locked_by, last_error_message FROM jobs WHERE name = 'PanicJob'"
            ).fetch_optional(&db).await.unwrap();
            row.filter(|(count, owner, message)| *count == 1 && owner.is_none() && message.as_deref().is_some_and(|m| m.contains("fixture panic payload")))
        }).await;
        assert!(
            !handle.is_finished(),
            "handler panic must not stop the worker"
        );
        NormalProbeJob
            .enqueue(state, "normal".into(), None)
            .await
            .unwrap();
        wait_for(|| async {
            let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM job_probe")
                .fetch_one(&db)
                .await
                .unwrap();
            (count == 1).then_some(())
        })
        .await;
        token.cancel();
        handle.await.unwrap().unwrap();
    }

    #[sqlx::test]
    async fn test_panics_dead_letter_with_final_count(db: sqlx::PgPool) {
        for max_retries in [0, 1] {
            let state = test_state(db.clone());
            PanicJob
                .enqueue(state.clone(), "terminal panic".into(), None)
                .await
                .unwrap();
            let id: uuid::Uuid =
                sqlx::query_scalar("SELECT job_id FROM jobs WHERE name = 'PanicJob'")
                    .fetch_one(&db)
                    .await
                    .unwrap();
            let token = CancellationToken::new();
            let handle = tokio::spawn(job_worker(
                state,
                Jobs,
                Duration::from_millis(20),
                max_retries,
                token.clone(),
                JobWorkerConfig {
                    heartbeat_interval: Duration::from_millis(300),
                    reclaim_window: Duration::from_millis(1200),
                    shutdown_drain_timeout: Duration::ZERO,
                },
            ));
            let count: i32 = wait_for(|| async {
                sqlx::query_scalar::<_, i32>(
                    "SELECT error_count FROM dead_letter_jobs WHERE original_job_id = $1",
                )
                .bind(id)
                .fetch_optional(&db)
                .await
                .unwrap()
            })
            .await;
            assert_eq!(count, max_retries + 1);
            let message: String = sqlx::query_scalar(
                "SELECT last_error_message FROM dead_letter_jobs WHERE original_job_id = $1",
            )
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
            assert!(message.contains("fixture panic payload"));
            token.cancel();
            handle.await.unwrap().unwrap();
        }
    }

    /// A cancellation-aware job must be allowed to finish its durable cleanup
    /// inside the bounded drain: the job observes cancellation, writes to the
    /// database, returns Ok, and the worker's normal completion bookkeeping
    /// (DELETE of the job row) runs — all before the worker exits. Without the
    /// drain, the future is dropped at the job's first cancellation checkpoint
    /// and the row survives (locked, then merely unlocked by cleanup).
    async fn run_worker_briefly(db: sqlx::PgPool) -> color_eyre::Result<()> {
        let app_state = TestAppState {
            db,
            cookie_key: CookieKey::generate(),
        };
        tokio::time::timeout(
            Duration::from_secs(5),
            job_worker(
                app_state,
                Jobs,
                Duration::from_millis(50),
                DEFAULT_MAX_RETRIES,
                CancellationToken::new(),
                JobWorkerConfig::default(),
            ),
        )
        .await
        .expect("a schema mismatch must end the worker at startup, not leave it polling")
    }

    /// The eyes incident: `AddJobErrorTracking` was never applied.
    #[sqlx::test]
    async fn test_worker_refuses_to_start_without_error_tracking_columns(db: sqlx::PgPool) {
        sqlx::query("ALTER TABLE jobs DROP COLUMN error_count")
            .execute(&db)
            .await
            .unwrap();

        let error = run_worker_briefly(db).await.unwrap_err();

        let message = format!("{error:#}");
        assert!(message.contains("error_count"), "{message}");
        assert!(message.contains("missing cja migrations"), "{message}");
    }

    #[sqlx::test]
    async fn test_worker_refuses_to_start_without_dead_letter_table(db: sqlx::PgPool) {
        sqlx::query("DROP TABLE dead_letter_jobs")
            .execute(&db)
            .await
            .unwrap();

        let error = run_worker_briefly(db).await.unwrap_err();

        let message = format!("{error:#}");
        assert!(message.contains("dead_letter_jobs"), "{message}");
        assert!(message.contains("missing cja migrations"), "{message}");
    }

    /// An unreachable database is the worker loop's problem, not a reason to
    /// refuse to start.
    #[sqlx::test]
    async fn test_schema_check_ignores_transient_database_errors(db: sqlx::PgPool) {
        db.close().await;

        verify_jobs_schema(&db)
            .await
            .expect("a closed pool is not a schema mismatch");
    }

    #[sqlx::test]
    async fn test_schema_check_passes_on_current_migrations(db: sqlx::PgPool) {
        verify_jobs_schema(&db).await.unwrap();
    }

    #[sqlx::test]
    async fn test_shutdown_drain_lets_a_cooperative_job_finish(db: sqlx::PgPool) {
        let app_state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };

        DrainCooperativeJob
            .clone()
            .enqueue(app_state.clone(), "drain-cooperative-test".to_owned(), None)
            .await
            .unwrap();

        let shutdown_token = CancellationToken::new();
        let worker_token = shutdown_token.clone();
        let handle = tokio::spawn(job_worker(
            app_state,
            Jobs,
            Duration::from_millis(10),
            20,
            worker_token,
            JobWorkerConfig {
                shutdown_drain_timeout: Duration::from_secs(10),
                ..Default::default()
            },
        ));

        wait_until_started(&db, DrainCooperativeJob::NAME).await;
        shutdown_token.cancel();

        tokio::time::timeout(Duration::from_secs(15), handle)
            .await
            .expect("worker must exit inside the drain window")
            .unwrap()
            .expect("worker must not error");

        // The job completed INSIDE the drain, so the success path's durable
        // bookkeeping (DELETE FROM jobs) ran. A row remaining here means the
        // future was dropped before the job could finish.
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(
            remaining, 0,
            "cooperative job must complete (row deleted) inside the drain"
        );
    }

    /// A cancellation-ignoring job must be dropped at the drain deadline, not
    /// run to completion: the worker returns promptly after the drain expires
    /// and `cleanup_worker_locks` has released the row, so another worker (or
    /// a post-restart daemon) can take the job over instead of waiting out the
    /// full reclaim window.
    #[sqlx::test]
    async fn test_shutdown_drain_drops_a_non_cooperative_job(db: sqlx::PgPool) {
        let app_state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };

        DrainStubbornJob
            .clone()
            .enqueue(app_state.clone(), "drain-stubborn-test".to_owned(), None)
            .await
            .unwrap();

        let shutdown_token = CancellationToken::new();
        let worker_token = shutdown_token.clone();
        let handle = tokio::spawn(job_worker(
            app_state,
            Jobs,
            Duration::from_millis(10),
            20,
            worker_token,
            JobWorkerConfig {
                shutdown_drain_timeout: Duration::from_secs(2),
                ..Default::default()
            },
        ));

        wait_until_started(&db, DrainStubbornJob::NAME).await;
        let started = std::time::Instant::now();
        shutdown_token.cancel();

        // The job sleeps for 30 seconds ignoring cancellation; the worker must
        // give up after its 2-second drain instead of waiting that out.
        tokio::time::timeout(Duration::from_secs(15), handle)
            .await
            .expect("worker must return after the drain expires, not after the job finishes")
            .unwrap()
            .expect("worker must not error");
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "worker ran {:?}, longer than its 2s drain — the job future was not dropped",
            started.elapsed()
        );

        // cleanup_worker_locks must have run after the drop: the row stays
        // (unlike the cooperative case) but must be unlocked.
        let (locked_by,): (Option<String>,) =
            sqlx::query_as("SELECT locked_by FROM jobs WHERE name = $1")
                .bind(DrainStubbornJob::NAME)
                .fetch_one(&db)
                .await
                .unwrap();
        assert!(
            locked_by.is_none(),
            "dropped job's lock must be released by cleanup_worker_locks"
        );
        let count: i32 = sqlx::query_scalar("SELECT error_count FROM jobs WHERE name = $1")
            .bind(DrainStubbornJob::NAME)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(count, 0, "graceful drain does not charge a failed attempt");
        let dead_letters: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM dead_letter_jobs")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(dead_letters, 0);
    }

    async fn assert_stubborn_drain(db: sqlx::PgPool, config: JobWorkerConfig) -> Duration {
        let app_state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };

        DrainStubbornJob
            .clone()
            .enqueue(app_state.clone(), "default-drain-test".to_owned(), None)
            .await
            .unwrap();

        let shutdown_token = CancellationToken::new();
        let worker_token = shutdown_token.clone();
        let handle = tokio::spawn(job_worker(
            app_state,
            Jobs,
            Duration::from_millis(10),
            20,
            worker_token,
            config,
        ));

        wait_until_started(&db, DrainStubbornJob::NAME).await;
        let started = std::time::Instant::now();
        shutdown_token.cancel();

        tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .expect("drain must be bounded")
            .unwrap()
            .expect("worker must not error");
        let elapsed = started.elapsed();

        let (locked_by,): (Option<String>,) =
            sqlx::query_as("SELECT locked_by FROM jobs WHERE name = $1")
                .bind(DrainStubbornJob::NAME)
                .fetch_one(&db)
                .await
                .unwrap();
        assert!(locked_by.is_none(), "lock must be released after drain");
        let count: i32 = sqlx::query_scalar("SELECT error_count FROM jobs WHERE name = $1")
            .bind(DrainStubbornJob::NAME)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(count, 0);
        let dead_letters: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM dead_letter_jobs")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(dead_letters, 0);
        elapsed
    }

    /// Default worker config gives a running body a bounded, nonzero drain.
    #[sqlx::test]
    async fn test_job_worker_default_drains_for_bounded_time(db: sqlx::PgPool) {
        let elapsed = assert_stubborn_drain(db, JobWorkerConfig::default()).await;
        assert!(
            elapsed >= Duration::from_secs(1) && elapsed < Duration::from_secs(10),
            "{elapsed:?}"
        );
    }

    #[sqlx::test]
    async fn test_job_worker_zero_drain_releases_uncharged(db: sqlx::PgPool) {
        let elapsed = assert_stubborn_drain(
            db,
            JobWorkerConfig {
                shutdown_drain_timeout: Duration::ZERO,
                ..Default::default()
            },
        )
        .await;
        assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
    }

    /// Test that `fetch_next_job` picks up a stale lock with an unprefixed owner.
    #[sqlx::test]
    async fn test_fetch_next_job_picks_up_stale_locked_job(db: sqlx::PgPool) {
        let app_state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };

        let job_id = uuid::Uuid::new_v4();
        let stale_worker_id = "crashed-worker";

        // Insert a job locked 120 seconds ago
        sqlx::query(
            "INSERT INTO jobs (job_id, name, payload, priority, run_at, created_at, context, error_count, locked_by, locked_at)
             VALUES ($1, $2, $3, $4, NOW(), NOW(), $5, $6, $7, NOW() - interval '120 seconds')",
        )
        .bind(job_id)
        .bind("TestJob")
        .bind(serde_json::json!({"id": "stale-lock-test"}))
        .bind(0)
        .bind("test-stale-lock")
        .bind(0)
        .bind(stale_worker_id)
        .execute(&db)
        .await
        .unwrap();

        // Create a worker with a 60-second reclaim window.
        let worker = Worker::new(
            app_state,
            Jobs,
            Duration::from_secs(1),
            20,
            CancellationToken::new(),
            JobWorkerConfig {
                heartbeat_interval: Duration::from_secs(15),
                reclaim_window: Duration::from_mins(1),
                ..Default::default()
            },
        )
        .unwrap();

        // fetch_next_job should pick up the stale locked job
        let fetched = worker.fetch_next_job().await.unwrap();
        assert!(fetched.is_some());
        let claimed = fetched.unwrap();
        assert_eq!(claimed.job.job_id, job_id);
        assert_eq!(claimed.job.error_count, 1);
        assert_eq!(claimed.previous_locked_by.as_deref(), Some(stale_worker_id));
        let stored_count: i32 =
            sqlx::query_scalar("SELECT error_count FROM jobs WHERE job_id = $1")
                .bind(job_id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(stored_count, 1);
        assert!(
            claimed
                .job
                .last_error_message
                .unwrap()
                .contains(stale_worker_id)
        );
    }

    #[sqlx::test]
    async fn test_terminal_expired_lease_dead_letters_without_dispatch(db: sqlx::PgPool) {
        let job_id = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO jobs (job_id, name, payload, priority, run_at, created_at, context,
               error_count, locked_by, locked_at)
             VALUES ($1, 'PanicJob', '{}', 0, NOW(), NOW(), 'never-dispatched', 1,
               'dead-worker', NOW() - interval '5 seconds')",
        )
        .bind(job_id)
        .execute(&db)
        .await
        .unwrap();
        let worker = Worker::new(
            test_state(db.clone()),
            Jobs,
            Duration::from_millis(20),
            1,
            CancellationToken::new(),
            JobWorkerConfig {
                heartbeat_interval: Duration::from_millis(300),
                reclaim_window: Duration::from_millis(1200),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(worker.fetch_next_job().await.unwrap().is_none());
        let row: (i32, String) = sqlx::query_as(
            "SELECT error_count, last_error_message FROM dead_letter_jobs WHERE original_job_id = $1"
        ).bind(job_id).fetch_one(&db).await.unwrap();
        assert_eq!(row.0, 2);
        assert!(row.1.contains("dead-worker"));
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE job_id = $1")
            .bind(job_id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(remaining, 0);
    }

    #[sqlx::test]
    async fn test_live_job_heartbeat_survives_boundary_failed_ticks(db: sqlx::PgPool) {
        use std::sync::{Arc, atomic::Ordering};
        let state = test_state(db.clone());
        LongJob
            .enqueue(state.clone(), "long".into(), None)
            .await
            .unwrap();
        // CI runs ten feature lanes against separate PostgreSQL containers at
        // once. Give this sustained-heartbeat test enough scheduling margin.
        let lease = JobWorkerConfig {
            heartbeat_interval: Duration::from_millis(800),
            reclaim_window: Duration::from_secs(4),
            ..Default::default()
        };
        let worker = Arc::new(
            Worker::new(
                state.clone(),
                Jobs,
                Duration::from_millis(20),
                20,
                CancellationToken::new(),
                lease,
            )
            .unwrap(),
        );
        let claimed = worker.fetch_next_job().await.unwrap().unwrap();
        let initial: chrono::DateTime<chrono::Utc> =
            sqlx::query_scalar("SELECT locked_at FROM jobs WHERE job_id = $1")
                .bind(claimed.job.job_id)
                .fetch_one(&db)
                .await
                .unwrap();
        // 4s / 800ms = 5 intervals: k-2 failures are the tolerated boundary.
        worker.fail_heartbeats.store(3, Ordering::SeqCst);
        let runner = tokio::spawn({
            let worker = Arc::clone(&worker);
            async move { worker.run_next_job(claimed).await }
        });
        wait_until_started(&db, LongJob::NAME).await;
        let advanced = wait_for(|| async {
            let current: chrono::DateTime<chrono::Utc> =
                sqlx::query_scalar("SELECT locked_at FROM jobs WHERE name = 'LongJob'")
                    .fetch_one(&db)
                    .await
                    .unwrap();
            (current > initial && worker.fail_heartbeats.load(Ordering::SeqCst) == 0)
                .then_some(current)
        })
        .await;
        let competitor = Worker::new(
            state,
            Jobs,
            Duration::from_millis(20),
            20,
            CancellationToken::new(),
            lease,
        )
        .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(13);
        while tokio::time::Instant::now() < deadline {
            assert!(competitor.fetch_next_job().await.unwrap().is_none());
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let row: (i32, chrono::DateTime<chrono::Utc>) =
            sqlx::query_as("SELECT error_count, locked_at FROM jobs WHERE name = 'LongJob'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(row.0, 0);
        assert!(row.1 > advanced);
        assert!(!runner.is_finished());
        runner.abort();
        let _ = runner.await;
        cleanup_worker_locks(&worker).await.unwrap();
    }

    #[sqlx::test]
    async fn test_watchdog_stops_body_before_expiry_and_reclaim_charges_once(db: sqlx::PgPool) {
        use std::sync::{Arc, atomic::Ordering};
        let state = test_state(db.clone());
        LongJob
            .enqueue(state.clone(), "watchdog".into(), None)
            .await
            .unwrap();
        let lease = JobWorkerConfig {
            heartbeat_interval: Duration::from_millis(300),
            reclaim_window: Duration::from_millis(1200),
            ..Default::default()
        };
        let worker = Arc::new(
            Worker::new(
                state.clone(),
                Jobs,
                Duration::from_millis(20),
                20,
                CancellationToken::new(),
                lease,
            )
            .unwrap(),
        );
        let claimed = worker.fetch_next_job().await.unwrap().unwrap();
        let id = claimed.job.job_id;
        worker.fail_heartbeats.store(3, Ordering::SeqCst);
        let runner = tokio::spawn({
            let worker = Arc::clone(&worker);
            async move { worker.run_next_job(claimed).await }
        });
        wait_until_started(&db, LongJob::NAME).await;
        let outcome = tokio::time::timeout(Duration::from_secs(5), runner)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(outcome, RunJobOutcome::LeaseLost(found) if found == id));
        assert_eq!(worker.fail_heartbeats.load(Ordering::SeqCst), 0);
        let row: (i32, Option<String>) =
            sqlx::query_as("SELECT error_count, locked_by FROM jobs WHERE job_id = $1")
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(row.0, 0);
        assert_eq!(row.1.as_deref(), Some(worker.owner_token.as_str()));
        cleanup_worker_locks(&worker).await.unwrap();
        let still_owned: Option<String> =
            sqlx::query_scalar("SELECT locked_by FROM jobs WHERE job_id = $1")
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(still_owned.as_deref(), Some(worker.owner_token.as_str()));
        let competitor = Worker::new(
            state,
            Jobs,
            Duration::from_millis(20),
            20,
            CancellationToken::new(),
            lease,
        )
        .unwrap();
        let reclaimed = wait_for(|| async { competitor.fetch_next_job().await.unwrap() }).await;
        assert_eq!(reclaimed.job.job_id, id);
        assert_eq!(reclaimed.job.error_count, 1);
        assert!(
            reclaimed
                .job
                .last_error_message
                .unwrap()
                .contains(&worker.owner_token)
        );
    }

    #[sqlx::test]
    async fn test_lost_ownership_stops_body_without_touching_new_owner(db: sqlx::PgPool) {
        let state = test_state(db.clone());
        LongJob
            .enqueue(state.clone(), "lost".into(), None)
            .await
            .unwrap();
        let lease = JobWorkerConfig {
            heartbeat_interval: Duration::from_millis(300),
            reclaim_window: Duration::from_millis(1200),
            ..Default::default()
        };
        let worker = Worker::new(
            state,
            Jobs,
            Duration::from_millis(20),
            20,
            CancellationToken::new(),
            lease,
        )
        .unwrap();
        let claimed = worker.fetch_next_job().await.unwrap().unwrap();
        let id = claimed.job.job_id;
        let runner = tokio::spawn(async move { worker.run_next_job(claimed).await });
        wait_until_started(&db, LongJob::NAME).await;
        sqlx::query("UPDATE jobs SET locked_by = 'new-owner' WHERE job_id = $1")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
        let outcome = tokio::time::timeout(Duration::from_secs(5), runner)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(outcome, RunJobOutcome::LeaseLost(found) if found == id));
        let row: (String, i32) =
            sqlx::query_as("SELECT locked_by, error_count FROM jobs WHERE job_id = $1")
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(row, ("new-owner".into(), 0));
    }

    #[tokio::test(start_paused = true)]
    async fn test_lease_tracker_uses_conservative_deadline() {
        let sent = tokio::time::Instant::now();
        let tracker = LeaseTracker {
            last_refresh_sent: sent,
            config: JobWorkerConfig {
                heartbeat_interval: Duration::from_millis(300),
                reclaim_window: Duration::from_millis(1200),
                ..Default::default()
            },
        };
        assert_eq!(tracker.deadline(), sent + Duration::from_millis(1050));
        tokio::time::advance(Duration::from_millis(1049)).await;
        assert!(tokio::time::Instant::now() < tracker.deadline());
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(tokio::time::Instant::now() >= tracker.deadline());
    }

    /// Test that `fetch_next_job` does NOT pick up a job with a fresh lock
    #[sqlx::test]
    async fn test_fetch_next_job_skips_recently_locked_job(db: sqlx::PgPool) {
        let app_state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };

        let job_id = uuid::Uuid::new_v4();
        let active_worker_id = "active-worker";

        // Insert a job locked only 10 seconds ago
        sqlx::query(
            "INSERT INTO jobs (job_id, name, payload, priority, run_at, created_at, context, error_count, locked_by, locked_at)
             VALUES ($1, $2, $3, $4, NOW(), NOW(), $5, $6, $7, NOW() - interval '10 seconds')",
        )
        .bind(job_id)
        .bind("TestJob")
        .bind(serde_json::json!({"id": "recent-lock-test"}))
        .bind(0)
        .bind("test-recent-lock")
        .bind(0)
        .bind(active_worker_id)
        .execute(&db)
        .await
        .unwrap();

        // Create a worker with one-hour reclaim window
        let worker = Worker::new(
            app_state,
            Jobs,
            Duration::from_secs(1),
            20,
            CancellationToken::new(),
            JobWorkerConfig {
                reclaim_window: Duration::from_hours(1),
                ..Default::default()
            },
        )
        .unwrap();

        // fetch_next_job should NOT pick up the recently locked job
        let fetched = worker.fetch_next_job().await.unwrap();
        assert!(fetched.is_none());
    }

    /// Test that unlocked jobs are picked up before stale locked jobs (by priority)
    #[sqlx::test]
    async fn test_fetch_next_job_prefers_unlocked_by_priority(db: sqlx::PgPool) {
        let app_state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };

        let unlocked_job_id = uuid::Uuid::new_v4();
        let stale_locked_job_id = uuid::Uuid::new_v4();

        // Insert unlocked job with higher priority
        sqlx::query(
            "INSERT INTO jobs (job_id, name, payload, priority, run_at, created_at, context, error_count)
             VALUES ($1, $2, $3, $4, NOW(), NOW(), $5, $6)",
        )
        .bind(unlocked_job_id)
        .bind("TestJob")
        .bind(serde_json::json!({"id": "unlocked"}))
        .bind(10) // Higher priority
        .bind("test-unlocked")
        .bind(0)
        .execute(&db)
        .await
        .unwrap();

        // Insert stale locked job with lower priority
        sqlx::query(
            "INSERT INTO jobs (job_id, name, payload, priority, run_at, created_at, context, error_count, locked_by, locked_at)
             VALUES ($1, $2, $3, $4, NOW(), NOW(), $5, $6, $7, NOW() - interval '120 seconds')",
        )
        .bind(stale_locked_job_id)
        .bind("TestJob")
        .bind(serde_json::json!({"id": "stale-locked"}))
        .bind(5) // Lower priority
        .bind("test-stale")
        .bind(0)
        .bind("crashed-worker")
        .execute(&db)
        .await
        .unwrap();

        // Create a worker with 60-second reclaim window
        let worker = Worker::new(
            app_state,
            Jobs,
            Duration::from_secs(1),
            20,
            CancellationToken::new(),
            JobWorkerConfig {
                heartbeat_interval: Duration::from_secs(15),
                reclaim_window: Duration::from_mins(1),
                ..Default::default()
            },
        )
        .unwrap();

        // Should pick the higher priority unlocked job first
        let fetched = worker.fetch_next_job().await.unwrap();
        assert!(fetched.is_some());
        let claimed = fetched.unwrap();
        assert_eq!(claimed.job.job_id, unlocked_job_id);
        assert_eq!(claimed.job.error_count, 0);
    }

    /// Test that among same-priority jobs, the one that became eligible earliest
    /// (smaller `run_at`) is picked first — even when a competing job has an older
    /// `created_at`. This guards the readiness-ordering contract: a job whose
    /// `run_at` was pushed into the future by retry backoff must not cut in front
    /// of a job that has been due longer, just because it was enqueued earlier.
    #[sqlx::test]
    async fn test_fetch_next_job_orders_by_run_at_over_created_at(db: sqlx::PgPool) {
        let app_state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };

        let older_created_later_run_id = uuid::Uuid::new_v4();
        let newer_created_earlier_run_id = uuid::Uuid::new_v4();

        // Job A: created earliest, but only became due 30 seconds ago (e.g. a
        // job that failed and had its run_at pushed out by backoff).
        sqlx::query(
            "INSERT INTO jobs (job_id, name, payload, priority, run_at, created_at, context, error_count)
             VALUES ($1, $2, $3, $4, NOW() - interval '30 seconds', NOW() - interval '300 seconds', $5, $6)",
        )
        .bind(older_created_later_run_id)
        .bind("TestJob")
        .bind(serde_json::json!({"id": "older-created-later-run"}))
        .bind(0)
        .bind("test-older-created")
        .bind(0)
        .execute(&db)
        .await
        .unwrap();

        // Job B: created more recently, but has been due longer (smaller run_at).
        sqlx::query(
            "INSERT INTO jobs (job_id, name, payload, priority, run_at, created_at, context, error_count)
             VALUES ($1, $2, $3, $4, NOW() - interval '120 seconds', NOW() - interval '60 seconds', $5, $6)",
        )
        .bind(newer_created_earlier_run_id)
        .bind("TestJob")
        .bind(serde_json::json!({"id": "newer-created-earlier-run"}))
        .bind(0)
        .bind("test-newer-created")
        .bind(0)
        .execute(&db)
        .await
        .unwrap();

        let worker = Worker::new(
            app_state,
            Jobs,
            Duration::from_secs(1),
            20,
            CancellationToken::new(),
            JobWorkerConfig {
                heartbeat_interval: Duration::from_secs(15),
                reclaim_window: Duration::from_mins(1),
                ..Default::default()
            },
        )
        .unwrap();

        // Both jobs share a priority; the one due longest (smaller run_at) wins,
        // regardless of which was created first.
        let fetched = worker.fetch_next_job().await.unwrap();
        assert!(fetched.is_some());
        assert_eq!(fetched.unwrap().job.job_id, newer_created_earlier_run_id);
    }

    /// Test that the worker loop survives transient database errors instead of
    /// exiting. Before the tick-error backoff was added, a single failed
    /// `fetch_next_job` (e.g. a pool acquire timeout during a database stall)
    /// propagated out of `job_worker` and killed the worker task — and with it,
    /// apps that join on all worker tasks.
    #[sqlx::test]
    async fn test_job_worker_survives_transient_db_errors(db: sqlx::PgPool) {
        let app_state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };

        let shutdown_token = CancellationToken::new();
        let worker_token = shutdown_token.clone();

        // Close the pool so every query the worker makes fails, simulating the
        // database being unreachable.
        db.close().await;

        let handle = tokio::spawn(async move {
            job_worker(
                app_state,
                Jobs,
                Duration::from_millis(10),
                20,
                worker_token,
                JobWorkerConfig::default(),
            )
            .await
        });

        // Give the worker time to hit at least one failing tick. Before the
        // fix, job_worker returned Err almost immediately; with the fix it
        // keeps looping (sleeping in error backoff).
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            !handle.is_finished(),
            "worker exited after a transient DB error instead of retrying"
        );

        // The worker must still respond promptly to shutdown, even while
        // sitting in an error-backoff sleep.
        shutdown_token.cancel();
        let joined = tokio::time::timeout(Duration::from_secs(5), handle).await;
        assert!(
            joined.is_ok(),
            "worker did not shut down promptly after cancellation"
        );
    }

    /// PR review pin (DEV-1537 round 1): the heartbeat must run alongside the
    /// job body, not instead of it. A slow heartbeat UPDATE (row lock held,
    /// pool contention, DB stall) must not freeze the running job for up to
    /// `heartbeat_interval / 3`.
    mod heartbeat_does_not_stall_body {
        use super::*;
        use std::sync::atomic::{AtomicU64, Ordering};

        static MAX_GAP_MS: AtomicU64 = AtomicU64::new(0);

        #[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
        struct TickingJob;

        #[async_trait::async_trait]
        impl Job<TestAppState> for TickingJob {
            const NAME: &'static str = "TickingJob";

            async fn run(&self, app_state: TestAppState) -> color_eyre::Result<()> {
                sqlx::query("UPDATE jobs SET context = 'started' WHERE name = $1")
                    .bind(Self::NAME)
                    .execute(app_state.db())
                    .await?;
                let mut last = std::time::Instant::now();
                loop {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    let now = std::time::Instant::now();
                    let gap = u64::try_from((now - last).as_millis()).unwrap();
                    MAX_GAP_MS.fetch_max(gap, Ordering::SeqCst);
                    last = now;
                }
            }
        }

        impl_job_registry!(TestAppState, TickingJob);

        #[sqlx::test]
        async fn test_slow_heartbeat_does_not_stall_job_body(db: sqlx::PgPool) {
            let state = test_state(db.clone());
            TickingJob
                .enqueue(state.clone(), "ticking".into(), None)
                .await
                .unwrap();
            // Per-attempt heartbeat budget is interval / 3 = 1s.
            let lease = JobWorkerConfig {
                heartbeat_interval: Duration::from_secs(3),
                reclaim_window: Duration::from_secs(12),
                ..Default::default()
            };
            let worker = std::sync::Arc::new(
                Worker::new(
                    state,
                    Jobs,
                    Duration::from_millis(20),
                    20,
                    CancellationToken::new(),
                    lease,
                )
                .unwrap(),
            );
            let claimed = worker.fetch_next_job().await.unwrap().unwrap();
            let id = claimed.job.job_id;
            let runner = tokio::spawn({
                let worker = std::sync::Arc::clone(&worker);
                async move { worker.run_next_job(claimed).await }
            });
            wait_until_started(&db, TickingJob::NAME).await;
            MAX_GAP_MS.store(0, Ordering::SeqCst);

            // Hold the row lock across the first heartbeat tick (~3s after
            // claim) so that heartbeat UPDATE blocks for its full 1s budget.
            let mut tx = db.begin().await.unwrap();
            sqlx::query("SELECT 1 FROM jobs WHERE job_id = $1 FOR UPDATE")
                .bind(id)
                .execute(&mut *tx)
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
            tx.rollback().await.unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;

            let max_gap = MAX_GAP_MS.load(Ordering::SeqCst);
            runner.abort();
            let _ = runner.await;
            cleanup_worker_locks(&worker).await.unwrap();
            assert!(
                max_gap < 400,
                "job body was not polled for {max_gap}ms while a heartbeat UPDATE was in flight"
            );
        }
    }
}
