# Architecture

CJA is a Rust web framework built around three subsystems — **Jobs**, **Cron**, and an **Axum HTTP server** — plus supporting modules for sessions, cookies, tracing, and database management.

## Crate Structure

```
crates/
  cja/           Core framework library
  cja.app/       Example application demonstrating the framework
  cli/           Project scaffolding CLI (cja-cli)
```

The `cja` crate is the only published library. `cja.app` serves as the canonical reference implementation. `cja-cli` generates new CJA projects.

## Module Tree

```
crates/cja/src/
├── lib.rs              Re-exports (sqlx, uuid, color_eyre, maud, chrono, chrono_tz)
├── app_state.rs        AppState trait
├── setup.rs            Sentry + tracing initialization
├── db.rs               Migration runner
├── tasks.rs            NamedTask, wait_for_first_error
├── jobs/
│   ├── mod.rs          Job trait, enqueue logic, DB queries
│   ├── worker.rs       job_worker() polling loop, retry/dead-letter logic
│   └── registry.rs     JobRegistry trait, impl_job_registry! macro
├── cron/
│   ├── mod.rs          Re-exports
│   ├── registry.rs     CronRegistry, Schedule enum, CronJob
│   └── worker.rs       Cron Worker tick loop
├── server/
│   ├── mod.rs          run_server(), middleware stack
│   ├── cookies/
│   │   ├── cookie_key.rs  CookieKey (from env or generated)
│   │   └── cookie_jar.rs  CookieJar Axum extractor
│   ├── session.rs      CJASession, AppSession trait, Session<T> extractor
│   ├── trace.rs        Tracer (MakeSpan + OnResponse for OpenTelemetry)
│   └── page/
│       ├── mod.rs      Page wrapper
│       └── factory.rs  Factory Axum extractor
└── testing/
    └── mock_oauth/     Feature-gated mock OAuth server for tests
```

## Key Traits

### AppState

The central trait all applications must implement. Provides access to the database pool, cookie key, and version string. The state is cloned per request, so implementations should wrap expensive resources in `Arc`.

```rust
pub trait AppState: Clone + Send + Sync + 'static {
    fn version(&self) -> &str;
    fn db(&self) -> &sqlx::PgPool;
    fn cookie_key(&self) -> &CookieKey;
}
```

### Job\<AppState\>

Defines a background job. Each job type has a unique `NAME` constant used for routing. Jobs are serialized to JSON, stored in PostgreSQL, and processed by a worker.

```rust
#[async_trait]
pub trait Job<AppState: AS>:
    Serialize + DeserializeOwned + Send + Sync + Debug + Clone + 'static
{
    const NAME: &'static str;
    async fn run(&self, app_state: AppState) -> Result<()>;
    async fn run_with_cancellation(&self, app_state: AppState, token: CancellationToken) -> Result<()>;
    async fn enqueue(self, app_state: AppState, context: String, priority: Option<i32>) -> Result<(), EnqueueError>;
}
```

### JobRegistry\<AppState\>

Maps job names to their implementations. Generated via the `impl_job_registry!` macro. Every job type used in the application must be listed in the macro invocation.

```rust
impl_job_registry!(MyAppState, EmailJob, PaymentJob, CleanupJob);
```

### AppSession

Defines custom session types backed by the `sessions` table. Implement this to add application-specific session data.

```rust
#[async_trait]
pub trait AppSession: Sized {
    async fn from_db(pool: &PgPool, session_id: Uuid) -> Result<Self>;
    async fn create(pool: &PgPool) -> Result<Self>;
    fn from_inner(inner: CJASession) -> Self;
    fn inner(&self) -> &CJASession;
}
```

The `Session<T>` Axum extractor automatically creates a new session if none exists, or loads an existing one from the encrypted cookie.

## Data Flow

### HTTP Request

```
Client request
  → listenfd/systemfd (optional, for zero-downtime reload)
  → Axum middleware stack
    → Cookie manager layer
    → Tracing layer (Tracer: captures URI, method, headers, latency)
  → Route handler
    → Session<T> extractor (reads/creates session)
    → State<AppState> extractor
  → Response
    → Tracer OnResponse (records status, latency, content-type)
```

