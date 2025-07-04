use crate::app_state::AppState as AS;
use serde::{Serialize, de::DeserializeOwned};
use thiserror::Error;
use tracing::instrument;

pub mod registry;

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
/// job.enqueue(app_state, "user-signup".to_string()).await?;
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
    async fn run(&self, app_state: AppState) -> color_eyre::Result<()>;

    /// Internal method used by the job system to deserialize and run jobs.
    ///
    /// You typically won't call this directly - it's used by the job worker.
    #[instrument(name = "jobs.run_from_value", skip(app_state), fields(job.name = Self::NAME), err)]
    async fn run_from_value(
        value: serde_json::Value,
        app_state: AppState,
    ) -> color_eyre::Result<()> {
        let job: Self = serde_json::from_value(value)?;

        job.run(app_state).await
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
    /// job.enqueue(app_state, "user-requested".to_string()).await?;
    /// # Ok(())
    /// # }
    /// ```
    #[instrument(name = "jobs.enqueue", skip(app_state), fields(job.name = Self::NAME), err)]
    async fn enqueue(self, app_state: AppState, context: String) -> Result<(), EnqueueError> {
        sqlx::query(
            "
        INSERT INTO jobs (job_id, name, payload, priority, run_at, created_at, context)
        VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(uuid::Uuid::new_v4())
        .bind(Self::NAME)
        .bind(serde_json::to_value(self)?)
        .bind(0)
        .bind(chrono::Utc::now())
        .bind(chrono::Utc::now())
        .bind(context)
        .execute(app_state.db())
        .await?;

        Ok(())
    }
}

pub mod worker;
