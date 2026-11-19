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
//! | 1 | 2 seconds |
//! | 2 | 4 seconds |
//! | 5 | 32 seconds |
//! | 10 | ~17 minutes |
//! | 20 | ~12 days |
//!
//! With `max_retries = N`, a job can execute N+1 times. Returned errors,
//! caught handler panics, expired leases, and bodies that exceed
//! [`Job::MAX_RUNTIME`] each count as a failed attempt.
//! A [`JobYield`] error releases the row uncharged; an unknown job name is
//! released uncharged on an age-scaled delay (30s for the first hour of the
//! row's life, hourly after that, dead-letter past 14 days); an
//! undeserializable payload is dead-lettered on first dispatch. The final
//! incremented count is stored in `dead_letter_jobs`.
//!
//! # Maximum Runtime
//!
//! Every job attempt runs under a per-job-type wall-clock limit:
//! [`Job::MAX_RUNTIME`], defaulting to [`DEFAULT_MAX_RUNTIME`] (15 minutes).
//! A job type whose healthy attempts run longer overrides it with a larger
//! `Duration` (e.g. `const MAX_RUNTIME: Duration =
//! Duration::from_secs(2 * 60 * 60);`). On expiry, the worker drops the body
//! future, stops heartbeating, releases the lease, and records a retryable
//! failure whose message names the limit; each retry gets a fresh budget.
//! The timeout cancels **async** work only — work already inside
//! `spawn_blocking` keeps running after the drop, so jobs that hand work to
//! blocking threads must bound it themselves. Move the attempt's concurrency
//! `slot` (see [`Job::run_with_cancellation`]) into the `spawn_blocking`
//! closure to hold the concurrency budget until that work ends.
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
//! database level. Each worker process also tracks running counts per job
//! type: a type declaring `Job::CONCURRENCY = PerProcess(n)` is excluded
//! from claims while at its cap, and types sharing a
//! `Concurrency::PerProcessShared` key are excluded while the shared
//! budget is saturated. A saturated scope never blocks other types.
//!
//! # Per-Process Concurrency
//!
//! A job type can declare `CONCURRENCY = Concurrency::PerProcess(n)` to cap
//! concurrent attempts, or `Concurrency::PerProcessShared { key, limit }` to
//! share a named budget across job types — every type with the same `key`
//! (and `limit`; conflicting limits fail worker startup) counts against one
//! in-process counter. The cap is per worker process (in-memory, shared by
//! all `job_worker` tasks via `JobWorkerConfig::type_concurrency`,
//! process-shared by default). Saturated scopes are skipped at claim
//! (`name <> ALL(...)`), so a worker claims the next runnable job of another
//! type. A raced claim is released uncharged via [`JobYield`], and a slot is
//! released when the attempt finalizes (success, failure,
//! [`Job::MAX_RUNTIME`] expiry, yield, lease loss, shutdown drain). A body
//! can also move its `slot` into `spawn_blocking` to hold the budget until
//! that blocking work ends. The cap bounds in-process resources only — for
//! outside resources (APIs, quotas), the effective limit is cap × instances.
//!
//! # Cancellation Support
//!
//! Long-running jobs can override [`Job::run_with_cancellation`] to exit
//! gracefully during shutdown by checking the cancellation token periodically.
//! A cooperative job should return `Err(JobYield::new("shutdown").into())`
//! when the token fires, so the worker releases its row without charging a
//! failure.
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
//! | `locked_at` | TIMESTAMPTZ | Last successful heartbeat (claim time initially) |
//! | `locked_by` | TEXT | Worker UUID string used as the exact ownership key |
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

use std::num::NonZeroUsize;
use std::time::Duration;

use crate::app_state::AppState as AS;
use jiff_sqlx::ToSqlx as _;
use serde::{Serialize, de::DeserializeOwned};
use thiserror::Error;
use tracing::{Instrument, instrument};

