# Development

## Prerequisites

- **Rust** (edition 2024) — install via [rustup](https://rustup.rs/)
- **PostgreSQL** — any recent version
- **systemfd** (optional) — for zero-downtime hot reload during development

## Setup

```bash
git clone https://github.com/coreyja/cja.git
cd cja
createdb cja_dev
export DATABASE_URL="postgres:///cja_dev"
cargo build
```

## Running the Example App

```bash
export DATABASE_URL="postgres:///cja_dev"
cargo run -p cja-site
```

The app starts the HTTP server on port 3000, a job worker, and a cron worker. Visit `http://localhost:3000` to see it running.

### Hot Reload

For automatic recompilation on file changes:

```bash
cargo install systemfd cargo-watch
systemfd --no-pid -s http::3000 -- cargo watch -x 'run -p cja-site'
```

This uses `systemfd` to pass the TCP listener across restarts, enabling zero-downtime reload.

## Testing

Tests use real PostgreSQL databases (no mocking). Each test creates an isolated database.

```bash
# Run all library and integration tests
cargo test --package cja --lib --test lib
```

**Important:** `cargo test --doc` does not work for this project because `sqlx::query!` macros require a live database connection at compile time, and doctest crates don't inherit the `DATABASE_URL` environment variable. Use `--lib --test lib` instead.

### Lima VM / Unix Socket Setup

If your PostgreSQL uses Unix sockets (common on Linux/Lima VMs), you need two different `DATABASE_URL` formats:

```bash
# For compile-time query! macro validation (cargo build / cargo test --no-run):
DATABASE_URL="postgres:///cja_dev?host=/var/run/postgresql" cargo test --package cja --no-run

# For runtime test execution:
DATABASE_URL="postgres://%2Fvar%2Frun%2Fpostgresql/postgres" cargo test --package cja --lib --test lib -- --test-threads=1
```

The two formats are needed because the test infrastructure's URL parser (`rfind('/')`) breaks on `?host=` query parameters at runtime. Use `--test-threads=1` to avoid database conflicts.

See `crates/cja/TESTING.md` for the full testing philosophy and test infrastructure details.

## Linting

Clippy is configured as pedantic at the deny level. Unsafe code is forbidden workspace-wide.

```bash
cargo clippy --workspace --all-targets
cargo fmt --check
```

Allowed lints (from `Cargo.toml`):
- `missing_errors_doc`, `missing_panics_doc`
- `module_name_repetitions`
- `blocks_in_conditions`
- `must_use_candidate`
- `no_effect_underscore_binding`
- `items_after_statements`

## Database Migrations

### Adding a New Framework Migration

```bash
cd crates/cja
sqlx migrate add MyMigrationName
```

Then copy the new migration file to all three locations:

1. `crates/cja/migrations/` — source of truth (already created by `sqlx migrate add`)
2. `migrations/` (workspace root) — copy for `sqlx` CLI
3. `crates/cja.app/migrations/` — example app

See [Architecture: Migration Locations](architecture.md#migration-locations) for details on why three copies exist.

### SQLx Offline Cache

When updating queries, regenerate the `.sqlx/` offline cache for CI:

```bash
DATABASE_URL="postgres:///cja_dev?host=/var/run/postgresql" cargo sqlx prepare --all --workspace -- --all-targets
```

The `--all-targets` flag is required to include test-only queries. Without it, `cargo sqlx prepare --check` fails in CI.
