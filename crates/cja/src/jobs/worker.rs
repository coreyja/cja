use std::time::Duration;

use thiserror::Error;
use tracing::Span;

use crate::app_state::AppState as AS;

use super::registry::JobRegistry;

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
}

impl<AppState: AS, R: JobRegistry<AppState>> Worker<AppState, R> {
    fn new(state: AppState, registry: R, sleep_duration: Duration, max_retries: i32) -> Self {
        Self {
            id: uuid::Uuid::new_v4(),
            state,
            registry,
            sleep_duration,
            max_retries,
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
        self.registry.run_job(job, self.state.clone()).await
    }

    pub(crate) async fn run_next_job(&self, job: JobFromDB) -> color_eyre::Result<RunJobResult> {
        let job_result = self.run_job(&job).await;

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
        skip(self),
        fields(
            worker.id = %self.id,
            job.id,
            job.name,
        ),
        err,
    )]
    async fn fetch_next_job(&self) -> color_eyre::Result<Option<JobFromDB>> {
        let job = sqlx::query_as::<_, JobFromDB>(
            "
            UPDATE jobs
            SET LOCKED_BY = $1, LOCKED_AT = NOW()
            WHERE job_id = (
                SELECT job_id
                FROM jobs
                WHERE run_at <= NOW() AND locked_by IS NULL
                ORDER BY priority DESC, created_at ASC
                LIMIT 1
                FOR UPDATE SKIP LOCKED
            )
            RETURNING job_id, name, payload, priority, run_at, created_at, context, error_count, last_error_message, last_failed_at
            ",
        )
        .bind(self.id.to_string())
        .fetch_optional(self.state.db())
        .await?;

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
///
/// # Retry Behavior
///
/// When a job fails:
/// - The error count is incremented
/// - The error message and timestamp are recorded
/// - The job is requeued with exponential backoff: delay = `2^(error_count)` seconds
/// - If `error_count` >= `max_retries`, the job is permanently deleted
///
/// # Example
///
/// ```ignore
/// use std::time::Duration;
///
/// // Start worker with default 20 max retries
/// cja::jobs::worker::job_worker(
///     app_state,
///     registry,
///     Duration::from_secs(60),
///     20,
/// ).await.unwrap();
/// ```
pub async fn job_worker<AppState: AS>(
    app_state: AppState,
    registry: impl JobRegistry<AppState>,
    sleep_duration: Duration,
    max_retries: i32,
) -> color_eyre::Result<()> {
    let worker = Worker::new(app_state, registry, sleep_duration, max_retries);

    loop {
        worker.tick().await?;
    }
}
