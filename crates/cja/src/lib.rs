#![doc(html_favicon_url = "https://cja.app/docs/favicon.svg")]
//! # CJA — Cron, Jobs and Axum
//!
//! > **ALPHA**: CJA is under active development. APIs may change between releases.
//!
//! A Rust web framework that combines background job processing, cron scheduling,
//! and an HTTP server built on [Axum](https://docs.rs/axum).
//!
//! ## Overview
//!
//! CJA provides three integrated subsystems:
//!
//! - **[`jobs`]** — PostgreSQL-backed background job queue with automatic retries,
//!   exponential backoff, dead letter queue, and graceful shutdown
//! - **[`cron`]** — Scheduled task execution with both interval-based and cron expression
//!   scheduling, timezone support
//! - **[`server`]** — HTTP server on Axum with encrypted cookie management, database-backed
//!   sessions, and zero-downtime reload via systemfd
//!
//! All three share a single [`app_state::AppState`] and coordinate shutdown via
//! [`tokio_util::sync::CancellationToken`].
//!
//! ## Getting Started
//!
//! Add CJA to your project:
//!
//! ```bash
//! cargo add cja
//! ```
//!
//! ### 1. Implement `AppState`
//!
//! ```rust
//! use cja::app_state::AppState;
//! use cja::server::cookies::CookieKey;
//!
//! #[derive(Clone)]
//! struct MyAppState {
//!     db: sqlx::PgPool,
//!     cookie_key: CookieKey,
//! }
//!
//! impl AppState for MyAppState {
//!     fn version(&self) -> &str { "1.0.0" }
//!     fn db(&self) -> &sqlx::PgPool { &self.db }
//!     fn cookie_key(&self) -> &CookieKey { &self.cookie_key }
//! }
//! ```
//!
//! [`app_state::AppState`] is cloned per request — wrap expensive resources in `Arc`.
//!
//! ### 2. Set Up the Database
//!
//! CJA requires `PostgreSQL`. Set `DATABASE_URL` and run migrations on startup:
//!
//! ```rust,ignore
//! let pool = sqlx::postgres::PgPoolOptions::new()
//!     .max_connections(5)
//!     .connect(&std::env::var("DATABASE_URL")?)
//!     .await?;
//!
//! // Run CJA's built-in migrations (jobs, crons, cron_pauses, sessions, dead_letter_jobs)
//! cja::db::run_migrations(&pool).await?;
//! ```
//!
//! ### 3. Wire Up `main()`
//!
//! ```rust,ignore
//! use cja::setup::setup_tracing;
//! use cja::tasks::{ShutdownBudget, Supervisor};
//!
//! fn main() -> cja::Result<()> {
//!     tokio::runtime::Builder::new_multi_thread()
//!         .enable_all()
//!         .build()?
//!         .block_on(run())
//! }
//!
//! async fn run() -> cja::Result<()> {
//!     // Flushes Eyes and PostHog when shutdown() is called below
//!     let telemetry = setup_tracing("my-app")?;
//!     let result = run_tasks().await;
//!     if let Err(error) = &result {
//!         tracing::error!(error = %format_args!("{error:#}"), "application tasks failed");
//!     }
//!     let flush = telemetry.shutdown().await;
//!     if result.is_ok() { flush?; }
//!     result
//! }
//!
//! // For platform-specific stdout, load the Eyes token before setup and
//! // configure its local filter on the stdout layer. This filter does not
//! // restrict Eyes, PostHog OTLP, or product capture.
//! async fn run_with_custom_stdout(
//!     org_id: uuid::Uuid,
//!     app_id: uuid::Uuid,
//! ) -> cja::Result<()> {
//!     use tracing_subscriber::Layer as _;
//!     let token = load_eyes_token().await?;
//!     let stdout = tracing_subscriber::fmt::layer()
//!         .json()
//!         .with_writer(std::io::stdout)
//!         .with_filter(tracing_subscriber::EnvFilter::new("info"));
//!     let telemetry = cja::setup::TracingConfig::new("my-app")
//!         .stdout_layer(stdout)
//!         .eyes_ids(org_id, app_id)
//!         .eyes_token(token)
//!         .init()?;
//!     let result = run_tasks().await;
//!     telemetry.shutdown().await?;
//!     result
//! }
//!
//! async fn run_tasks() -> cja::Result<()> {
//!     let app_state = MyAppState { /* ... */ };
//!
//!     // Registers SIGTERM/SIGINT and owns the shutdown sequence
//!     let mut supervisor = Supervisor::new(ShutdownBudget::from_env())?;
//!     let shutdown = supervisor.shutdown_token();
//!
//!     // HTTP server: stops accepting connections when shutdown starts
//!     supervisor.spawn("server",
//!         cja::server::run_server_until(
//!             routes(app_state.clone()),
//!             shutdown.clone().cancelled_owned(),
//!         )
//!     );
//!
//!     // Job worker: stops claiming jobs, drains the in-flight one, then
//!     // releases its lock so another instance picks it up immediately
//!     supervisor.spawn("jobs",
//!         cja::jobs::worker::job_worker(
//!             app_state.clone(),
//!             Jobs,  // your job registry (see jobs module docs)
//!             std::time::Duration::from_secs(60),
//!             cja::jobs::DEFAULT_MAX_RETRIES,
//!             shutdown.clone(),
//!             Default::default(),
//!         )
//!     );
//!
//!     // Returns Ok after a signal, Err if a task died on its own
//!     supervisor.run().await
//! }
//! ```
//!
//! See `crates/cja.app/` in the repository for a complete working example.
//! Keep the telemetry handle and call `shutdown().await` after every application
//! exit path. An underscore binding suppresses `must_use` but loses buffered
//! Eyes and `PostHog` data. `PostHog`'s OTLP copy redacts sensitive query values
//! (`code`, `state`, token, password, secret, key, signature and OTP keys),
//! drops authorization/cookie headers, and passes job `self`, `value`, and
//! serde `error.detail` fields through as Eyes does. Eyes keeps raw fields. App-owned
//! spans remain the app's responsibility: use
//! `#[instrument(skip(self))]` on `Job::run` to avoid exporting its payload.
//! Explicit product events and boot manifests are app-owned payloads.
//! Returned `PostHog` construction errors skip that sink. A dependency panic
//! during construction aborts in a `panic = "abort"` build.
//!
//! ## Configuration
//!
//! | Variable | Required | Default | Description |
//! |----------|----------|---------|-------------|
//! | `DATABASE_URL` | Yes | — | `PostgreSQL` connection string |
//! | `PORT` | No | `3000` | HTTP server port |
//! | `COOKIE_KEY` | No | Ephemeral, generated at boot | Base64-encoded cookie encryption key, at least 32 bytes decoded. Unset or blank generates a new key on each boot, in release builds too, and logs a warning. Set it if sessions need to survive restarts and deploys. |
//! | `SERVER_DISABLED` | No | `false` | Set to `"true"` to skip starting the HTTP server |
//! | `JOBS_DISABLED` | No | `false` | Set to `"true"` to skip starting the job worker |
//! | `CRON_DISABLED` | No | `false` | Set to `"true"` to skip starting the cron worker |
//! | `RUST_LOG` | No | `info` | Tracing filter directive. rig's content-bearing targets are always capped: `rig::completions`/`rig::streaming` at DEBUG, `rig_agent::agent::typed` at ERROR |
//! | `CJA_SHUTDOWN_JOB_DRAIN_SECS` | No | `1` | How long an in-flight job may run after a shutdown signal before it is dropped and unlocked. Invalid values warn and use the default |
//! | `CJA_SHUTDOWN_EXIT_GRACE_SECS` | No | `500ms when unset; whole seconds when set` | Extra time for tasks to exit after the job drain. Drain + grace + the telemetry flush (`TelemetryShutdownHandle::shutdown`, up to 3s when a sink is stalled) must fit the platform's kill window (Cloud Run 10s fixed, Fly 5s default). Invalid values warn and use the default |
//! | `JSON_LOGS` | No | — | If set, outputs JSON logs instead of tree format |
//! | `POSTHOG_PROJECT_TOKEN` | No | — | Enable OTLP logs/traces and explicit [`product_event!`] capture at boot |
//! | `POSTHOG_HOST` | No | `https://us.i.posthog.com` | OTLP and capture host |
//! | `POSTHOG_LOG_LEVEL` | No | info | Minimum level for BOTH `PostHog` log records and `PostHog` spans (one shared filter): `off` sends no traces either. Exporter crates (`opentelemetry*`, `hyper*`, `h2`, `reqwest`, `rustls`, `want`, `mio`, `posthog`) are always excluded, matched by target prefix |
//! | `POSTHOG_TRACES_SAMPLE_RATIO` | No | 1.0 | Root trace sampling probability |
//! | `EYES_ORG_ID` | No | — | Eyes tracing org ID (both `ORG_ID` and `APP_ID` required) |
//! | `EYES_APP_ID` | No | — | Eyes tracing app ID |
//! | `EYES_URL` | No | `https://eyes.coreyja.com` | Eyes server URL |
//!
//! ## Feature Flags
//!
//! ```toml
//! [features]
//! default = ["cron", "jobs"]
//! cron = ["jobs"]           # cron depends on jobs
//! jobs = []
//! testing = []              # enables mock OAuth for tests
//! ```
//!
//! Disabling `jobs` at compile time also disables `cron`.
//! [`testing::test_db`] is always available; `testing` enables only
//! `testing::mock_oauth`.
//!
//! ## Re-exports
//!
//! CJA re-exports key dependencies so you don't need to add them directly:
//!
//! ```rust
//! use cja::sqlx;
//! use cja::uuid;
//! use cja::color_eyre;
//! use cja::maud;
//! use cja::jiff;
//! use cja::jiff_sqlx;
//! ```

pub mod assets;
pub mod observability;
#[cfg(feature = "jobs")]
pub mod posthog_manifest;
pub use sqlx;
pub use uuid;

#[cfg(feature = "cron")]
pub mod cron;
#[cfg(feature = "jobs")]
pub mod eyes_manifest;
#[cfg(feature = "jobs")]
pub mod jobs;
pub mod server;
pub mod testing;

pub mod app_state;
pub mod setup;

pub use color_eyre;
pub use color_eyre::Result;

pub use maud;

pub mod db;
pub mod tasks;
mod tls;

pub use jiff;
pub use jiff_sqlx;

mod product_event;
#[doc(hidden)]
pub mod __private {
    pub use crate::product_event::{bare, enabled, identity, submit};
    pub use serde_json;
    pub use tracing;
}