pub(crate) fn enqueue_span<AppState: AS, J: Job<AppState>>(
    job: &J,
    context: &str,
    priority: Option<i32>,
    created_at: jiff::Timestamp,
    job_id: uuid::Uuid,
) -> tracing::Span {
    tracing::info_span!(
        "jobs.enqueue",
        job.id = %job_id,
        job.name = J::NAME,
        job.context = %context,
        job.priority = priority.unwrap_or(0),
        job.created_at = %created_at,
        job.run_at = %created_at,
        "self" = ?job,
        context = %context,
        priority = ?priority,
    )
}

pub(crate) async fn enqueue_on_connection<AppState: AS, J: Job<AppState>>(
    conn: &mut sqlx::PgConnection,
    job: J,
    context: &str,
    priority: Option<i32>,
    created_at: jiff::Timestamp,
    job_id: uuid::Uuid,
) -> Result<uuid::Uuid, EnqueueError> {
    sqlx::query(
        "INSERT INTO jobs (job_id, name, payload, priority, run_at, created_at, context)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(job_id)
    .bind(J::NAME)
    .bind(serde_json::to_value(job)?)
    .bind(priority.unwrap_or(0))
    .bind(created_at.to_sqlx())
    .bind(created_at.to_sqlx())
    .bind(context)
    .execute(conn)
    .await?;
    Ok(job_id)
}

pub(crate) fn enqueue_receipt<JName: AsRef<str>>(job_id: uuid::Uuid, name: JName) {
    tracing::info!(event_type = "job_enqueued", job_id = %job_id, job.name = name.as_ref(), "Job enqueued");
}

pub(crate) fn enqueue_failure(job_id: uuid::Uuid, error: &EnqueueError) {
    tracing::error!(job_id = %job_id, %error, "Job enqueue failed");
}

/// The shared enqueue core: the single INSERT plus its receipt (or failure
/// event). The caller wraps this in its `jobs.enqueue` span, so span
/// construction (`enqueue_span`), the INSERT (`enqueue_on_connection`),
/// and the receipt/failure events each live in exactly one place.
async fn enqueue_core<AppState: AS, J: Job<AppState>>(
    conn: &mut sqlx::PgConnection,
    job: J,
    context: &str,
    priority: Option<i32>,
    created_at: jiff::Timestamp,
    job_id: uuid::Uuid,
) -> Result<uuid::Uuid, EnqueueError> {
    let result =
        enqueue_on_connection::<AppState, J>(conn, job, context, priority, created_at, job_id)
            .await;
    match &result {
        Ok(id) => {
            enqueue_receipt(*id, J::NAME);
        }
        Err(error) => enqueue_failure(job_id, error),
    }
    result
}

pub mod registry;

pub use tokio_util::sync::CancellationToken;
pub use worker::{
    ConcurrencyScope, ConcurrencySlot, ConcurrencyTracker, DEFAULT_HEARTBEAT_INTERVAL,
    DEFAULT_MAX_RETRIES, DEFAULT_RECLAIM_WINDOW, IdlePollGate, JobWorkerConfig,
};

/// Default maximum runtime per job attempt ([`Job::MAX_RUNTIME`]).
///
/// 15 minutes is ~30 default heartbeat intervals and 3× the 5-minute stopgap
/// arena shipped for its wedged backups (BattlesnakeOfficial/arena#341): long
/// enough that no healthy job notices, short enough that a hung body releases
/// its worker slot and lease in bounded time. Renders as `900s` in
/// [`crate::jobs::JobMaxRuntimeExceeded`]'s message (`Duration`'s `Debug`).
pub const DEFAULT_MAX_RUNTIME: Duration = Duration::from_mins(15);

#[derive(Debug, Error)]
pub enum EnqueueError {
    #[error("SqlxError: {0}")]
    SqlxError(#[from] sqlx::Error),
    #[error("SerdeJsonError: {0}")]
    SerdeJsonError(#[from] serde_json::Error),
}

/// A distinguished job outcome: release the row without charging a failure.
///
/// Return this from [`Job::run`] or [`Job::run_with_cancellation`] — almost
/// always because the cancellation token fired during shutdown — and the
/// worker releases the row's lock while leaving `error_count`, `run_at`, and
/// the error columns untouched. The job is immediately claimable again
/// (typically by a post-deploy worker).
///
/// Returning `Ok` instead would DELETE the row with the work unfinished;
/// returning any other `Err` charges a failure attempt and grows the backoff.
/// A job that yields without checking a token will be re-claimed immediately
/// and spin — check the token before yielding.
#[derive(Debug, Error)]
#[error("job yielded: {reason}")]
pub struct JobYield {
    /// Why the job yielded (logged only; never persisted).
    pub reason: String,
}

impl JobYield {
    /// Create a yield with a reason.
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

/// The claimed job's `name` is not in this binary's registry — typically a
/// newer binary enqueued a job type this one predates. Produced by
/// [`crate::impl_job_registry!`]; the worker releases the row uncharged on
/// an age-scaled delay — 30s for the first hour of the row's life, hourly
/// after that — so a binary that knows the name can claim it soon, and
/// dead-letters the row past 14 days. Hand-written `JobRegistry` impls
/// should return this from their fallback arm to get the same treatment.
#[derive(Debug, Error)]
#[error("Unknown job type: {0}")]
pub struct UnknownJobType(pub String);

/// The stored payload cannot be deserialized into the job type. Produced by
/// [`Job::run_from_value`]; a payload that fails to parse will never parse,
/// so the worker moves the row straight to `dead_letter_jobs`.
#[derive(Debug, Error)]
#[error("job payload is not deserializable")]
pub struct InvalidJobPayload(#[from] serde_json::Error);

/// A single job attempt exceeded [`Job::MAX_RUNTIME`] and was dropped by the
/// worker. Charged like any other failure: the attempt increments
/// `error_count`, takes the normal exponential backoff, and eventually
/// dead-letters. Dropping the future cancels async work only — work already
/// inside `spawn_blocking` keeps running.
///
/// The message renders the limit via `Duration`'s `Debug` (`900s` at the
/// 15-minute default, `2s` for a 2-second override).
#[derive(Debug, Error)]
#[error("job exceeded its max runtime of {limit:?}")]
pub struct JobMaxRuntimeExceeded {
    /// The `Job::MAX_RUNTIME` that expired.
    pub limit: Duration,
}

/// A job type's declared concurrency limit, enforced by the worker at claim
/// time. See [`Job::CONCURRENCY`].
///
/// This enum is `non_exhaustive`: a cross-instance (`Global`) or rate-limit
/// variant may be added later.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Concurrency {
    /// No limit (the default): any number of attempts of this type may run
    /// concurrently in this process.
    Unlimited,
    /// At most `N` attempts of this job type run concurrently **per worker
    /// process**. All `job_worker` tasks in the process share one counter
    /// (via [`crate::jobs::worker::JobWorkerConfig::type_concurrency`],
    /// process-shared by default), so the cap holds across every worker the
    /// process spawns.
    ///
    /// This bounds **in-process** resources (CPU, memory, blocking-pool
    /// threads an attempt occupies). It is NOT a ticket for outside
    /// resources: with M processes (instances), the effective limit on an
    /// external API or quota is `N x M`, since each process counts only its
    /// own attempts. Use it for semaphore-style in-process limits, not to
    /// protect a rate-limited third-party API.
    ///
    /// Saturated types are excluded from the worker's claim query, so
    /// workers claim the next runnable job of another type instead of
    /// parking. Combined with [`Job::MAX_RUNTIME`], a hung attempt releases
    /// its slot in bounded time.
    PerProcess(NonZeroUsize),
    /// At most `limit` attempts run concurrently per worker process across
    /// **every job type declaring the same `key`**: one shared in-process
    /// counter per key, tracked through
    /// [`crate::jobs::worker::JobWorkerConfig::type_concurrency`] like
    /// [`Concurrency::PerProcess`]. Job types contending for one resource
    /// — e.g., Arena's archive types sharing a backup slot on a 1-vCPU box
    /// — share a budget by declaring the same `key` and `limit`.
    ///
    /// Every type sharing a key MUST declare the same `limit`: conflicting
    /// limits fail worker startup instead of being silently min'd or
    /// max'd. Validation runs per registry at worker build; conflicting
    /// limits across two registries in one process sharing the
    /// process-shared tracker are not detected.
    /// Shared keys and [`Concurrency::PerProcess`] scopes use separate
    /// namespaces: a key matching a job type name has a separate budget
    /// from that type's per-type scope.
    ///
    /// Enforced like [`Concurrency::PerProcess`]: types with a saturated
    /// key are excluded from the claim query (so the worker claims an
    /// unrelated runnable job instead of parking), raced claims release
    /// uncharged via [`JobYield`], and the slot is held for the whole
    /// attempt. The cap bounds **in-process** resources only — for
    /// external resources (APIs, quotas), the effective limit is
    /// `limit x instances`.
    ///
    /// A body can hold the shared budget across blocking work that
    /// outlives its attempt — e.g., preventing the next archive from
    /// starting while CPU work from a `MAX_RUNTIME`-expired attempt
    /// finishes: override [`Job::run_with_cancellation`] and move `slot`
    /// into the `spawn_blocking` closure (`let _held = slot;` at the top).
    /// The budget stays held until the closure exits. Slot-holding
    /// blocking work must bound itself with its own timeout: an unbounded
    /// closure saturates the key until process exit — fail-closed, never
    /// fail-open.
    PerProcessShared {
        /// Shared budget name: job types declaring the same `key` (and
        /// `limit`) share one in-process counter.
        key: &'static str,
        /// The maximum concurrent attempts across all types declaring
        /// `key`.
        limit: NonZeroUsize,
    },
}

impl Concurrency {
    /// The in-process scope and limit this declaration counts within, or
    /// `None` for `Unlimited`. `PerProcess` scopes by type name;
    /// `PerProcessShared` scopes by its key. The two scope kinds are
    /// separate namespaces: a shared key spelled like a job type name is
    /// a different budget from that type's implicit per-type scope.
    pub(crate) fn scope_and_limit(
        &self,
        type_name: &str,
    ) -> Option<(crate::jobs::worker::ConcurrencyScope, NonZeroUsize)> {
        match self {
            Self::Unlimited => None,
            Self::PerProcess(limit) => Some((
                crate::jobs::worker::ConcurrencyScope::Type(type_name.to_owned()),
                *limit,
            )),
            Self::PerProcessShared { key, limit } => Some((
                crate::jobs::worker::ConcurrencyScope::Shared((*key).to_owned()),
                *limit,
            )),
        }
    }
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
/// completed. The lease reclaim window handles this:
/// - New workers heartbeat active jobs every 30 seconds by default
/// - A missing heartbeat makes the job claimable after 120 seconds by default
/// - Every expired lease counts as a failed attempt
/// - This ensures jobs are eventually processed even after worker failures
///
/// A crashed worker is the lease-based case; a body that is alive but stuck
/// (the process keeps heartbeating while the job never progresses) is bounded
/// by [`Job::MAX_RUNTIME`] instead — see "Maximum Runtime".
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

    /// Maximum wall-clock time a single attempt of this job may run before
    /// the worker drops the body future. Defaults to
    /// [`DEFAULT_MAX_RUNTIME`] (15 minutes). Job types whose healthy
    /// attempts take longer can override it with a larger `Duration`, e.g.
    /// `const MAX_RUNTIME: Duration = Duration::from_secs(2 * 60 * 60);`.
    ///
    /// On expiry, the worker drops the future, stops heartbeating, releases
    /// the lease, and charges the attempt as a normal retryable failure
    /// (the error message names the limit). Each retry gets a fresh budget.
    ///
    /// # Caveat
    ///
    /// The limit cancels **async** work only. Work running inside
    /// `tokio::task::spawn_blocking` (or a detached `tokio::spawn`) continues
    /// after the job future is dropped. Jobs that hand work to blocking
    /// threads must bound it themselves (e.g. with their own timeout around
    /// the blocking call) if it must not outlive the attempt.
    const MAX_RUNTIME: Duration = DEFAULT_MAX_RUNTIME;

    /// Maximum concurrent attempts of this job type in one worker process.
    /// Defaults to [`Concurrency::Unlimited`].
    ///
    /// Enforced at claim time: a worker in a process already running
    /// `PerProcess(n)` attempts of this type will not claim another row of
    /// this type (claiming the next runnable job of another type instead),
    /// and a raced claim is released uncharged. The slot is held for the
    /// whole attempt — body plus finalization — and released on success,
    /// failure, [`Job::MAX_RUNTIME`] expiry, yield, lease loss, and shutdown
    /// drain.
    ///
    /// `Concurrency::PerProcessShared { key, limit }` instead caps the
    /// **combined** attempts of all job types declaring the same `key`
    /// (they must declare the same `limit`; conflicts fail worker
    /// startup) — one named budget for several types contending for a
    /// single resource. See that variant's docs for the full story,
    /// including holding the budget across blocking work that outlives
    /// the attempt by moving this attempt's `slot` (the
    /// `run_with_cancellation` parameter) into the `spawn_blocking`
    /// closure.
    ///
    /// # Caveat
    ///
    /// Per **process**: bounds in-process resources only. The effective limit
    /// on an outside resource (API, quota) is the cap times your instance
    /// count. Work already in `spawn_blocking` is not cancelled when an
    /// attempt ends; the count tracks attempts, not OS threads, so blocking
    /// work that outlives its attempt (see `MAX_RUNTIME`'s caveat) is
    /// invisible to the counter — unless the body moves its `slot` into the
    /// closure, holding the budget until the closure exits.
    const CONCURRENCY: Concurrency = Concurrency::Unlimited;

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
    /// `slot` is the worker's handle on this attempt's concurrency budget
    /// ([`Job::CONCURRENCY`]). The worker keeps its own handle for the whole
    /// attempt, so the default implementation — and any override that ignores
    /// `slot` — is safe: dropping this clone early releases nothing. A body
    /// that hands work to `spawn_blocking` can move `slot` into the closure
    /// (`let _held = slot;` at the top) to hold the budget until that blocking
    /// work ends — for a shared key, this is how the next archive waits out
    /// the CPU work of an attempt whose `MAX_RUNTIME` expired.
    ///
    /// When the token fires, a job body has three options:
    ///
    /// - Return `Ok(())` — the worker DELETEs the row, treating the unfinished
    ///   work as complete. Only correct if the shutdown path finished the work
    ///   or made it moot.
    /// - Return a plain `Err` — the worker charges a failure attempt
    ///   (`error_count + 1`, exponential backoff, eventual dead-letter). This
    ///   is the normal behavior for real errors.
    /// - Return `Err(JobYield::new(...).into())` — the worker releases the
    ///   row's lock without charging anything, leaving `error_count` and
    ///   `run_at` untouched so a post-deploy worker can claim it at once.
    ///   This is what a cooperative body should return on shutdown when the
    ///   work is incomplete.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use cja::jobs::{Job, CancellationToken, JobYield};
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
    ///         _slot: cja::jobs::ConcurrencySlot,
    ///     ) -> color_eyre::Result<()> {
    ///         for i in 0..self.iterations {
    ///             // Check if shutdown was requested
    ///             if cancellation_token.is_cancelled() {
    ///                 tracing::info!("Job cancelled after {} iterations", i);
    ///                 // Release the row uncharged; a post-deploy worker
    ///                 // picks the job back up.
    ///                 return Err(JobYield::new("cancelled during shutdown").into());
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
        _slot: crate::jobs::worker::ConcurrencySlot,
    ) -> color_eyre::Result<()> {
        // Default implementation ignores the cancellation token AND the
        // slot: the worker holds its own handle for the whole attempt, so
        // dropping this clone early is safe. Override and move `slot`
        // into `spawn_blocking` to hold your budget for blocking work that
        // outlives the attempt.
        self.run(app_state).await
    }

    /// Internal method used by the job system to deserialize and run jobs.
    ///
    /// You typically won't call this directly - it's used by the job worker.
    /// A payload that fails to deserialize is wrapped in [`InvalidJobPayload`]
    /// (its `serde_json::Error` rides along as the source); the worker
    /// dead-letters such rows on the first dispatch instead of retrying.
    #[instrument(name = "jobs.run_from_value", skip(app_state, cancellation_token, slot), fields(job.name = Self::NAME), err)]
    async fn run_from_value(
        value: serde_json::Value,
        app_state: AppState,
        cancellation_token: CancellationToken,
        slot: crate::jobs::worker::ConcurrencySlot,
    ) -> color_eyre::Result<()> {
        let job: Self = serde_json::from_value(value).map_err(InvalidJobPayload)?;

        job.run_with_cancellation(app_state, cancellation_token, slot)
            .await
    }

    /// Enqueue this job on a caller-owned connection — for example inside a
    /// caller-managed transaction, so the job row commits atomically with the
    /// caller's own writes.
    ///
    /// Creates the same `jobs.enqueue` span and emits the same
    /// `enqueue_receipt` as [`Job::enqueue`], and performs the same single
    /// INSERT. The receipt (and the span) mark the INSERT, not the commit:
    /// if the caller's transaction rolls back afterward, the receipt
    /// remains in telemetry. It does NOT call [`crate::app_state::AppState::job_enqueued`]:
    /// that is not a transaction-commit hook. The caller MUST call
    /// `state.job_enqueued()` on its `AppState` after its transaction
    /// commits, so an idle worker is woken for the new row.
    async fn enqueue_on(
        self,
        conn: &mut sqlx::PgConnection,
        context: String,
        priority: Option<i32>,
    ) -> Result<uuid::Uuid, EnqueueError> {
        let job_id = uuid::Uuid::new_v4();
        let created_at = jiff::Timestamp::now();
        // Eyes captures fields at span creation; same constructor as `enqueue`
        // so an enqueue stays linkable to its worker attempts.
        let span = enqueue_span::<AppState, Self>(&self, &context, priority, created_at, job_id);
        enqueue_core::<AppState, Self>(conn, self, &context, priority, created_at, job_id)
            .instrument(span)
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
    async fn enqueue(
        self,
        app_state: AppState,
        context: String,
        priority: Option<i32>,
    ) -> Result<(), EnqueueError> {
        let job_id = uuid::Uuid::new_v4();
        let created_at = jiff::Timestamp::now();
        // Same span (same field list) as `enqueue_on`: a pool-acquire
        // failure is an enqueue failure and is recorded inside the span
        // through `enqueue_failure`, exactly like an insert failure — the
        // pre-refactor behavior. (sqlx does not log pool-acquire errors
        // itself: `PoolTimedOut`/`PoolClosed` surface only as the returned
        // error.)
        let span = enqueue_span::<AppState, Self>(&self, &context, priority, created_at, job_id);
        async move {
            let mut conn = match app_state.db().acquire().await {
                Ok(conn) => conn,
                Err(error) => {
                    let error = EnqueueError::from(error);
                    enqueue_failure(job_id, &error);
                    return Err(error);
                }
            };
            enqueue_core::<AppState, Self>(&mut conn, self, &context, priority, created_at, job_id)
                .await
                .map(|_| ())?;
            // Return the connection to the pool before waking a worker —
            // exact parity with main's enqueue, which releases `conn` at
            // the end of its inner async block before `job_enqueued()`.
            drop(conn);
            app_state.job_enqueued();
            Ok(())
        }
        .instrument(span)
        .await
    }
}

pub mod worker;

#[cfg(test)]
mod enqueue_on_tests {
    use super::*;
    use crate::app_state::AppState;
    use crate::server::cookies::CookieKey;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::{Layer, layer::Context, prelude::*};

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

    #[derive(Debug, Clone, Serialize, serde::Deserialize)]
    struct EnqueueOnJob {
        value: u32,
    }

    #[async_trait::async_trait]
    impl Job<TestAppState> for EnqueueOnJob {
        const NAME: &'static str = "EnqueueOnJob";

        async fn run(&self, _app_state: TestAppState) -> color_eyre::Result<()> {
            Ok(())
        }
    }

    #[derive(Clone)]
    struct RawSpans(Arc<Mutex<Vec<String>>>);
    struct DebugVisitor<'a>(&'a mut Vec<String>);
    impl tracing::field::Visit for DebugVisitor<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0.push(format!("{}={value:?}", field.name()));
        }
    }
    impl<S: tracing::Subscriber> Layer<S> for RawSpans {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            _id: &tracing::span::Id,
            _ctx: Context<'_, S>,
        ) {
            attrs.record(&mut DebugVisitor(&mut self.0.lock().unwrap()));
        }
    }

    #[derive(Debug, Clone, Serialize, serde::Deserialize)]
    struct EmailJob {
        recipient_override: String,
    }
    #[async_trait::async_trait]
    impl Job<TestAppState> for EmailJob {
        const NAME: &'static str = "EmailJob";
        async fn run(&self, _app_state: TestAppState) -> color_eyre::Result<()> {
            Ok(())
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_enqueue_span_keeps_raw_sibling_but_drops_debug_from_posthog() {
        let captures = Arc::new(Mutex::new(Vec::<(String, Vec<u8>)>::new()));
        let requests_for_server = captures.clone();
        let app =
            axum::Router::new().fallback(move |uri: axum::http::Uri, body: axum::body::Bytes| {
                let requests = requests_for_server.clone();
                async move {
                    requests
                        .lock()
                        .unwrap()
                        .push((uri.path().into(), body.to_vec()));
                    axum::http::StatusCode::OK
                }
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let sink = crate::setup::posthog_test_support::PostHogSink::build(
            &crate::setup::posthog_test_support::PostHogConfig::new_for_tests(
                host,
                "phc_test",
                tracing_subscriber::filter::LevelFilter::TRACE,
                1.0,
            ),
            "jobs-test",
            None,
            None,
            8192,
        )
        .unwrap();
        let raw = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry()
            .with(
                tracing_opentelemetry::layer()
                    .with_tracer(sink.tracer())
                    .with_filter(sink.filter()),
            )
            .with(sink.log_bridge().with_filter(sink.filter()))
            .with(RawSpans(raw.clone()));
        tracing::subscriber::with_default(subscriber, || {
            let job = EmailJob {
                recipient_override: "synthetic@example.test".into(),
            };
            let span = enqueue_span::<TestAppState, EmailJob>(
                &job,
                "test-context",
                Some(7),
                jiff::Timestamp::now(),
                uuid::Uuid::nil(),
            );
            let _entered = span.enter();
            tracing::info!(target: "fixture", "job-event");
        });
        sink.shutdown_handle().shutdown().await;
        let raw = raw.lock().unwrap().join(" ");
        assert!(
            raw.contains("synthetic@example.test") && raw.contains("self="),
            "{raw}"
        );
        let captures = captures.lock().unwrap();
        for path in ["/i/v1/traces", "/i/v1/logs"] {
            let body = captures
                .iter()
                .filter(|(p, _)| p == path)
                .flat_map(|(_, b)| b.iter().copied())
                .collect::<Vec<_>>();
            let text = String::from_utf8_lossy(&body);
            assert!(
                text.contains("job.id")
                    && text.contains("job.name")
                    && text.contains("job.context")
                    && text.contains("job.priority"),
                "{path}"
            );
            assert!(
                !text.contains("synthetic@example.test") && !text.contains("recipient_override"),
                "{path}: leaked job Debug"
            );
        }
        server.abort();
    }

    #[sqlx::test]
    async fn enqueue_on_is_transaction_scoped(db: sqlx::PgPool) {
        let state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };
        let mut tx = state.db().begin().await.unwrap();

        let job_id = EnqueueOnJob { value: 7 }
            .enqueue_on(&mut tx, "tx-test".to_string(), None)
            .await
            .unwrap();

        // Outside the transaction: nothing committed yet.
        let before: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE job_id = $1")
            .bind(job_id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(
            before, 0,
            "job row must not be visible before the transaction commits"
        );

        tx.commit().await.unwrap();

        let after: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE job_id = $1")
            .bind(job_id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(
            after, 1,
            "job row must be committed with the caller's transaction"
        );
    }
}
