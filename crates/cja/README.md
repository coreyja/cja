# CJA — Cron, Jobs and Axum

A Rust web framework that combines background job processing, cron scheduling, and an HTTP server built on Axum.

> **ALPHA**: CJA is under active development. APIs may change between releases.

## Overview

CJA provides three integrated subsystems:

- **Jobs** — PostgreSQL-backed background job queue with automatic retries, exponential backoff, dead letter queue, and graceful shutdown
- **Cron** — Scheduled task execution with both interval-based and cron expression scheduling, timezone support
- **Server** — HTTP server on Axum with encrypted cookie management, database-backed sessions, and zero-downtime reload via systemfd

All three share a single `AppState` and coordinate shutdown via `CancellationToken`.

## Getting Started

Add CJA to your project:

```bash
cargo add cja
```

### 1. Implement AppState

```rust
use cja::app_state::AppState;
use cja::server::cookies::CookieKey;

#[derive(Clone)]
struct MyAppState {
    db: sqlx::PgPool,
    cookie_key: CookieKey,
}

impl AppState for MyAppState {
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

sqlx::migrate!().run(&pool).await?;
```

### 3. Wire Up main()

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
    setup_tracing("my-app")?;

    let app_state = MyAppState { /* ... */ };
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
            Jobs,  // your job registry (see below)
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

See `crates/cja.app/` for a complete working example.

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
| `SENTRY_DSN` | No | — | Enables Sentry error tracking (50% trace sample rate) |
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
use cja::jobs::Job;
use serde::{Serialize, Deserialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
struct SendEmailJob {
    to: String,
    subject: String,
}

#[async_trait::async_trait]
impl Job<MyAppState> for SendEmailJob {
    const NAME: &'static str = "SendEmailJob";

    async fn run(&self, app_state: MyAppState) -> cja::Result<()> {
        // Send email using app_state.db() for templates, etc.
        Ok(())
    }
}
```

### Registering Jobs

All job types must be registered via the `impl_job_registry!` macro:

```rust
cja::impl_job_registry!(MyAppState, SendEmailJob, CleanupJob, ReportJob);
```

This generates a `Jobs` struct that routes jobs by their `NAME` constant. Forgetting to register a job type means it won't be processed.

### Enqueuing Jobs

```rust
let job = SendEmailJob {
    to: "user@example.com".into(),
    subject: "Welcome!".into(),
};

// Default priority (0)
job.clone().enqueue(app_state.clone(), "user-signup".into(), None).await?;

// High priority
job.enqueue(app_state, "urgent-notification".into(), Some(10)).await?;
```

**Priority**: Higher values run first (`ORDER BY priority DESC`). Priority 10 runs before 0, and -10 runs last. This is per-enqueue, not per-job-type.

### Retry Behavior

Failed jobs are automatically retried with exponential backoff:

| Retry # | Delay |
|---------|-------|
| 1 | 4 seconds |
| 2 | 8 seconds |
| 3 | 16 seconds |
| 5 | ~1 minute |
| 10 | ~17 minutes |
| 15 | ~9 hours |
| 20 | ~12 days |

After 20 retries (configurable), jobs are moved to the `dead_letter_jobs` table.

### Cancellation Support

Long-running jobs can override `run_with_cancellation` to exit gracefully during shutdown:

```rust
async fn run_with_cancellation(
    &self,
    app_state: MyAppState,
    token: CancellationToken,
) -> cja::Result<()> {
    for item in &self.items {
        if token.is_cancelled() {
            return Err(eyre!("Cancelled during shutdown"));
        }
        process(item).await?;
    }
    Ok(())
}
```

### Concurrent Workers

Multiple `job_worker()` instances can run safely — the worker SQL uses `FOR UPDATE SKIP LOCKED` to prevent duplicate processing at the database level.

### Idempotency

Jobs must be idempotent. The system retries on timeouts, worker crashes, and network failures. Guard against double-application with:

- `ON CONFLICT DO NOTHING` for inserts
- Early-exit checks at job start
- Idempotency keys in your domain logic

## Cron System

### Interval Scheduling

```rust
use cja::cron::CronRegistry;
use std::time::Duration;

let mut registry = CronRegistry::new();

// Run a Job every 60 seconds
registry.register_job(MyJob, Some("Description"), Duration::from_secs(60));
```

### Cron Expression Scheduling

```rust
// Run at 9 AM every day (cron syntax: sec min hour day month weekday year)
registry.register_job_with_cron(
    DailyReportJob,
    Some("Daily report"),
    "0 0 9 * * * *",
)?;
```

### Running the Cron Worker

```rust
use cja::cron::Worker;
use chrono_tz::US::Eastern;

Worker::new_with_timezone(app_state, registry, Eastern, Duration::from_secs(60))
    .run(shutdown_token)
    .await?;
```

The default poll interval is 60 seconds. To change it, use `new_with_timezone` even if you just want UTC — there is no `new_with_interval` method.

### Queue Pileup Warning

If a cron job takes longer to run than its scheduling interval, the queue grows unbounded (CJA does not deduplicate). For example, a 60-second interval with 20-minute job runtime queues ~20 copies before the first finishes. Mitigation: use longer intervals, add job-level deduplication, or set timeouts.

## Server

### Routes

Build routes using standard Axum patterns:

```rust
fn routes(app_state: MyAppState) -> axum::Router {
    axum::Router::new()
        .route("/", axum::routing::get(index))
        .route("/api/health", axum::routing::get(health))
        .with_state(app_state)
}
```

### Sessions

Implement `AppSession` for your session type, then use `Session<T>` as an Axum extractor:

```rust
async fn index(Session(session): Session<MySession>) -> impl IntoResponse {
    html! {
        p { "Session: " (session.inner().session_id) }
    }
}
```

Sessions are automatically created on first access and persisted via encrypted cookies.

### Zero-Downtime Reload

```bash
systemfd --no-pid -s http::3000 -- cargo watch -x 'run -p my-app'
```

CJA checks for the `LISTEN_FDS` environment variable and reuses the existing socket when present.

## Migrations

CJA ships with migrations for the `jobs`, `dead_letter_jobs`, `crons`, and `sessions` tables. Run them on startup:

```rust
sqlx::migrate!().run(&pool).await?;
```

SQLx handles advisory locking internally — do not wrap with custom `pg_advisory_lock` calls.

## Tracing & Observability

`setup_tracing("app-name")` configures:

- **Console**: Tree-formatted logs (or JSON with `JSON_LOGS`)
- **Sentry**: Error tracking (with `SENTRY_DSN`)
- **Honeycomb**: OpenTelemetry export (with `HONEYCOMB_API_KEY`)
- **Eyes**: Distributed tracing (with both `EYES_ORG_ID` and `EYES_APP_ID`)

The function returns `Result<Option<EyesShutdownHandle>>` — keep the handle alive in `main` scope for graceful shutdown of the Eyes exporter.

## Re-exports

CJA re-exports these dependencies so you don't need to add them directly:

```rust
use cja::sqlx;
use cja::uuid;
use cja::color_eyre;
use cja::maud;
use cja::chrono;
use cja::chrono_tz;
```
