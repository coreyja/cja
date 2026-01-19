use std::time::Duration;

use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tracing::Span;

use crate::app_state::AppState as AS;

use super::registry::JobRegistry;

/// Default maximum number of retry attempts before a job is permanently deleted.
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
pub const DEFAULT_LOCK_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);

pub(super) type RunJobResult = Result<RunJobSuccess, JobError>;

#[derive(Debug)]
pub(super) struct RunJobSuccess(JobFromDB);

#[derive(Debug)]
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

impl JobFromDB {
    fn from_row(row: &tokio_postgres::Row) -> Self {
        Self {
            job_id: row.get(0),
            name: row.get(1),
            payload: row.get(2),
            priority: row.get(3),
            run_at: row.get(4),
            created_at: row.get(5),
            context: row.get(6),
            error_count: row.get(7),
            last_error_message: row.get(8),
            last_failed_at: row.get(9),
        }
    }
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

        let client = self.state.db().get().await?;

        if let Err(e) = job_result {
            // Extract error message from color_eyre::Report
            let error_message = format!("{e}");

            // Check if job has exceeded max retries
            if job.error_count >= self.max_retries {
                // Permanently delete job that has exceeded max retries
                tracing::error!(
                    worker.id = %self.id,
                    job_id = %job.job_id,
                    error_count = job.error_count,
                    max_retries = self.max_retries,
                    "Job permanently failed - max retries exceeded"
                );

                sql_check_macros::query!(
                    "DELETE FROM jobs
                    WHERE job_id = $1 AND locked_by = $2",
                    job.job_id,
                    self.id.to_string()
                )
                .execute(&*client)
                .await?;

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

            sql_check_macros::query!(
                "UPDATE jobs
                SET locked_by = NULL,
                    locked_at = NULL,
                    error_count = error_count + 1,
                    last_error_message = $3,
                    last_failed_at = NOW(),
                    run_at = NOW() + (POWER(2, error_count + 1)) * interval '1 second'
                WHERE job_id = $1 AND locked_by = $2",
                job.job_id,
                self.id.to_string(),
                error_message
            )
            .execute(&*client)
            .await?;

            return Ok(Err(JobError(job, e)));
        }

        sql_check_macros::query!(
            "DELETE FROM jobs
            WHERE job_id = $1 AND locked_by = $2",
            job.job_id,
            self.id.to_string()
        )
        .execute(&*client)
        .await?;

        Ok(Ok(RunJobSuccess(job)))
    }

    #[tracing::instrument(
        name = "worker.fetch_next_job",
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

        let client = self.state.db().get().await?;

        let row = client
            .query_opt(
                "UPDATE jobs
                SET LOCKED_BY = $1, LOCKED_AT = NOW()
                WHERE job_id = (
                    SELECT job_id
                    FROM jobs
                    WHERE run_at <= NOW()
                      AND (
                        locked_by IS NULL
                        OR locked_at < NOW() - ($2 || ' seconds')::interval
                      )
                    ORDER BY priority DESC, created_at ASC
                    LIMIT 1
                    FOR UPDATE SKIP LOCKED
                )
                RETURNING job_id, name, payload, priority, run_at, created_at, context, error_count, last_error_message, last_failed_at",
                &[&self.id.to_string(), &lock_timeout_secs.to_string()],
            )
            .await?;

        let job = row.map(|row| JobFromDB::from_row(&row));

        if let Some(job) = &job {
            let span = Span::current();
            span.record("job.id", job.job_id.to_string());
            span.record("job.name", &job.name);
        }

