# Conventions

## Error Handling

- Use `color_eyre::Result` (re-exported as `cja::Result`) as the return type everywhere
- Use `eyre!("message")` for ad-hoc errors
- Use `.wrap_err("context")` to add context to propagated errors
- Never `unwrap()` in library code; use `?` propagation

## Tracing

- Use `#[instrument]` on async functions with structured fields
- Use `info!` for lifecycle events (server start, worker start/stop)
- Use `warn!` for retries and recoverable failures
- Use `error!` for permanent failures
- Skip large fields in `#[instrument]` (e.g., `skip(app_state)`)

## Feature Flags

- **Compile-time:** `#[cfg(feature = "...")]` for optional modules (`postgres`, `jobs`, `cron`) and helpers such as `testing::mock_oauth`
- **Runtime:** `{FEATURE}_DISABLED` env vars (e.g., `SERVER_DISABLED`, `JOBS_DISABLED`, `CRON_DISABLED`)

Defaults enable `postgres`, `jobs`, and `cron`. The chain is `cron → jobs → postgres`.
Without default features, `AppState` needs only `version()` and `cookie_key()`;
`db()`, `db`, `server::session`, and `testing::test_db` require `postgres`.
The `testing` feature independently enables only `testing::mock_oauth`.

## Unsafe Code

Forbidden workspace-wide via `unsafe_code = "forbid"` in `Cargo.toml`.

## Clippy

Pedantic at deny level with these allows:

- `missing_errors_doc` — not all `Result`-returning functions need error docs
- `missing_panics_doc` — same for panics
- `module_name_repetitions` — e.g., `jobs::JobRegistry` is fine
- `blocks_in_conditions` — allows complex conditions with blocks
- `must_use_candidate` — not everything needs `#[must_use]`
- `no_effect_underscore_binding` — allows `let _x = ...` patterns
- `items_after_statements` — allows `let` bindings interspersed with items

## Naming

- `AppState` (not `State`) — avoids confusion with Axum's `State` extractor
- `NamedTask` — for spawned `tokio::task` handles that need identification
- `{Feature}_DISABLED` — env var pattern for runtime feature gating
- Job names are `const NAME: &'static str` — typically the struct name (e.g., `"NoopJob"`)

## Extensibility Pattern: Trait → Registry → Macro

CJA uses a consistent pattern for extensible subsystems:

1. **Trait** — defines the interface (`Job<AS>`, `AppSession`)
2. **Registry** — collects implementations (`CronRegistry`, `JobRegistry`)
3. **Macro** — generates the registry boilerplate (`impl_job_registry!`)

To add a new job type:
1. Define a struct implementing `Job<YourAppState>`
2. Add it to `impl_job_registry!(YourAppState, ExistingJob, NewJob)`
3. For cron scheduling, register it in your `CronRegistry`

## Re-exports

CJA re-exports key dependencies so consumers don't need to add them directly:

- `cja::sqlx` and `cja::jiff_sqlx` with `postgres`
- `cja::uuid`
- `cja::color_eyre`
- `cja::maud`
- `cja::jiff`

Use the re-exports for version consistency.

## Database Conventions

- All tables use `UUID` primary keys
- Timestamps are `TIMESTAMPTZ` (timezone-aware)
- Use `IF NOT EXISTS` in CREATE TABLE statements
- Use `DEFAULT gen_random_uuid()` for auto-generated IDs
- The `sessions` table has an auto-update trigger for `updated_at`

Product analytics uses `cja::product_event!` with a name, literal message, optional
`user_id`, and explicitly typed properties. The macro emits tracing for stdout
and Eyes and queues PostHog capture when `POSTHOG_PROJECT_TOKEN` is set. Ordinary
`event_type` tracing fields are operational and are never captured. The capture
queue is bounded and drops when full. Anonymous events use a fresh distinct ID
and disable person profile processing; identified events use the acting user ID.
`service.name` is reserved and comes from the `setup_tracing` crate name.

Use only an opaque internal ID for `user_id`, never an email, name, or other PII.
Property values, especially `?` Debug output, are sent to PostHog verbatim;
do not pass PII or secrets.
