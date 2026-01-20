use crate::app_state::AppState as AS;
use serde::{Serialize, de::DeserializeOwned};
use thiserror::Error;
use tracing::instrument;

pub mod registry;

pub use tokio_util::sync::CancellationToken;
pub use worker::{DEFAULT_LOCK_TIMEOUT, DEFAULT_MAX_RETRIES};

#[derive(Debug, Error)]
pub enum EnqueueError {
    #[error("DbError: {0}")]
    DbError(#[from] tokio_postgres::Error),
    #[error("PoolError: {0}")]
    PoolError(String),
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
/// - Jobs are permanently deleted after exceeding the configured max retries (default: 20)
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
/// #     db: cja::app_state::DbPool,
/// # }
/// # impl cja::app_state::AppState for MyAppState {
/// #     fn db(&self) -> &cja::app_state::DbPool { &self.db }
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
/// # struct MyAppState { db: cja::app_state::DbPool }
/// # impl cja::app_state::AppState for MyAppState {
/// #     fn db(&self) -> &cja::app_state::DbPool { &self.db }
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
/// job.enqueue(app_state, "user-signup".to_string()).await?;
/// # Ok(())
/// # }
/// ```
///
/// # Job with Database Access
///
/// ```rust,no_run
/// use cja::jobs::Job;
/// use cja::app_state::AppState;
/// use serde::{Serialize, Deserialize};
///
/// # #[derive(Debug, Clone)]
/// # struct MyAppState {
/// #     db: cja::app_state::DbPool,
/// # }
/// # impl cja::app_state::AppState for MyAppState {
/// #     fn db(&self) -> &cja::app_state::DbPool { &self.db }
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
///         // Access the database through app_state
///         let client = app_state.db().get().await
///             .map_err(|e| color_eyre::eyre::eyre!("Pool error: {e}"))?;
///
///         let user = client.query_one(
///             "SELECT name FROM users WHERE id = $1",
///             &[&self.user_id]
///         ).await?;
///         let name: String = user.get(0);
///
///         println!("Processing payment of {} cents for user {} #{}",
///                  self.amount_cents, name, self.user_id);
///
///         client.execute(
///             "INSERT INTO payments (user_id, amount_cents) VALUES ($1, $2)",
///             &[&self.user_id, &self.amount_cents]
///         ).await?;
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
    /// # struct MyAppState { db: cja::app_state::DbPool }
    /// # impl cja::app_state::AppState for MyAppState {
    /// #     fn db(&self) -> &cja::app_state::DbPool { &self.db }
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
    ///
    /// # Example
    ///
    /// ```rust
    /// # use cja::jobs::Job;
    /// # use serde::{Serialize, Deserialize};
    /// # #[derive(Debug, Serialize, Deserialize, Clone)]
    /// # struct MyJob { data: String }
    /// # #[derive(Debug, Clone)]
    /// # struct MyAppState { db: cja::app_state::DbPool }
    /// # impl cja::app_state::AppState for MyAppState {
    /// #     fn db(&self) -> &cja::app_state::DbPool { &self.db }
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
    /// job.enqueue(app_state, "user-requested".to_string()).await?;
    /// # Ok(())
    /// # }
    /// ```
    #[instrument(name = "jobs.enqueue", skip(app_state), fields(job.name = Self::NAME), err)]
    async fn enqueue(self, app_state: AppState, context: String) -> Result<(), EnqueueError> {
        let client = app_state
            .db()
            .get()
            .await
            .map_err(|e| EnqueueError::PoolError(e.to_string()))?;

        let now = chrono::Utc::now();
        sql_check_macros::query!(
            "INSERT INTO jobs (job_id, name, payload, priority, run_at, created_at, context)
            VALUES ($1, $2, $3, $4, $5, $6, $7)",
            uuid::Uuid::new_v4(),
            Self::NAME,
            serde_json::to_value(self)?,
            0i32,
            now,
            now,
            context
        )
        .execute(&client)
        .await?;

        Ok(())
    }
}

pub mod worker;
