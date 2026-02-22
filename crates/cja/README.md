# CJA — Cron, Jobs and Axum

> **ALPHA**: CJA is under active development. APIs may change between releases.

A Rust web framework that combines background job processing, cron scheduling,
and an HTTP server built on [Axum](https://docs.rs/axum).

## Overview

CJA provides three integrated subsystems:

- **Jobs** — PostgreSQL-backed background job queue with automatic retries,
  exponential backoff, dead letter queue, and graceful shutdown
- **Cron** — Scheduled task execution with both interval-based and cron expression
  scheduling, timezone support
- **Server** — HTTP server on Axum with encrypted cookie management, database-backed
  sessions, and zero-downtime reload via systemfd

All three share a single `AppState` and coordinate shutdown via `CancellationToken`.

## Getting Started

Add CJA to your project:

```bash
cargo add cja
```

### 1. Implement `AppState`

```rust
use cja::app_state::AppState;
use cja::server::cookies::CookieKey;

#[derive(Clone)]
struct MyApp {
    db: sqlx::PgPool,
    cookie_key: CookieKey,
}

impl AppState for MyApp {
    fn version(&self) -> &str { "1.0.0" }
    fn db(&self) -> &sqlx::PgPool { &self.db }
    fn cookie_key(&self) -> &CookieKey { &self.cookie_key }
}
```

`AppState` is cloned per request — wrap expensive resources in `Arc`.

### 2. Set Up the Database

CJA requires PostgreSQL. Set `DATABASE_URL` and run migrations on startup:

```rust
let pool = sqlx::postgres::PgPoolOptions::new()
    .max_connections(5)
    .connect(&std::env::var("DATABASE_URL")?)
    .await?;

// Run CJA's built-in migrations (jobs, crons, sessions, dead_letter_jobs)
cja::db::run_migrations(&pool).await?;
```

**Note:** `run_migrations()` uses SQLx's built-in advisory locking — do NOT wrap it with additional `pg_advisory_lock` calls.

### 3. Wire Up `main()`

```rust
use cja::setup::{setup_sentry, setup_tracing};
use cja::tasks::NamedTask;
use cja::jobs::CancellationToken;

fn main() -> cja::Result<()> {
    let _sentry_guard = setup_sentry();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> cja::Result<()> {
    // Keep the Eyes handle alive for graceful shutdown
    let _eyes_handle = setup_tracing("my-app")?;

    let app_state = MyApp { /* ... */ };
    let shutdown_token = CancellationToken::new();

    let mut tasks = vec![];

    // HTTP server
    tasks.push(NamedTask::spawn("server",
        cja::server::run_server(routes(app_state.clone()))
    ));

    // Job worker
    tasks.push(NamedTask::spawn("jobs",
        cja::jobs::worker::job_worker(
            app_state.clone(),
            Jobs,  // your job registry
            std::time::Duration::from_secs(60),
            cja::jobs::DEFAULT_MAX_RETRIES,
            shutdown_token.clone(),
            cja::jobs::DEFAULT_LOCK_TIMEOUT,
        )
    ));

    // Signal handler
    let token = shutdown_token.clone();
    tasks.push(NamedTask::spawn("signals", async move {
        tokio::signal::ctrl_c().await?;
        token.cancel();
        Ok(())
    }));

    cja::tasks::wait_for_first_error(tasks).await
}
```

See `crates/cja.app/` in the repository for a complete working example.

## Configuration

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `DATABASE_URL` | Yes | — | PostgreSQL connection string |
| `PORT` | No | `3000` | HTTP server port |
| `COOKIE_KEY` | No | Auto-generated | Base64-encoded 64-byte cookie encryption key. **Must be set in production** for session persistence across restarts. |
| `SERVER_DISABLED` | No | `false` | Set to `"true"` to skip starting the HTTP server |
| `JOBS_DISABLED` | No | `false` | Set to `"true"` to skip starting the job worker |
| `CRON_DISABLED` | No | `false` | Set to `"true"` to skip starting the cron worker |
| `RUST_LOG` | No | `info` | Tracing filter directive |
| `JSON_LOGS` | No | — | If set, outputs JSON logs instead of tree format |
| `SENTRY_DSN` | No | — | Enables Sentry error tracking |
| `HONEYCOMB_API_KEY` | No | — | Enables OpenTelemetry export to Honeycomb |
| `EYES_ORG_ID` | No | — | Eyes tracing org ID (both `ORG_ID` and `APP_ID` required) |
| `EYES_APP_ID` | No | — | Eyes tracing app ID |
| `EYES_URL` | No | `https://eyes.coreyja.com` | Eyes server URL |

## Feature Flags

```toml
[features]
default = ["cron", "jobs"]
cron = ["jobs"]           # cron depends on jobs
jobs = []
testing = []              # enables mock OAuth for tests
```

Disabling `jobs` at compile time also disables `cron`.

## Job System

### Defining a Job

```rust
use serde::{Serialize, Deserialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
struct SendEmail {
    to: String,
    subject: String,
}

#[async_trait::async_trait]
impl cja::jobs::Job<MyApp> for SendEmail {
    const NAME: &'static str = "SendEmail";

    async fn run(&self, app_state: MyApp) -> cja::Result<()> {
        // Send the email...
        Ok(())
    }
}
```

### Job Registry

Register all job types so the worker can dispatch them:

```rust
cja::impl_job_registry!(MyApp, SendEmail, AnotherJob);
```

### Enqueuing Jobs

```rust
SendEmail {
    to: "user@example.com".into(),
    subject: "Welcome".into(),
}
.enqueue(app_state, "user signup".into(), None)
.await?;

// With priority (higher value = runs first)
my_job.enqueue(app_state, "urgent".into(), Some(10)).await?;
```

**Priority:** Higher values run first (`ORDER BY priority DESC`). Priority 10 runs before 0, and -10 runs last. This is the opposite of Rails delayed_job conventions.

### Retry Behavior

- Failed jobs retry with exponential backoff: delay = `2^(error_count + 1)` seconds
- Default max retries: 20 (`DEFAULT_MAX_RETRIES`)
- Default lock timeout: 2 hours (`DEFAULT_LOCK_TIMEOUT`)
- After max retries, jobs move to the `dead_letter_jobs` table

### Idempotency

**Jobs must be idempotent.** The system retries on timeouts, worker crashes, and network failures. If a job applies state changes (inserts, counter increments), guard against double-application with `ON CONFLICT DO NOTHING` or an early-exit check.

### Multiple Workers

It's safe to run multiple `job_worker()` tasks concurrently — they use `FOR UPDATE SKIP LOCKED` at the database level to prevent duplicate processing.

## Cron System

### Registration

```rust
use cja::cron::{CronRegistry, Worker};
use std::time::Duration;

let mut registry = CronRegistry::new();

// Interval-based (every 60 seconds)
registry.register_job(MyJob, Some("description"), Duration::from_secs(60));

// Cron expression
registry.register_job_with_cron(MyJob, Some("description"), "0 */5 * * * *".parse()?);
```

### Running the Cron Worker

```rust
// Default: UTC timezone, 60-second poll interval
Worker::new(app_state, registry).run(shutdown_token).await?;

// Custom timezone and poll interval
Worker::new_with_timezone(app_state, registry, chrono_tz::US::Eastern, Duration::from_secs(10))
    .run(shutdown_token)
    .await?;
```

Note: `Worker::new()` uses a 60-second poll interval with UTC. The only way to customize the poll interval is `Worker::new_with_timezone()` — there is no `new_with_interval()` method.

### Queue Pileup Warning

If a cron job takes longer to run than its scheduling interval, the queue grows unbounded (no built-in deduplication). For example, a 60s interval with a 20min runtime means ~1 new queued job per minute. Mitigations: increase the interval, add job-level deduplication, or use timeouts.

When a cron job's logic depends on the interval (e.g., "process N items per run"), define the interval as a `pub const` and import it in the job implementation to prevent calculation drift.

## Server

### Running

```rust
let routes = axum::Router::new()
    .route("/", axum::routing::get(|| async { "Hello" }))
    .with_state(app_state);

cja::server::run_server(routes).await?;
```

### Zero-Downtime Reload

CJA supports zero-downtime reload via `systemfd`:

```bash
systemfd --no-pid -s http::3000 -- cargo watch -x 'run -p cja-site'
```

### Sessions

Implement `AppSession` for your session type:

```rust
struct MySession {
    inner: cja::server::session::CJASession,
}

#[async_trait::async_trait]
impl cja::server::session::AppSession for MySession {
    async fn from_db(pool: &sqlx::PgPool, session_id: uuid::Uuid) -> cja::Result<Self> {
        // Load from database...
    }
    async fn create(pool: &sqlx::PgPool) -> cja::Result<Self> {
        // Create new session...
    }
    fn from_inner(inner: cja::server::session::CJASession) -> Self {
        Self { inner }
    }
    fn inner(&self) -> &cja::server::session::CJASession {
        &self.inner
    }
}
```

Then use the `Session<MySession>` extractor in handlers:

```rust
async fn handler(session: cja::server::session::Session<MySession>) -> impl IntoResponse {
    format!("Session ID: {}", session.0.inner().session_id)
}
```

### Maud Templating

CJA re-exports [Maud](https://maud.lambda.xyz/) for HTML templating.

Note: `Markup` is `PreEscaped<String>`, not `PreEscaped<&str>`. When embedding raw HTML, use `.to_string()`:

```rust
PreEscaped(r#"<svg>...</svg>"#.to_string())
```

## Migrations

CJA ships migrations for its core tables (jobs, crons, sessions, dead_letter_jobs). Two approaches:

### Recommended: `cja::db::run_migrations()`

```rust
cja::db::run_migrations(&pool).await?;
```

This runs the framework's migrations using SQLx's built-in advisory locking. Do NOT wrap with additional `pg_advisory_lock` calls.

### Alternative: Copy Migrations

The `cja init` CLI command copies framework migrations into your project:

```bash
cja init my-project
```

If using this approach, you're responsible for keeping them in sync when upgrading CJA.

**Known issue:** `cja init` does not currently copy the `dead_letter_jobs` migration. Without it, job retry exhaustion will crash. Either copy it manually from `crates/cja/migrations/20260206200552_AddDeadLetterJobs.sql` or use `cja::db::run_migrations()` instead.

## Tracing & Observability

```rust
use cja::setup::{setup_sentry, setup_tracing};

let _sentry_guard = setup_sentry();
// Keep the handle alive — dropping it shuts down Eyes
let _eyes_handle = setup_tracing("my-app")?;
```

`setup_tracing()` returns `Result<Option<EyesShutdownHandle>>`. If Eyes is configured (both `EYES_ORG_ID` and `EYES_APP_ID` set), the handle must stay alive in your `main` scope.

## Re-exports

CJA re-exports key dependencies:

```rust
use cja::sqlx;
use cja::uuid;
use cja::color_eyre;
use cja::maud;
use cja::chrono;
use cja::chrono_tz;
```
