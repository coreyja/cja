//! Background job processing with `PostgreSQL` persistence.
//!
//! The job system provides a reliable, database-backed work queue with automatic retries,
//! exponential backoff, priority scheduling, and a dead letter queue for permanently
//! failed jobs.
//!
//! # Defining a Job
//!
//! Implement the [`Job`] trait for your job type:
//!
//! ```rust,ignore
//! use cja::jobs::Job;
//! use serde::{Serialize, Deserialize};
//!
//! #[derive(Debug, Serialize, Deserialize, Clone)]
//! struct SendEmailJob {
//!     to: String,
//!     subject: String,
//! }
//!
//! #[async_trait::async_trait]
//! impl Job<MyAppState> for SendEmailJob {
//!     const NAME: &'static str = "SendEmailJob";
//!
//!     async fn run(&self, app_state: MyAppState) -> cja::Result<()> {
//!         // Send email using app_state.db() for templates, etc.
//!         Ok(())
//!     }
//! }
//! ```
//!
//! # Registering Jobs
//!
//! All job types must be registered via the [`impl_job_registry!`](crate::impl_job_registry) macro:
//!
//! ```rust,ignore
//! cja::impl_job_registry!(MyAppState, SendEmailJob, CleanupJob, ReportJob);
//! ```
//!
//! This generates a `Jobs` struct that routes jobs by their `NAME` constant.
//! Forgetting to register a job type means it won't be processed.
//!
//! # Enqueuing Jobs
//!
//! ```rust,ignore
//! let job = SendEmailJob {
//!     to: "user@example.com".into(),
//!     subject: "Welcome!".into(),
//! };
//!
//! // Default priority (0)
//! job.clone().enqueue(app_state.clone(), "user-signup".into(), None).await?;
//!
//! // High priority — higher values run first (ORDER BY priority DESC)
//! job.enqueue(app_state, "urgent".into(), Some(10)).await?;
//! ```
//!
//! **Priority**: Higher values run first. Priority 10 runs before 0, and -10
//! runs last. This is per-enqueue, not per-job-type.
//!
//! # Retry Behavior
//!
//! Failed jobs are automatically retried with exponential backoff
//! (delay = `2^(error_count + 1)` seconds):
//!
//! | Retry # | Delay |
//! |---------|-------|
//! | 1 | 4 seconds |
//! | 2 | 8 seconds |
//! | 5 | ~1 minute |
//! | 10 | ~17 minutes |
//! | 20 | ~12 days |
//!
//! After [`DEFAULT_MAX_RETRIES`] (20) attempts,
//! jobs are moved to the `dead_letter_jobs` table.
//!
//! # Idempotency
//!
//! **Jobs must be idempotent.** The system retries on timeouts, worker crashes,
//! and network failures. Guard against double-application with:
//!
//! - `ON CONFLICT DO NOTHING` for inserts
//! - Early-exit checks at job start
//! - Idempotency keys in your domain logic
//!
//! # Concurrent Workers
//!
//! Multiple [`worker::job_worker`] instances can run safely — the worker SQL
//! uses `FOR UPDATE SKIP LOCKED` to prevent duplicate processing at the
//! database level.
//!
//! # Cancellation Support
//!
//! Long-running jobs can override [`Job::run_with_cancellation`] to exit
//! gracefully during shutdown by checking the cancellation token periodically.
//!
//! # Database Schema
//!
//! ## `jobs` table
//!
//! | Column | Type | Description |
//! |--------|------|-------------|
//! | `job_id` | UUID | Primary key |
//! | `name` | TEXT | Job type name (matches `Job::NAME`) |
//! | `payload` | JSONB | Serialized job data |
//! | `priority` | INT | Higher = runs first (`ORDER BY priority DESC`) |
//! | `run_at` | TIMESTAMPTZ | When job can next be executed |
//! | `created_at` | TIMESTAMPTZ | When job was enqueued |
//! | `locked_at` | TIMESTAMPTZ | When a worker locked this job |
//! | `locked_by` | TEXT | Worker UUID that holds the lock |
//! | `context` | TEXT | Debug info (e.g., "user-signup") |
//! | `error_count` | INT | Number of failures |
//! | `last_error_message` | TEXT | Most recent error |
//! | `last_failed_at` | TIMESTAMPTZ | Timestamp of last failure |
//!
//! ## `dead_letter_jobs` table
//!
//! Jobs that exceeded max retries are moved here for manual investigation.
//!
//! | Column | Type | Description |
//! |--------|------|-------------|
//! | `id` | UUID | Primary key |
//! | `original_job_id` | UUID | Original `job_id` |
//! | `name` | TEXT | Job type name |
//! | `payload` | JSONB | Serialized job data |
//! | `context` | TEXT | Debug info |
//! | `priority` | INT | Original priority |
//! | `error_count` | INT | Total failure count |
//! | `last_error_message` | TEXT | Final error |
//! | `created_at` | TIMESTAMPTZ | Original creation time |
//! | `failed_at` | TIMESTAMPTZ | When moved to dead letter |

