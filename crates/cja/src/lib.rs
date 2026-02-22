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
//! // Run CJA's built-in migrations (jobs, crons, sessions, dead_letter_jobs)
//! cja::db::run_migrations(&pool).await?;
//! ```
//!
//! ### 3. Wire Up `main()`
//!
//! ```rust,ignore
//! use cja::setup::{setup_sentry, setup_tracing};
//! use cja::tasks::NamedTask;
//! use cja::jobs::CancellationToken;
//!
//! fn main() -> cja::Result<()> {
//!     let _sentry_guard = setup_sentry();
//!     tokio::runtime::Builder::new_multi_thread()
//!         .enable_all()
//!         .build()?
//!         .block_on(run())
//! }
//!
//! async fn run() -> cja::Result<()> {
//!     // Keep the Eyes handle alive for graceful shutdown
//!     let _eyes_handle = setup_tracing("my-app")?;
//!
//!     let app_state = MyAppState { /* ... */ };
//!     let shutdown_token = CancellationToken::new();
//!
//!     let mut tasks = vec![];
//!
//!     // HTTP server
//!     tasks.push(NamedTask::spawn("server",
//!         cja::server::run_server(routes(app_state.clone()))
//!     ));
//!
//!     // Job worker
//!     tasks.push(NamedTask::spawn("jobs",
//!         cja::jobs::worker::job_worker(
//!             app_state.clone(),
//!             Jobs,  // your job registry (see jobs module docs)
//!             std::time::Duration::from_secs(60),
//!             cja::jobs::DEFAULT_MAX_RETRIES,
//!             shutdown_token.clone(),
//!             cja::jobs::DEFAULT_LOCK_TIMEOUT,
//!         )
//!     ));
//!
//!     // Signal handler
//!     let token = shutdown_token.clone();
//!     tasks.push(NamedTask::spawn("signals", async move {
//!         tokio::signal::ctrl_c().await?;
//!         token.cancel();
//!         Ok(())
//!     }));
//!
//!     cja::tasks::wait_for_first_error(tasks).await
//! }
//! ```
//!
//! See `crates/cja.app/` in the repository for a complete working example.
//!
//! ## Configuration
//!
//! | Variable | Required | Default | Description |
//! |----------|----------|---------|-------------|
//! | `DATABASE_URL` | Yes | — | `PostgreSQL` connection string |
//! | `PORT` | No | `3000` | HTTP server port |
//! | `COOKIE_KEY` | No | Auto-generated | Base64-encoded 64-byte cookie encryption key. **Must be set in production** for session persistence across restarts. |
//! | `SERVER_DISABLED` | No | `false` | Set to `"true"` to skip starting the HTTP server |
//! | `JOBS_DISABLED` | No | `false` | Set to `"true"` to skip starting the job worker |
//! | `CRON_DISABLED` | No | `false` | Set to `"true"` to skip starting the cron worker |
//! | `RUST_LOG` | No | `info` | Tracing filter directive |
//! | `JSON_LOGS` | No | — | If set, outputs JSON logs instead of tree format |
//! | `SENTRY_DSN` | No | — | Enables Sentry error tracking |
//! | `HONEYCOMB_API_KEY` | No | — | Enables OpenTelemetry export to Honeycomb |
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
//! use cja::chrono;
//! use cja::chrono_tz;
//! ```

pub use sqlx;
pub use uuid;

#[cfg(feature = "cron")]
pub mod cron;
#[cfg(feature = "jobs")]
pub mod jobs;
pub mod server;
#[cfg(feature = "testing")]
pub mod testing;

pub mod app_state;
pub mod setup;

pub use color_eyre;
pub use color_eyre::Result;

pub use maud;

pub mod db;
pub mod tasks;

pub use chrono;
pub use chrono_tz;
