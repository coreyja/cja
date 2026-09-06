use std::time::Duration;

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

/// Default lock timeout duration (2 hours).
///
/// Jobs locked for longer than this duration will be considered abandoned and
/// made available for other workers to pick up. This handles cases where a worker
/// crashes or becomes unresponsive while processing a job.
pub const DEFAULT_LOCK_TIMEOUT: Duration = Duration::from_hours(2);

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
    state: AppState,
    registry: R,
    sleep_duration: Duration,
    max_retries: i32,
    cancellation_token: CancellationToken,
    lock_timeout: Duration,
}

impl<AppState: AS, R: JobRegistry<AppState>> Worker<AppState, R> {
    fn new(
        state: AppState,
        registry: R,
        sleep_duration: Duration,
        max_retries: i32,
        cancellation_token: CancellationToken,
        lock_timeout: Duration,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4(),
            state,
            registry,
            sleep_duration,
            max_retries,
            cancellation_token,
            lock_timeout,
        }
    }

    #[tracing::instrument(
        name = "worker.run_job",
        skip(self, job),
        fields(
            job.id = %job.job_id,
            job.name = job.name,
            job.priority = job.priority,
            job.run_at = %job.run_at,
            job.created_at = %job.created_at,
            job.context = job.context,
            job.error_count = job.error_count,
            worker.id = %self.id,
        )
        err,
    )]
    async fn run_job(&self, job: &JobFromDB) -> color_eyre::Result<()> {
        self.registry
            .run_job(job, self.state.clone(), self.cancellation_token.clone())
            .await
    }

    pub(crate) async fn run_next_job(&self, job: JobFromDB) -> color_eyre::Result<RunJobResult> {
        let job_result = self.run_job(&job).await;

        if let Err(e) = job_result {
            // Extract error message from color_eyre::Report
            let error_message = format!("{e}");

            // Check if job has exceeded max retries
            if job.error_count >= self.max_retries {
                // Move job to dead letter queue
                tracing::error!(
                    worker.id = %self.id,
                    job_id = %job.job_id,
                    error_count = job.error_count,
                    max_retries = self.max_retries,
                    "Job permanently failed - moved to dead letter queue"
                );

                let mut tx = self.state.db().begin().await?;

                sqlx::query(
                    "INSERT INTO dead_letter_jobs (original_job_id, name, payload, context, priority, error_count, last_error_message, created_at)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                )
                .bind(job.job_id)
                .bind(&job.name)
                .bind(&job.payload)
                .bind(&job.context)
                .bind(job.priority)
                .bind(job.error_count)
                .bind(&error_message)
                .bind(job.created_at)
                .execute(&mut *tx)
                .await?;

                sqlx::query("DELETE FROM jobs WHERE job_id = $1 AND locked_by = $2")
                    .bind(job.job_id)
                    .bind(self.id.to_string())
                    .execute(&mut *tx)
                    .await?;

                tx.commit().await?;

                return Ok(Err(JobError(job, e)));
            }

            // Job is under max retries - requeue with exponential backoff
            tracing::warn!(
                worker.id = %self.id,
                job_id = %job.job_id,
                error_count = job.error_count,
                retry_attempt = job.error_count + 1,
                "Job failed, retry #{}",
                job.error_count + 1
            );

            sqlx::query(
                "
                UPDATE jobs
                SET locked_by = NULL,
                    locked_at = NULL,
                    error_count = error_count + 1,
                    last_error_message = $3,
                    last_failed_at = NOW(),
                    run_at = NOW() + (POWER(2, error_count + 1)) * interval '1 second'
                WHERE job_id = $1 AND locked_by = $2
                ",
            )
            .bind(job.job_id)
            .bind(self.id.to_string())
            .bind(error_message)
            .execute(self.state.db())
            .await?;

            return Ok(Err(JobError(job, e)));
        }

        sqlx::query(
            "
                DELETE FROM jobs
                WHERE job_id = $1 AND locked_by = $2
                ",
        )
        .bind(job.job_id)
        .bind(self.id.to_string())
        .execute(self.state.db())
        .await?;

        Ok(Ok(RunJobSuccess(job)))
    }

    #[tracing::instrument(
        name = "worker.fetch_next_job",
        level = "trace",
        skip(self),
        fields(
            worker.id = %self.id,
            job.id,
            job.name,
            lock_timeout_secs = self.lock_timeout.as_secs(),
        ),
        err,
    )]
    #[allow(clippy::cast_possible_wrap)]
    async fn fetch_next_job(&self) -> color_eyre::Result<Option<JobFromDB>> {
        // Cast is safe: lock timeouts are typically hours, not approaching i64::MAX seconds
        let lock_timeout_secs = self.lock_timeout.as_secs() as i64;

        let job = sqlx::query_as::<_, JobFromDB>(
            "
            UPDATE jobs
            SET LOCKED_BY = $1, LOCKED_AT = NOW()
            WHERE job_id = (
                SELECT job_id
                FROM jobs
                WHERE run_at <= NOW()
                  AND (
                    locked_by IS NULL
                    OR locked_at < NOW() - ($2 || ' seconds')::interval
                  )
                ORDER BY priority DESC, run_at ASC, created_at ASC
                LIMIT 1
                FOR UPDATE SKIP LOCKED
            )
            RETURNING job_id, name, payload, priority, run_at, created_at, context, error_count, last_error_message, last_failed_at
            ",
        )
        .bind(self.id.to_string())
        .bind(lock_timeout_secs.to_string())
        .fetch_optional(self.state.db())
        .await?;

        if let Some(job) = &job {
            let span = Span::current();
            span.record("job.id", job.job_id.to_string());
            span.record("job.name", &job.name);
        }

        Ok(job)
    }
}