use crate::app_state::AppState as AS;
use serde::{Serialize, de::DeserializeOwned};
use thiserror::Error;
use tracing::instrument;

pub mod registry;

pub use tokio_util::sync::CancellationToken;
pub use worker::{DEFAULT_LOCK_TIMEOUT, DEFAULT_MAX_RETRIES};

#[derive(Debug, Error)]
pub enum EnqueueError {
    #[error("SqlxError: {0}")]
    SqlxError(#[from] sqlx::Error),
    #[error("SerdeJsonError: {0}")]
    SerdeJsonError(#[from] serde_json::Error),
}

/// A trait for defining background jobs that can be enqueued and processed asynchronously.
///
/// Jobs must be serializable and provide a unique name identifier. The job system
/// handles persistence, retries, and concurrent execution automatically.
///
/// # Automatic Retry Behavior
///
/// All jobs are automatically retried on failure with exponential backoff:
/// - Failed jobs are requeued with increasing delays: 2, 4, 8, 16, 32... seconds
/// - Error messages and failure timestamps are tracked in the database
/// - Jobs are moved to a dead letter queue after exceeding the configured max retries (default: 20)
/// - No manual intervention required for transient failures
///
/// # Lock Timeout (Abandoned Job Recovery)
///
/// Jobs are locked while being processed to prevent multiple workers from running the same
/// job. If a worker crashes or becomes unresponsive, the lock remains but the job is never
/// completed. The lock timeout mechanism handles this:
/// - Jobs locked longer than the timeout (default: 2 hours) are considered abandoned
/// - Any worker can pick up abandoned jobs and retry them
/// - This ensures jobs are eventually processed even after worker failures
///
/// # Example
///
/// ```rust
/// use cja::jobs::Job;
/// use serde::{Serialize, Deserialize};
///
/// # #[derive(Debug, Clone)]
/// # struct MyAppState {
/// #     db: sqlx::PgPool,
/// # }
/// # impl cja::app_state::AppState for MyAppState {
/// #     fn db(&self) -> &sqlx::PgPool { &self.db }
/// #     fn version(&self) -> &str { "1.0.0" }
/// #     fn cookie_key(&self) -> &cja::server::cookies::CookieKey { todo!() }
/// # }
///
/// #[derive(Debug, Serialize, Deserialize, Clone)]
/// struct EmailJob {
///     to: String,
///     subject: String,
///     body: String,
/// }
///
/// #[async_trait::async_trait]
/// impl Job<MyAppState> for EmailJob {
///     const NAME: &'static str = "EmailJob";
///
///     async fn run(&self, app_state: MyAppState) -> color_eyre::Result<()> {
///         // Send email logic here
///         println!("Sending email to {} with subject: {}", self.to, self.subject);
///         // You can access the database through app_state.db()
///         Ok(())
///     }
/// }
/// ```
///
/// # Enqueuing Jobs
///
/// ```rust
/// # use cja::jobs::Job;
/// # use serde::{Serialize, Deserialize};
/// # #[derive(Debug, Serialize, Deserialize, Clone)]
/// # struct EmailJob { to: String, subject: String, body: String }
/// # #[derive(Debug, Clone)]
/// # struct MyAppState { db: sqlx::PgPool }
/// # impl cja::app_state::AppState for MyAppState {
/// #     fn db(&self) -> &sqlx::PgPool { &self.db }
/// #     fn version(&self) -> &str { "1.0.0" }
/// #     fn cookie_key(&self) -> &cja::server::cookies::CookieKey { todo!() }
/// # }
/// # #[async_trait::async_trait]
/// # impl Job<MyAppState> for EmailJob {
/// #     const NAME: &'static str = "EmailJob";
/// #     async fn run(&self, _: MyAppState) -> color_eyre::Result<()> { Ok(()) }
/// # }
/// # async fn example(app_state: MyAppState) -> Result<(), cja::jobs::EnqueueError> {
/// let job = EmailJob {
///     to: "user@example.com".to_string(),
///     subject: "Welcome!".to_string(),
///     body: "Thank you for signing up!".to_string(),
/// };
///
/// // Enqueue the job with a context string for debugging
/// job.enqueue(app_state, "user-signup".to_string(), None).await?;
/// # Ok(())
/// # }
/// ```
///
/// # Job with Database Access
///
/// ```rust
/// use cja::jobs::Job;
/// use serde::{Serialize, Deserialize};
///
/// # #[derive(Debug, Clone)]
/// # struct MyAppState {
/// #     db: sqlx::PgPool,
/// # }
/// # impl cja::app_state::AppState for MyAppState {
/// #     fn db(&self) -> &sqlx::PgPool { &self.db }
/// #     fn version(&self) -> &str { "1.0.0" }
/// #     fn cookie_key(&self) -> &cja::server::cookies::CookieKey { todo!() }
/// # }
///
/// #[derive(Debug, Serialize, Deserialize, Clone)]
/// struct ProcessPaymentJob {
///     user_id: i32,
///     amount_cents: i64,
/// }
///
/// #[async_trait::async_trait]
/// impl Job<MyAppState> for ProcessPaymentJob {
///     const NAME: &'static str = "ProcessPaymentJob";
///
///     async fn run(&self, app_state: MyAppState) -> color_eyre::Result<()> {
///         use crate::cja::app_state::AppState;
///         use sqlx::Row;
///
///         // Access the database through app_state
///         let user = sqlx::query("SELECT name FROM users WHERE id = $1")
///             .bind(self.user_id)
///             .fetch_one(app_state.db())
///             .await?;
///
///         println!("Processing payment of {} cents for user {} #{}",
///                  self.amount_cents, user.get::<String, _>("name"), self.user_id);
///
///         sqlx::query("INSERT INTO payments (user_id, amount_cents) VALUES ($1, $2)")
///             .bind(self.user_id)
///             .bind(self.amount_cents)
///             .execute(app_state.db())
///             .await?;
///
///         Ok(())
///     }
/// }
/// ```
#[async_trait::async_trait]
pub trait Job<AppState: AS>:
    Serialize + DeserializeOwned + Send + Sync + std::fmt::Debug + Clone + 'static
{
    /// The unique name identifier for this job type.
    /// This is used for routing jobs to their handlers.
    const NAME: &'static str;

    /// Execute the job logic.
    ///
    /// This method has access to the full application state,
    /// including the database connection pool.
    ///
    /// If this method returns an error, the job will be automatically retried
    /// with exponential backoff until it succeeds or exceeds max retries.
    async fn run(&self, app_state: AppState) -> color_eyre::Result<()>;

    /// Execute the job logic with an optional cancellation token for graceful shutdown.
    ///
    /// Long-running jobs can override this method to check the cancellation token
    /// periodically and exit early during shutdown. The default implementation
    /// ignores the token and calls `run()`, so existing jobs continue to work
    /// without changes.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use cja::jobs::{Job, CancellationToken};
    /// # use serde::{Serialize, Deserialize};
    /// # #[derive(Debug, Serialize, Deserialize, Clone)]
    /// # struct LongJob { iterations: u32 }
    /// # #[derive(Debug, Clone)]
    /// # struct MyAppState { db: sqlx::PgPool }
    /// # impl cja::app_state::AppState for MyAppState {
    /// #     fn db(&self) -> &sqlx::PgPool { &self.db }
    /// #     fn version(&self) -> &str { "1.0.0" }
    /// #     fn cookie_key(&self) -> &cja::server::cookies::CookieKey { todo!() }
    /// # }
    /// #[async_trait::async_trait]
    /// impl Job<MyAppState> for LongJob {
    ///     const NAME: &'static str = "LongJob";
    ///
    ///     async fn run(&self, _app_state: MyAppState) -> color_eyre::Result<()> {
    ///         // Simple implementation without cancellation support
    ///         Ok(())
    ///     }
    ///
    ///     async fn run_with_cancellation(
    ///         &self,
    ///         app_state: MyAppState,
    ///         cancellation_token: CancellationToken,
    ///     ) -> color_eyre::Result<()> {
    ///         for i in 0..self.iterations {
    ///             // Check if shutdown was requested
    ///             if cancellation_token.is_cancelled() {
    ///                 tracing::info!("Job cancelled after {} iterations", i);
    ///                 return Err(color_eyre::eyre::eyre!("Job cancelled during shutdown"));
    ///             }
    ///
    ///             // Do some work
    ///             process_iteration(i, app_state.clone()).await?;
    ///         }
    ///         Ok(())
    ///     }
    /// }
    /// # async fn process_iteration(_i: u32, _app_state: MyAppState) -> color_eyre::Result<()> { Ok(()) }
    /// ```
    async fn run_with_cancellation(
        &self,
        app_state: AppState,
        _cancellation_token: CancellationToken,
    ) -> color_eyre::Result<()> {
        // Default implementation ignores cancellation token
        self.run(app_state).await
    }

    /// Internal method used by the job system to deserialize and run jobs.
    ///
    /// You typically won't call this directly - it's used by the job worker.
    #[instrument(name = "jobs.run_from_value", skip(app_state, cancellation_token), fields(job.name = Self::NAME), err)]
    async fn run_from_value(
        value: serde_json::Value,
        app_state: AppState,
        cancellation_token: CancellationToken,
    ) -> color_eyre::Result<()> {
        let job: Self = serde_json::from_value(value)?;

        job.run_with_cancellation(app_state, cancellation_token)
            .await
    }

    /// Enqueue this job for asynchronous execution.
    ///
    /// The job will be persisted to the database and picked up by a worker process.
    /// Jobs are executed with at-least-once semantics and automatic retries on failure.
    ///
    /// # Arguments
    /// * `app_state` - The application state containing the database connection
    /// * `context` - A string describing why this job was enqueued (useful for debugging)
    /// * `priority` - Optional priority for this job instance. Higher values run first.
    ///   Defaults to 0 if `None`. Use negative values for lower-priority background work.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use cja::jobs::Job;
    /// # use serde::{Serialize, Deserialize};
    /// # #[derive(Debug, Serialize, Deserialize, Clone)]
    /// # struct MyJob { data: String }
    /// # #[derive(Debug, Clone)]
    /// # struct MyAppState { db: sqlx::PgPool }
    /// # impl cja::app_state::AppState for MyAppState {
    /// #     fn db(&self) -> &sqlx::PgPool { &self.db }
    /// #     fn version(&self) -> &str { "1.0.0" }
    /// #     fn cookie_key(&self) -> &cja::server::cookies::CookieKey { todo!() }
    /// # }
    /// # #[async_trait::async_trait]
    /// # impl Job<MyAppState> for MyJob {
    /// #     const NAME: &'static str = "MyJob";
    /// #     async fn run(&self, _: MyAppState) -> color_eyre::Result<()> { Ok(()) }
    /// # }
    /// # async fn example(app_state: MyAppState) -> Result<(), cja::jobs::EnqueueError> {
    /// let job = MyJob { data: "important work".to_string() };
    /// // Default priority
    /// job.clone().enqueue(app_state.clone(), "user-requested".to_string(), None).await?;
    /// // Low priority background work
    /// job.enqueue(app_state, "background-cleanup".to_string(), Some(-10)).await?;
    /// # Ok(())
    /// # }
    /// ```
    #[instrument(name = "jobs.enqueue", skip(app_state), fields(job.name = Self::NAME), err)]
    async fn enqueue(
        self,
        app_state: AppState,
        context: String,
        priority: Option<i32>,
    ) -> Result<(), EnqueueError> {
        sqlx::query(
            "
        INSERT INTO jobs (job_id, name, payload, priority, run_at, created_at, context)
        VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(uuid::Uuid::new_v4())
        .bind(Self::NAME)
        .bind(serde_json::to_value(self)?)
        .bind(priority.unwrap_or(0))
        .bind(chrono::Utc::now())
        .bind(chrono::Utc::now())
        .bind(context)
        .execute(app_state.db())
        .await?;

        Ok(())
    }
}

pub mod worker;