### Job Processing

```
job.enqueue(app_state, context, priority)
  → INSERT INTO jobs (payload as JSONB)

job_worker() loop:
  → SELECT ... FROM jobs
      WHERE run_at <= NOW()
        AND (locked_by IS NULL OR locked_at < NOW() - lock_timeout)
      ORDER BY priority DESC, created_at ASC
      LIMIT 1
      FOR UPDATE SKIP LOCKED
    → UPDATE SET locked_by = worker_id, locked_at = NOW()
    → RETURNING *

  → registry.run_job(job) → Job::run_with_cancellation()

  → On success: DELETE FROM jobs WHERE job_id = $1
  → On failure (under max retries):
      UPDATE SET locked_by = NULL,
        error_count = error_count + 1,
        run_at = NOW() + 2^(error_count+1) seconds
  → On failure (exceeded max retries):
      INSERT INTO dead_letter_jobs (...)
      DELETE FROM jobs WHERE job_id = $1
```

The `FOR UPDATE SKIP LOCKED` clause is what makes concurrent workers safe — multiple workers compete at the database level without duplicate processing.

### Cron Scheduling

```
Cron Worker tick (every sleep_duration, default 60s):
  → For each registered job:
    → Check schedule (interval or cron expression) against last_run_at from DB
    → If should_run:
      → Enqueue job or execute closure
      → UPSERT crons SET last_run_at = NOW()
```

## Database Schema

### jobs

| Column | Type | Description |
|--------|------|-------------|
| job_id | UUID | Primary key |
| name | TEXT | Job type name (matches `Job::NAME`) |
| payload | JSONB | Serialized job data |
| priority | INT | Higher = runs first (`ORDER BY priority DESC`) |
| run_at | TIMESTAMPTZ | When job can next be executed |
| created_at | TIMESTAMPTZ | When job was enqueued |
| locked_at | TIMESTAMPTZ | When a worker locked this job |
| locked_by | TEXT | Worker UUID that holds the lock |
| context | TEXT | Debug info (e.g., "user-signup") |
| error_count | INT | Number of failures |
| last_error_message | TEXT | Most recent error |
| last_failed_at | TIMESTAMPTZ | Timestamp of last failure |

### dead_letter_jobs

Jobs that exceeded max retries are moved here for manual investigation.

| Column | Type | Description |
|--------|------|-------------|
| id | UUID | Primary key |
| original_job_id | UUID | Original job_id |
| name | TEXT | Job type name |
| payload | JSONB | Serialized job data |
| context | TEXT | Debug info |
| priority | INT | Original priority |
| error_count | INT | Total failure count |
| last_error_message | TEXT | Final error |
| created_at | TIMESTAMPTZ | Original creation time |
| failed_at | TIMESTAMPTZ | When moved to dead letter |

### crons

| Column | Type | Description |
|--------|------|-------------|
| cron_id | UUID | Primary key |
| name | TEXT | Job name (UNIQUE) |
| last_run_at | TIMESTAMPTZ | When it last ran |
| created_at | TIMESTAMPTZ | Row creation |
| updated_at | TIMESTAMPTZ | Last update |

### sessions

| Column | Type | Description |
|--------|------|-------------|
| session_id | UUID | Primary key |
| created_at | TIMESTAMPTZ | Session creation |
| updated_at | TIMESTAMPTZ | Auto-updated via trigger |

## Graceful Shutdown

CJA uses `tokio_util::sync::CancellationToken` for coordinated shutdown:

1. Signal handler catches SIGTERM/SIGINT
2. Cancels the shared `CancellationToken`
3. Job worker stops accepting new jobs, releases database locks
4. Cron worker stops ticking
5. HTTP server stops accepting connections
6. `wait_for_first_error()` detects the exit and propagates

All long-running tasks are wrapped in `NamedTask` and monitored via `wait_for_first_error()` — if any task exits (even successfully), it's treated as an error because these services should run indefinitely.