/// Release database locks held by a worker.
///
/// This should be called during graceful shutdown to immediately release any job locks
/// held by this worker, rather than waiting for the 2-hour lock timeout.
async fn cleanup_worker_locks<AppState: AS, R: JobRegistry<AppState>>(
    worker: &Worker<AppState, R>,
) -> color_eyre::Result<()> {
    tracing::info!(worker_id = %worker.id, "Releasing database locks");

    let result = sqlx::query(
        "UPDATE jobs
         SET locked_by = NULL, locked_at = NULL
         WHERE locked_by = $1",
    )
    .bind(worker.id.to_string())
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
/// * `lock_timeout` - How long a job can be locked before it's considered abandoned and
///   becomes available for other workers (default: 2 hours)
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
/// - Any currently executing job is allowed to complete
/// - Database locks are released immediately (instead of waiting for the lock timeout)
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
/// # Lock Timeout
///
/// If a worker crashes or becomes unresponsive while processing a job, the job will remain
/// locked in the database. The `lock_timeout` parameter controls how long to wait before
/// considering such jobs abandoned. After the timeout expires, any worker can pick up the
/// job and retry it.
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
/// // Start worker with graceful shutdown support and lock timeout
/// tokio::spawn(async move {
///     cja::jobs::worker::job_worker(
///         app_state,
///         registry,
///         Duration::from_secs(60),      // poll every 60s when idle
///         20,                            // max 20 retries
///         worker_token,                  // for graceful shutdown
///         Duration::from_secs(2 * 3600), // 2 hour lock timeout
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
    lock_timeout: Duration,
) -> color_eyre::Result<()> {
    job_worker_with_shutdown_drain(
        app_state,
        registry,
        sleep_duration,
        max_retries,
        shutdown_token,
        lock_timeout,
        Duration::ZERO,
    )
    .await
}

/// Start a worker which gives an in-flight job a bounded opportunity to
/// observe cancellation and finish its durable cleanup.
///
/// Fetching and idle waits remain immediately cancellable. Once a job has
/// been claimed, cancellation waits at most `shutdown_drain_timeout` for its
/// future to finish. On expiry the future is dropped and this worker's locks
/// are released. Use [`job_worker`] when immediate cancellation is desired.
#[allow(clippy::too_many_arguments)]
pub async fn job_worker_with_shutdown_drain<AppState: AS>(
    app_state: AppState,
    registry: impl JobRegistry<AppState>,
    sleep_duration: Duration,
    max_retries: i32,
    shutdown_token: CancellationToken,
    lock_timeout: Duration,
    shutdown_drain_timeout: Duration,
) -> color_eyre::Result<()> {
    let worker = Worker::new(
        app_state,
        registry,
        sleep_duration,
        max_retries,
        shutdown_token.clone(),
        lock_timeout,
    );

    let mut tick_error_backoff = TICK_ERROR_BACKOFF_BASE;

    loop {
        let fetched = tokio::select! {
            result = worker.fetch_next_job() => result,
            () = shutdown_token.cancelled() => break,
        };
        let result = match fetched {
            Ok(Some(job)) => {
                let mut running = std::pin::pin!(worker.run_next_job(job));
                let result = tokio::select! {
                    result = &mut running => result,
                    () = shutdown_token.cancelled() => {
                        if shutdown_drain_timeout.is_zero() {
                            break;
                        }
                        match tokio::time::timeout(shutdown_drain_timeout, &mut running).await {
                            Ok(result) => result,
                            Err(_) => break,
                        }
                    }
                }?;
                match result {
                    Ok(RunJobSuccess(job)) => {
                        tracing::info!(worker.id = %worker.id, job_id = %job.job_id, "Job Ran");
                    }
                    Err(job_error) => {
                        tracing::error!(worker.id = %worker.id, job_id = %job_error.0.job_id, error_count = %job_error.0.error_count, error_msg = %job_error.1, "Job Errored");
                    }
                }
                Ok(())
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

    impl_job_registry!(TestAppState, TestJob);

    /// Test that `fetch_next_job` picks up a job with a stale lock (lock older than timeout)
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

        // Create a worker with 60 second lock timeout
        let worker = Worker::new(
            app_state,
            Jobs,
            Duration::from_secs(1),
            20,
            CancellationToken::new(),
            Duration::from_mins(1), // 60 second timeout
        );

        // fetch_next_job should pick up the stale locked job
        let fetched = worker.fetch_next_job().await.unwrap();
        assert!(fetched.is_some());
        assert_eq!(fetched.unwrap().job_id, job_id);
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

        // Create a worker with 1 hour lock timeout
        let worker = Worker::new(
            app_state,
            Jobs,
            Duration::from_secs(1),
            20,
            CancellationToken::new(),
            Duration::from_hours(1), // 1 hour timeout
        );

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

        // Create a worker with 60 second lock timeout
        let worker = Worker::new(
            app_state,
            Jobs,
            Duration::from_secs(1),
            20,
            CancellationToken::new(),
            Duration::from_mins(1),
        );

        // Should pick the higher priority unlocked job first
        let fetched = worker.fetch_next_job().await.unwrap();
        assert!(fetched.is_some());
        assert_eq!(fetched.unwrap().job_id, unlocked_job_id);
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
            Duration::from_mins(1),
        );

        // Both jobs share a priority; the one due longest (smaller run_at) wins,
        // regardless of which was created first.
        let fetched = worker.fetch_next_job().await.unwrap();
        assert!(fetched.is_some());
        assert_eq!(fetched.unwrap().job_id, newer_created_earlier_run_id);
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
                DEFAULT_LOCK_TIMEOUT,
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
}
