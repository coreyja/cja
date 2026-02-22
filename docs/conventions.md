# Conventions

## Error Handling

CJA uses `color_eyre::Result` throughout. Re-exported via `cja::Result`.

- Use `eyre!()` for ad-hoc errors
- Use `.wrap_err("context")` to add context to errors
- Propagate with `?` — don't swallow errors silently
- Sentry integration captures errors automatically when configured

## Tracing

- Use `#[instrument]` on async functions for automatic span creation
- Use structured fields: `tracing::info!(job_id = %id, "Processing job")`
- `info!` for lifecycle events (startup, shutdown, task completion)
- `warn!` for recoverable errors (job retries, missing optional config)
- `error!` for unrecoverable errors (task exits, permanent failures)
- `debug!` for operational details (sleep durations, polling intervals)

## Feature Flags

Two levels of feature control:

**Compile-time** (Cargo features):
- `default = ["cron", "jobs"]`
- `cron` depends on `jobs` — enabling cron automatically enables jobs
- `testing` — enables mock OAuth and test utilities
- Feature-gated modules use `#[cfg(feature = "...")]`

**Runtime** (environment variables):
- `JOBS_DISABLED=true` — skips starting the job worker
- `CRON_DISABLED=true` — skips starting the cron worker
- `SERVER_DISABLED=true` — skips starting the HTTP server
- Only the exact string `"true"` disables; any other value is treated as enabled

## Naming

- `AppState` for the application state trait (not `State`, which conflicts with Axum's extractor)
- `NamedTask` for spawned background tasks (wraps `JoinHandle` with a name)
- `{FEATURE}_DISABLED` for runtime disable env vars (positive = enabled by default)
- `Job::NAME` as a `const &str` — must be unique across all job types in the application

## Extensibility Pattern

CJA follows a **Trait → Registry → Macro** pattern:

1. **Trait** — Define behavior (`Job<AppState>` trait)
2. **Registry** — Collect implementations (`JobRegistry<AppState>` trait)
3. **Macro** — Generate the glue (`impl_job_registry!(AppState, Job1, Job2, ...)`)

This pattern makes it impossible to forget registering a new job type — the macro generates a match statement that routes by `Job::NAME`, and unregistered names produce a runtime error.

## Re-exports

CJA re-exports key dependencies so users don't need to add them to their `Cargo.toml`:

```rust
pub use sqlx;
pub use uuid;
pub use color_eyre;
pub use maud;
pub use chrono;
pub use chrono_tz;
```

Use `cja::sqlx` instead of adding `sqlx` as a direct dependency.

## Unsafe Code

Forbidden workspace-wide via `unsafe_code = "forbid"` in `[workspace.lints.rust]`.

## Clippy

Pedantic mode (`deny`) with specific allows. See `Cargo.toml` at workspace root for the full list. The key philosophy: enforce quality without blocking on documentation completeness.