        Ok(job)
    }

    #[tracing::instrument(
        name = "worker.tick",
        skip(self),
        fields(
            worker.id = %self.id,
        ),
    )]
    async fn tick(&self) -> color_eyre::Result<()> {
        let job = self.fetch_next_job().await?;

        let Some(job) = job else {
            let duration = self.sleep_duration;
            tracing::debug!(worker.id =% self.id, ?duration, "No Job to Run, sleeping for requested duration");

            tokio::time::sleep(duration).await;

            return Ok(());
        };

        let result = self.run_next_job(job).await?;

        match result {
            Ok(RunJobSuccess(job)) => {
                tracing::info!(worker.id =% self.id, job_id =% job.job_id, "Job Ran");
            }
            Err(job_error) => {
                tracing::error!(
                    worker.id =% self.id,
                    job_id =% job_error.0.job_id,
                    error_count =% job_error.0.error_count,
                    error_msg =% job_error.1,
                    "Job Errored"
                );
            }
        }

        Ok(())
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

    let client = worker.state.db().get().await?;

    let locks_released = sql_check_macros::query!(
        "UPDATE jobs
         SET locked_by = NULL, locked_at = NULL
         WHERE locked_by = $1",
        worker.id.to_string()
    )
    .execute(&*client)
    .await?;

    tracing::info!(
        worker_id = %worker.id,
        locks_released = locks_released,
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
/// * `max_retries` - Maximum number of times to retry a failed job before permanent deletion (default: 20)
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
/// - If `error_count` >= `max_retries`, the job is permanently deleted
///
/// # Graceful Shutdown
///
/// When the `shutdown_token` is cancelled:
/// - The worker stops polling for new jobs
/// - Any currently executing job is allowed to complete
/// - Database locks are released immediately (instead of waiting for the lock timeout)
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
    let worker = Worker::new(
        app_state,
        registry,
        sleep_duration,
        max_retries,
        shutdown_token.clone(),
        lock_timeout,
    );

    loop {
        tokio::select! {
            result = worker.tick() => {
                result?;
            }
            () = shutdown_token.cancelled() => {
                tracing::info!(worker_id = %worker.id, "Job worker shutdown requested");
                break;
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
    use crate::app_state::{AppState, DbPool};
    use crate::db::Migrator;
    use crate::impl_job_registry;
    use crate::jobs::Job;
    use crate::server::cookies::CookieKey;
    use deadpool_postgres::{Config, Pool, Runtime};
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use tokio_postgres::NoTls;

    // Test database setup infrastructure
    static TEST_DB_MUTEX: std::sync::LazyLock<Arc<Mutex<()>>> =
        std::sync::LazyLock::new(|| Arc::new(Mutex::new(())));

    fn base_db_url() -> String {
        std::env::var("DATABASE_URL").unwrap_or_else(|_| "postgres://localhost/postgres".to_string())
    }

    fn url_without_db(url: &str) -> String {
        if let Some(idx) = url.rfind('/') {
            url[..idx].to_string()
        } else {
            url.to_string()
        }
    }

    async fn create_pool(db_url: &str) -> Result<Pool, deadpool_postgres::CreatePoolError> {
        let config = db_url
            .parse::<tokio_postgres::Config>()
            .expect("failed to parse DATABASE_URL");

        let mut cfg = Config::new();
        cfg.dbname = config.get_dbname().map(String::from);
        cfg.host = config.get_hosts().first().map(|h| match h {
            tokio_postgres::config::Host::Tcp(s) => s.clone(),
            tokio_postgres::config::Host::Unix(p) => p.to_string_lossy().into_owned(),
        });
        cfg.port = config.get_ports().first().copied();
        cfg.user = config.get_user().map(String::from);
        cfg.password = config
            .get_password()
            .map(|p| String::from_utf8_lossy(p).into_owned());

        cfg.create_pool(Some(Runtime::Tokio1), NoTls)
    }

    async fn setup_test_db(test_name: &str) -> (Pool, String) {
        let _lock = TEST_DB_MUTEX.lock().await;

        let db_name = format!("cja_worker_test_{}_{}", test_name, uuid::Uuid::new_v4().to_string().replace('-', ""));
        let base_url = base_db_url();
        let base_url_without_db = url_without_db(&base_url);

        // Connect to the base database to create our test database
        let base_pool = create_pool(&base_url).await.expect("failed to create base pool");
        let base_client = base_pool.get().await.expect("failed to get base client");

        // Drop existing test database if it exists (from a failed previous run)
        let drop_sql = format!("DROP DATABASE IF EXISTS \"{db_name}\"");
        base_client.execute(&drop_sql, &[]).await.expect("failed to drop test db");

        // Create the test database
        let create_sql = format!("CREATE DATABASE \"{db_name}\"");
        base_client.execute(&create_sql, &[]).await.expect("failed to create test db");

        drop(base_client);

        // Connect to the new test database
        let test_db_url = format!("{base_url_without_db}/{db_name}");
        let test_pool = create_pool(&test_db_url).await.expect("failed to create test pool");

        // Run migrations
        let migrator = Migrator::from_path("./migrations").expect("failed to load migrations");
        let client = test_pool.get().await.expect("failed to get client for migrations");
        migrator.run(&client).await.expect("failed to run migrations");
        drop(client);

        (test_pool, db_name)
    }

    async fn cleanup_test_db(db_name: &str) {
        let base_url = base_db_url();
        if let Ok(base_pool) = create_pool(&base_url).await {
            if let Ok(client) = base_pool.get().await {
                // Terminate existing connections
                let terminate_sql = format!(
                    "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{db_name}'"
                );
                let _ = client.execute(&terminate_sql, &[]).await;
                // Drop the database
                let _ = client.execute(&format!("DROP DATABASE IF EXISTS \"{db_name}\""), &[]).await;
            }
        }
    }

    #[derive(Clone)]
    struct TestAppState {
        db: DbPool,
        cookie_key: CookieKey,
    }

    impl AppState for TestAppState {
        fn db(&self) -> &DbPool {
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
    #[tokio::test]
    async fn test_fetch_next_job_picks_up_stale_locked_job() {
        let (pool, db_name) = setup_test_db("stale_lock").await;
        let app_state = TestAppState {
            db: pool.clone(),
            cookie_key: CookieKey::generate(),
        };

        let job_id = uuid::Uuid::new_v4();
        let stale_worker_id = "crashed-worker";

        // Insert a job locked 120 seconds ago
        let client = pool.get().await.unwrap();
        client
            .execute(
                "INSERT INTO jobs (job_id, name, payload, priority, run_at, created_at, context, error_count, locked_by, locked_at)
                 VALUES ($1, $2, $3, $4, NOW(), NOW(), $5, $6, $7, NOW() - interval '120 seconds')",
                &[
                    &job_id,
                    &"TestJob",
                    &serde_json::json!({"id": "stale-lock-test"}),
                    &0i32,
                    &"test-stale-lock",
                    &0i32,
                    &stale_worker_id,
                ],
            )
            .await
            .unwrap();
        drop(client);

        // Create a worker with 60 second lock timeout
        let worker = Worker::new(
            app_state,
            Jobs,
            Duration::from_secs(1),
            20,
            CancellationToken::new(),
            Duration::from_secs(60), // 60 second timeout
        );

        // fetch_next_job should pick up the stale locked job
        let fetched = worker.fetch_next_job().await.unwrap();
        assert!(fetched.is_some());
        assert_eq!(fetched.unwrap().job_id, job_id);

        pool.close();
        cleanup_test_db(&db_name).await;
    }

    /// Test that `fetch_next_job` does NOT pick up a job with a fresh lock
    #[tokio::test]
    async fn test_fetch_next_job_skips_recently_locked_job() {
        let (pool, db_name) = setup_test_db("recent_lock").await;
        let app_state = TestAppState {
            db: pool.clone(),
            cookie_key: CookieKey::generate(),
        };

        let job_id = uuid::Uuid::new_v4();
        let active_worker_id = "active-worker";

        // Insert a job locked only 10 seconds ago
        let client = pool.get().await.unwrap();
        client
            .execute(
                "INSERT INTO jobs (job_id, name, payload, priority, run_at, created_at, context, error_count, locked_by, locked_at)
                 VALUES ($1, $2, $3, $4, NOW(), NOW(), $5, $6, $7, NOW() - interval '10 seconds')",
                &[
                    &job_id,
                    &"TestJob",
                    &serde_json::json!({"id": "recent-lock-test"}),
                    &0i32,
                    &"test-recent-lock",
                    &0i32,
                    &active_worker_id,
                ],
            )
            .await
            .unwrap();
        drop(client);

        // Create a worker with 1 hour lock timeout
        let worker = Worker::new(
            app_state,
            Jobs,
            Duration::from_secs(1),
            20,
            CancellationToken::new(),
            Duration::from_secs(3600), // 1 hour timeout
        );

        // fetch_next_job should NOT pick up the recently locked job
        let fetched = worker.fetch_next_job().await.unwrap();
        assert!(fetched.is_none());

        pool.close();
        cleanup_test_db(&db_name).await;
    }

    /// Test that unlocked jobs are picked up before stale locked jobs (by priority)
    #[tokio::test]
    async fn test_fetch_next_job_prefers_unlocked_by_priority() {
        let (pool, db_name) = setup_test_db("priority").await;
        let app_state = TestAppState {
            db: pool.clone(),
            cookie_key: CookieKey::generate(),
        };

        let unlocked_job_id = uuid::Uuid::new_v4();
        let stale_locked_job_id = uuid::Uuid::new_v4();

        let client = pool.get().await.unwrap();

        // Insert unlocked job with higher priority
        client
            .execute(
                "INSERT INTO jobs (job_id, name, payload, priority, run_at, created_at, context, error_count)
                 VALUES ($1, $2, $3, $4, NOW(), NOW(), $5, $6)",
                &[
                    &unlocked_job_id,
                    &"TestJob",
                    &serde_json::json!({"id": "unlocked"}),
                    &10i32, // Higher priority
                    &"test-unlocked",
                    &0i32,
                ],
            )
            .await
            .unwrap();

        // Insert stale locked job with lower priority
        client
            .execute(
                "INSERT INTO jobs (job_id, name, payload, priority, run_at, created_at, context, error_count, locked_by, locked_at)
                 VALUES ($1, $2, $3, $4, NOW(), NOW(), $5, $6, $7, NOW() - interval '120 seconds')",
                &[
                    &stale_locked_job_id,
                    &"TestJob",
                    &serde_json::json!({"id": "stale-locked"}),
                    &5i32, // Lower priority
                    &"test-stale",
                    &0i32,
                    &"crashed-worker",
                ],
            )
            .await
            .unwrap();
        drop(client);

        // Create a worker with 60 second lock timeout
        let worker = Worker::new(
            app_state,
            Jobs,
            Duration::from_secs(1),
            20,
            CancellationToken::new(),
            Duration::from_secs(60),
        );

        // Should pick the higher priority unlocked job first
        let fetched = worker.fetch_next_job().await.unwrap();
        assert!(fetched.is_some());
        assert_eq!(fetched.unwrap().job_id, unlocked_job_id);

        pool.close();
        cleanup_test_db(&db_name).await;
    }
}
