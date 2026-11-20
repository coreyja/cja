# CJA — Cron, Jobs and Axum

> **ALPHA**: CJA is under active development. APIs may change between releases.

A Rust web framework that combines background job processing, cron scheduling,
and an HTTP server built on [Axum](https://docs.rs/axum).

## Embedded frontend assets

Add `cja-build = { git = "https://github.com/coreyja/cja", rev = "<same rev as cja>" }` under `[build-dependencies]` and
put files in `assets/`. Pin `cja-build` and `cja` to the same revision. The build script is:

```rust
fn main() -> Result<(), cja_build::BuildError> {
    cja_build::AssetsBuilder::new()
        .static_dir("assets")
        .entry(cja_build::Entry::new("src/studio.ts", "studio.js"))
        .build()
}
```

Entries are relative to `frontend/` by default; `.frontend_dir(path)` changes
that root. A TypeScript entry requires `frontend/tsconfig.json`. Start with
`{"compilerOptions":{"target":"ES2022","module":"ESNext","moduleResolution":"Bundler","strict":true,"noEmit":true,"allowImportingTsExtensions":true,"lib":["ES2022","DOM"],"types":[]},"include":["src/**/*.ts"]}`.
Every entry must belong to the tsconfig program. Include each TypeScript entry
through `include` or `files`; for JavaScript entries, also enable `allowJs`.
`cja-build` runs native TypeScript 7.0.2 for type checking, then Rolldown for
minified IIFE bundles and external source maps. No Node installation is needed.
Imports must be relative and stay inside the frontend directory; `node_modules`, CSS, and
dynamic imports are rejected. Each ESM entry (`.format(EntryFormat::Esm)`) must
remain one self-contained chunk. For an IIFE export, set
`.format(EntryFormat::Iife { global_name: Some("cjaPasskey".into()) })`.

The native tsc resolves through `TSC`, the shared `$XDG_CACHE_HOME/cja` cache
(or `~/.cache/cja`), then a pinned HTTPS download verified by SHA-256. Set
`CARGO_NET_OFFLINE=true` only with a verified cache or `TSC` override. Type or
bundle errors fail `cargo build`. Cargo reruns the build for config, source,
declaration, and static changes. Source map `sources` are frontend-relative
labels with embedded `sourcesContent`, not served source URLs.

### Tailwind CSS inputs

Add `.tailwind(cja_build::Tailwind::new("assets/input.css", "app.css"))` to
the build script. A root stylesheet starts with explicit, top-level inputs:

```css
@import "tailwindcss" source(none);
@source "../src";
@import "./portfolio.css";
```

Add a separate top-level `@source "../...";` for every template directory
under the app manifest, relative to `assets/input.css`. The root stylesheet
needs exactly one Tailwind import and at least one source. Imported CSS may
contain local `./` or `../` `.css` imports, but no Tailwind import or source.
The scanner accepts a strict subset of Tailwind CSS: one leading UTF-8 BOM,
ASCII bytes outside strings and comments, and ASCII whitespace around at-rule
arguments. `source(` outside strings and comments is reserved for the exact
root Tailwind import, even when comments split the token. Source paths are
literal directories beginning `./` or `../`, with no glob metacharacters.
Parentheses must balance and cannot contain braces.
`@import` and `@source` must start a statement (at the start of the file or
after `;`, `{` or `}`, outside parentheses) and be followed by a space, tab
or newline. Neutral whitespace before these rules is only space, tab, LF or
CRLF; a lone CR or form feed can make Tailwind drop a rule. Custom-property
values cannot contain at-rules and must balance `()`, `[]` and `{}`.
All inputs must remain inside the manifest directory, outside `target/`, and
free of symlinks. Unsupported at-rules fail the build, including `@plugin`,
`@config`, `@reference`, and `@tailwind`.

| Allowed at-rule | Consumer or reason allowed |
| --- | --- |
| `import` | cja.app and eyes; exact Tailwind root import or confined local `.css` imports |
| `source` | cja.app and future-form eyes; explicit depth-zero directories |
| `media` | cja.app and eyes; no file/path input or `source(...)` |
| `supports` | No file/path input or `source(...)` |
| `layer` | No file/path input or `source(...)` |
| `keyframes` | cja.app; no file/path input or `source(...)` |
| `font-face` | No file/path input or `source(...)` |
| `theme` | No file/path input or `source(...)` |
| `utility` | No file/path input or `source(...)` |
| `variant` | No file/path input or `source(...)` |
| `custom-variant` | No file/path input or `source(...)` |
| `apply` | No file/path input or `source(...)` |
| `container` | No file/path input or `source(...)` |
| `property` | No file/path input or `source(...)` |
| `starting-style` | No file/path input or `source(...)` |
| `page` | No file/path input or `source(...)` |
| `charset` | No file/path input or `source(...)` |

Arena does not use Tailwind.

At module scope, call `cja::include_assets!(site_assets)`. Use
`site_assets::asset_url("img/logo.svg")` for a scoped logical-name lookup; an
unknown name panics. Merge
`cja::assets::router::<AppState>(&[&site_assets::MANIFEST])?` into the root
Axum router before `.with_state(state)`. The router accepts multiple manifests
and rejects duplicate served URLs at startup. The macro records the invoking
crate via `CARGO_PKG_NAME`; collision errors name both owning crates and logical
names. Call `router()` once with every
manifest: calling it twice, or defining another `/assets` wildcard, conflicts
at startup. Unknown `/assets` URLs return 404 even with an app fallback.

Names may contain ASCII letters, digits, `.`, `_`, `~`, `@`, `+`, `-`, and `/`.
Dot-prefixed files and directories at any depth are skipped. Files are embedded at `/assets/name.<sha256-8>.ext` with immutable cache headers.
Root-level `favicon.ico` and `robots.txt` also have one-hour stable aliases.
Missing directories, unreadable files, invalid names, and collisions fail the
Cargo build. No asset is read from disk at runtime.


## Overview

CJA provides an HTTP server and, by default, two PostgreSQL-backed subsystems:

- **Jobs** — PostgreSQL-backed background job queue with automatic retries,
  exponential backoff, dead letter queue, and graceful shutdown
- **Cron** — Scheduled task execution with both interval-based and cron expression
  scheduling, timezone support
- **Server** — HTTP server on Axum with encrypted cookie management and
  zero-downtime reload via systemfd; database-backed sessions require `postgres`

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

For a server without PostgreSQL, use `cja = { version = "...", default-features = false }`.
The `AppState` implementation needs no pool:

```rust
use cja::{app_state::AppState, server::cookies::CookieKey};

#[derive(Clone)]
struct StatelessState { cookie_key: CookieKey }

impl AppState for StatelessState {
    fn version(&self) -> &str { "1.0.0" }
    fn cookie_key(&self) -> &CookieKey { &self.cookie_key }
}
```

### 2. Set Up the Database

With the default `postgres` feature, set `DATABASE_URL` and run migrations on startup:

```rust
let pool = sqlx::postgres::PgPoolOptions::new()
    .max_connections(5)
    .connect(&std::env::var("DATABASE_URL")?)
    .await?;

// Run CJA's built-in migrations (jobs, crons, cron_pauses, sessions, dead_letter_jobs)
cja::db::run_migrations(&pool).await?;
```

**Note:** `run_migrations()` uses SQLx's built-in advisory locking — do NOT wrap it with additional `pg_advisory_lock` calls.

### 3. Wire Up `main()`

```rust
use cja::setup::setup_tracing;
use cja::tasks::{ShutdownBudget, Supervisor};

fn main() -> cja::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> cja::Result<()> {
    // Flushes Eyes and PostHog when shutdown() is called below
    let telemetry = setup_tracing("my-app")?;
    let result = run_tasks().await;
    if let Err(error) = &result {
        tracing::error!(error = %format_args!("{error:#}"), "application tasks failed");
    }
    let flush = telemetry.shutdown().await;
    if result.is_ok() { flush?; }
    result
}

async fn run_tasks() -> cja::Result<()> {
    let app_state = MyApp { /* ... */ };
    let mut supervisor = Supervisor::new(ShutdownBudget::from_env())?;
    let shutdown = supervisor.shutdown_token();

    supervisor.spawn("server",
        cja::server::run_server_until(
            routes(app_state.clone()), shutdown.clone().cancelled_owned(),
        )
    );
    supervisor.spawn("jobs",
        cja::jobs::worker::job_worker(
            app_state.clone(),
            Jobs, // your job registry
            std::time::Duration::from_secs(60),
            cja::jobs::DEFAULT_MAX_RETRIES,
            shutdown.clone(),
            Default::default(),
        )
    );

    supervisor.run().await
}
```

See `crates/cja.app/` in the repository for a complete working example.

## Configuration

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `DATABASE_URL` | With `postgres` | — | PostgreSQL connection string |
| `PORT` | No | `3000` | HTTP server port |
| `COOKIE_KEY` | No | Ephemeral, generated at boot | Base64-encoded cookie encryption key, at least 32 bytes decoded. Unset or blank generates a new key on each boot, in release builds too, and logs a warning. Set it if sessions need to survive restarts and deploys. |
| `SERVER_DISABLED` | No | `false` | Set to `"true"` to skip starting the HTTP server |
| `JOBS_DISABLED` | No | `false` | Set to `"true"` to skip starting the job worker |
| `CRON_DISABLED` | No | `false` | Set to `"true"` to skip starting the cron worker |
| `RUST_LOG` | No | `info` | Tracing filter directive |
| `CJA_SHUTDOWN_JOB_DRAIN_SECS` | No | `1` | How long an in-flight job may run after a shutdown signal before it is dropped and unlocked. Invalid values warn and use the default |
| `CJA_SHUTDOWN_EXIT_GRACE_SECS` | No | `500ms when unset; whole seconds when set` | Extra time for tasks to exit after the job drain. Drain + grace + the telemetry flush (`TelemetryShutdownHandle::shutdown`, up to 3s when a sink is stalled) must fit the platform's kill window (Cloud Run 10s fixed, Fly 5s default). Invalid values warn and use the default |
| `JSON_LOGS` | No | — | If set, outputs JSON logs instead of tree format |
| `POSTHOG_PROJECT_TOKEN` | No | — | Enable OTLP logs and traces to PostHog at boot |
| `POSTHOG_HOST` | No | `https://us.i.posthog.com` | PostHog OTLP host |
| `POSTHOG_LOG_LEVEL` | No | `info` | Minimum level for BOTH PostHog log records and PostHog spans (one shared filter): `off` sends no traces either |
| `POSTHOG_TRACES_SAMPLE_RATIO` | No | `1.0` | Root trace sampling probability |
| `EYES_ORG_ID` | No | — | Eyes tracing org ID (both `ORG_ID` and `APP_ID` required) |
| `EYES_APP_ID` | No | — | Eyes tracing app ID |
| `EYES_URL` | No | `https://eyes.coreyja.com` | Eyes server URL |

## Feature Flags

```toml
[features]
default = ["postgres", "cron", "jobs"]
postgres = ["dep:sqlx", "dep:jiff-sqlx"]
cron = ["jobs"]           # cron depends on jobs
jobs = ["postgres"]       # jobs depends on postgres
testing = []              # enables testing::mock_oauth
```

`cron` implies `jobs`, which implies `postgres`. The `postgres` feature exposes
`db`, `server::session`, and `testing::test_db`; `testing` independently enables
`testing::mock_oauth`.

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
- Default heartbeat interval: 30 seconds (`DEFAULT_HEARTBEAT_INTERVAL`)
- Default reclaim window: 120 seconds (`DEFAULT_RECLAIM_WINDOW`)
- Handler panics are caught and recorded like returned errors. An expired lease also increments `error_count` and records its previous worker.
- With `max_retries = N`, a job may execute N+1 times. A terminal failure moves it to `dead_letter_jobs` with the final incremented count. Success and graceful shutdown do not count as failures.

The single `job_worker` accepts `JobWorkerConfig { heartbeat_interval, reclaim_window, shutdown_drain_timeout, idle_poll_gate, type_concurrency }`; pass `Default::default()` unless the application needs custom timings. `type_concurrency` carries per-process running counts behind `Job::CONCURRENCY` caps: production uses `ConcurrencyTracker::process_shared()` (also what `Default` gives you) — one shared tracker per process across all workers; `ConcurrencyTracker::isolated()` is for tests embedding independent worker fleets in one process. `idle_poll_gate` defaults to `None`, preserving independent polling. Defaults are 30 seconds between heartbeats, 120 seconds from the last successful heartbeat before reclaim, and the `ShutdownBudget::from_env().job_drain` value (1 second when unset). `CJA_SHUTDOWN_JOB_DRAIN_SECS` controls both the default worker drain and a default Supervisor budget. The reclaim window must be at least three heartbeat intervals. A missing heartbeat makes any owned row eligible after the window; actual pickup also depends on the idle poll cadence. A still-running body stops if its worker detects lost ownership or its watchdog expires. Job side effects must remain idempotent.

To limit idle claims, create one `IdlePollGate::new(NonZeroUsize::new(2).unwrap())`, clone it into each worker's `JobWorkerConfig { idle_poll_gate: Some(gate.clone()), ..Default::default() }`, and keep a clone in application state. Override `AppState::job_enqueued` to call `gate.wake_one()`. This local wake follows successful autocommit enqueue only. Direct SQL, other processes, transactional enqueue, due retries, and expired leases still need periodic claims; permits never cover job bodies or heartbeats.

The worker's `shutdown_drain_timeout` should match the `job_drain` of the `ShutdownBudget` given to its Supervisor. If the Supervisor uses a programmatically constructed budget, pass `JobWorkerConfig { shutdown_drain_timeout: supervisor.budget().job_drain, ..Default::default() }`. `ShutdownBudget` owns the process deadline (`job_drain + exit_grace`); the worker config bounds its local body drain. Keep `exit_grace` positive so lock-release SQL can finish, and fit the full deadline inside the hosting platform's kill window.

Heartbeats share the app `PgPool`; size it for heartbeat and job queries. With W busy workers and H-second intervals, heartbeat load averages at most W/H row UPDATEs per second (ticks may cluster); idle workers issue none. At 30 seconds, 3–11 busy arena workers use about 0.1–0.37 UPDATEs/s, 20 use 0.67/s, and 100 use 3.3/s.

A `panic = "abort"` build or hard kill cannot catch a handler panic, so its abandoned lease is charged on expiry.

Panics outside the caught handler future (worker loop, SQL, heartbeat/watchdog setup, or job-body `Drop`) still unwind the worker. `Supervisor::run_until` logs `Task ended the process`, cancels shared shutdown, lets peers perform bounded drain and uncharged lock release, then returns an error for platform restart. The panicking worker's abandoned row is charged when its lease expires.

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

// Cron expression — a second job type, since each name registers once
registry.register_job_with_cron(OtherJob, Some("description"), "0 */5 * * * *");
```

Job registrations (`register_job` and `register_job_with_cron`) commit exactly
one enqueue per due interval or cron slot across schedulers sharing a database.
The row lock, queue insert, and `last_run_at` update share one transaction. A
crash before commit rolls everything back, so the next tick retries. Job
execution remains at-least-once under worker lease and retry semantics.

Callback registrations (`register` and `register_with_cron`) claim at most once
per interval: the timestamp commits before invocation. A callback error is
logged and the worker keeps running — the next interval fires without a
restart. Process death still skips that interval; the next interval fires
after restart. A panicking callback is not caught: under the Supervisor it
ends the process. Its claim has already committed, so the restart does not
retry it immediately, but a callback that always panics ends the process
again each interval. During a rollout, an older scheduler overlapping an
upgraded one can still double-fire.

Duplicate cron names panic at registration — callback and job names share one
namespace, so each cron name registers exactly once. A `last_run_at` ahead of
the database clock fires on the next tick and no longer pauses the cron.

### Running the Cron Worker

```rust
// Default: UTC timezone, 60-second poll interval
Worker::new(app_state, registry).run(shutdown_token).await;

// Custom timezone and poll interval
let eastern = cja::jiff::tz::TimeZone::get("America/New_York")?;
Worker::new_with_timezone(app_state, registry, eastern, Duration::from_secs(10))
    .run(shutdown_token)
    .await;

// Inside a `tasks::Supervisor`, whose tasks return `cja::Result<()>`:
let token = shutdown.clone();
supervisor.spawn("cron", async move {
    worker.run(token).await;
    Ok(())
});
```

Note: `Worker::new()` uses a 60-second poll interval with UTC. The only way to customize the poll interval is `Worker::new_with_timezone()` — there is no `new_with_interval()` method.

Cron expressions are evaluated against local wall-clock time in the worker's
timezone. Across daylight-saving changes, a time the clock skips (02:30 on
spring-forward night in the US) fires right after the jump, and a time the
clock repeats (01:30 on fall-back night) fires once. Sub-hourly expressions
don't fire again during the repeated hour, so use an interval when a job needs
elapsed-time cadence.

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
    async fn from_db(
        pool: &sqlx::PgPool,
        session_id: uuid::Uuid,
    ) -> cja::Result<Option<Self>> {
        // Load from database with fetch_optional; Ok(None) for a missing
        // row (the extractor then creates a session), Err only for real
        // failures (the extractor 500s).
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

### Session lifecycle

The `Session` extractor refreshes `sessions.updated_at` at most once per
`ACTIVITY_REFRESH_MIN_INTERVAL`, so the timestamp tracks real activity without
a write per request. Schedule `cja::server::session::prune(pool, idle)` from a
cron to delete idle sessions (the scaffold registers a daily 4am example);
`idle` must be at least twice `ACTIVITY_REFRESH_MIN_INTERVAL` — the function
refuses smaller values. The logout route lives in cja-passport (`POST /auth/logout`):
it deletes the session row and clears the cookie.

### Maud Templating

CJA re-exports [Maud](https://maud.lambda.xyz/) for HTML templating.

Note: `Markup` is `PreEscaped<String>`, not `PreEscaped<&str>`. When embedding raw HTML, use `.to_string()`:

```rust
PreEscaped(r#"<svg>...</svg>"#.to_string())
```

## Migrations

CJA ships migrations for its core tables (jobs, crons, cron_pauses, sessions, dead_letter_jobs). Two approaches:

### Recommended: `cja::db::run_migrations()`

```rust
cja::db::run_migrations(&pool).await?;
```

This runs the framework's migrations using SQLx's built-in advisory locking. Do NOT wrap with additional `pg_advisory_lock` calls.

### Alternative: Copy Migrations

If your app keeps its own copies of cja's migration files (in its `migrations/`
directory), keep them in sync with the framework when upgrading CJA. The
`cja sync-migrations` CLI command copies the real files from the cja version
you have locked in your lockfile:

```bash
cja sync-migrations
```

Both migrators share the `_sqlx_migrations` table; if you also run your own
migrations, see the "App migrations" section of the `cja::db` module docs.

## Tracing & Observability

```rust
use cja::setup::setup_tracing;

let telemetry = setup_tracing("my-app")?;
// After application tasks finish
telemetry.shutdown().await?;
```

For a platform-specific stdout format and a token resolved at runtime, use
`TracingConfig` inside Tokio. Keep the handle until all tasks exit:

```rust,ignore
use tracing_subscriber::Layer as _;

let eyes_token = load_eyes_token().await?;
let stdout = tracing_subscriber::fmt::layer()
    .json()
    .with_writer(std::io::stdout)
    .with_filter(tracing_subscriber::EnvFilter::new("info"));
let telemetry = cja::setup::TracingConfig::new("my-app")
    .stdout_layer(stdout)
    .eyes_ids(org_id, app_id)
    .eyes_token(eyes_token)
    .init()?;
let result = run_app().await;
telemetry.shutdown().await?;
result
```

The local stdout filter changes only stdout. It must admit operational INFO
lines consumed by platform metrics; a WARN-only filter is useful for test
isolation but would hide those lines in production. Explicit Eyes IDs and token
override their corresponding env values; omitted options keep env behavior.
`POSTHOG_PROJECT_TOKEN` is the only PostHog runtime gate. Do not put a PostHog
personal key in an app runtime.

Call `shutdown().await?` on the returned `TelemetryShutdownHandle` to flush Eyes and PostHog, on the error path too. `let _handle = setup_tracing(..)?` suppresses the `must_use` warning but loses buffered telemetry on exit. Each flush is bounded: the call returns within 3 seconds even when a sink is stalled, always returns `Ok`, and logs failures. It runs after the supervisor's drain and grace, so count it against the platform's kill window. The default total is 1s drain + 0.5s grace + up to 3s flush = 4.5s. For Cloud Run, `CJA_SHUTDOWN_JOB_DRAIN_SECS=4` and `CJA_SHUTDOWN_EXIT_GRACE_SECS=2` give 9s total. Set `POSTHOG_PROJECT_TOKEN` in the process environment before tracing setup. `POSTHOG_HOST` defaults to `https://us.i.posthog.com`, `POSTHOG_LOG_LEVEL` to `info`, and `POSTHOG_TRACES_SAMPLE_RATIO` to `1.0`. Both signals honor scoped `RUST_LOG` and the rig content cap.

PostHog's OTLP copy redacts query values for `code`, `state`, `token`, `access_token`, `id_token`, `refresh_token`, `code_verifier`, `password`, `secret`, `client_secret`, `key`, `api_key`, `apikey`, `signature`, `sig`, and `otp`, plus keys containing `token`, `secret`, `password`, `passwd`, or `signature` (case insensitive). It keeps query keys and other values in `uri`, `url.query`, and `http.request.header.referer`, and drops the whole query if parsing fails. It drops recorded `authorization`, `proxy_authorization`, `cookie`, and `set_cookie` headers, the unstructured `self` Debug field on `jobs.enqueue`, and `value` on `jobs.run_from_value` and on log records inside a job. The worker's deserialize-failure detail remains in Eyes and `dead_letter_jobs`; PostHog receives only the top-level error. Other request headers, paths, and routes remain. Eyes receives raw telemetry. App-owned span fields are outside this policy: use `#[instrument(skip(self))]` on `Job::run` so its payload is not exported. Explicit product-event capture and the boot manifest use app-chosen payloads and do not use this filter. Returned PostHog construction errors skip that sink; a dependency panic aborts the process in a `panic = "abort"` profile.

`POSTHOG_LOG_LEVEL` gates both PostHog log records and PostHog spans through one shared per-layer filter, so `off` sends no traces either (the sample ratio only applies to spans the level admits). The same filter always excludes the exporter's own crates, matched by target prefix (tracing-subscriber `Targets` uses `starts_with`): `opentelemetry`, `tracing_opentelemetry`, `hyper`, `hyper_util`, `h2`, `reqwest`, `rustls`, `want`, `mio` and `posthog`, so `opentelemetry` also covers `opentelemetry_sdk`, `opentelemetry_otlp` and the rest of the family. While PostHog is enabled, Eyes also drops `opentelemetry*` targets, so the OTel SDK's own diagnostics (queue drops, export errors) reach stdout only; with no token, Eyes is unfiltered as before.

## Re-exports

CJA re-exports key dependencies:

```rust
use cja::uuid;
use cja::color_eyre;
use cja::maud;
use cja::jiff;
```

With `postgres`, `cja::sqlx` and `cja::jiff_sqlx` are also re-exported.

# PostHog boot manifests

When `POSTHOG_PROJECT_TOKEN` is set, call `posthog_manifest::emit_boot_manifest`
once after constructing the complete cron registry and app state, before moving
the registry to its worker. Pass `BootManifest::new(app_slug, &registry,
&worker_timezone, &critical_jobs, &hogql_checks)` with explicit lists, including
empty lists. The slug must be stable across boots and is independent of
`EYES_APP_ID`. Cron expressions use cja's parser: seconds are required, the
year is optional, day of month and day of week both apply, and weekday 1 is
Sunday. Interval schedules are sent as milliseconds.

The emitter validates declarations synchronously, then sends a complete
`cja_manifest` capture asynchronously. Capture uses `POSTHOG_HOST` (default
`https://us.i.posthog.com`) and does not read OTLP filter knobs. Network
failures never delay boot; retried snapshots retain their event ID and expire
after 24 hours or process exit. A successful HTTP response acknowledges receipt,
not later query visibility.
