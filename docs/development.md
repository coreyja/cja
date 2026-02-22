# Development

## Prerequisites

- **Rust** (edition 2024) — install via [rustup](https://rustup.rs/)
- **PostgreSQL** — for job queue, cron state, and sessions
- **systemfd** (optional) — for zero-downtime hot reload during development

## Setup

```bash
git clone https://github.com/coreyja/cja.git
cd cja

# Create a development database
createdb cja_dev

# Set the database URL
export DATABASE_URL="postgres:///cja_dev"

# Build everything
cargo build
```

## Running the Example App

```bash
cargo run -p cja-site
```

This starts the HTTP server on port 3000 (or `$PORT`), the job worker, and the cron worker. Visit `http://localhost:3000` to see a simple page that demonstrates session management.

### With Hot Reload

```bash
cargo install systemfd cargo-watch
systemfd --no-pid -s http::3000 -- cargo watch -x 'run -p cja-site'
```

The server will automatically restart on code changes while keeping the listening socket open (no dropped connections during reloads).

### Disabling Subsystems

Use environment variables to run only the parts you need:

```bash
JOBS_DISABLED=true cargo run -p cja-site    # Server + cron only
CRON_DISABLED=true cargo run -p cja-site    # Server + jobs only
SERVER_DISABLED=true cargo run -p cja-site  # Jobs + cron only (headless worker)
```

## Testing

See `crates/cja/TESTING.md` for the full testing philosophy. Key points:

- Integration tests use real PostgreSQL (no mocking)
- Each test creates an isolated database (`cja_test_<uuid>`)
- Tests default to `postgres://localhost/postgres` for the connection

```bash
# Run all tests
cargo test

# Run integration tests only
cargo test --test lib

# Run a specific test suite
cargo test sessions --test lib
cargo test jobs --test lib
```

**Note:** `sqlx::test` attribute is used for database tests — it handles database creation and cleanup automatically.

## Linting

```bash
# Clippy (pedantic mode, configured in workspace Cargo.toml)
cargo clippy --workspace --all-targets

# Format check
cargo fmt --check
```

Clippy is configured as `deny` for pedantic lints with these specific allows:
- `missing_errors_doc`, `missing_panics_doc` — doc coverage not enforced
- `module_name_repetitions` — allows `server::ServerConfig` style naming
- `blocks_in_conditions`, `must_use_candidate` — ergonomic relaxations
- `no-effect-underscore-binding`, `items-after-statements` — style flexibility

Unsafe code is **forbidden** workspace-wide.

## Database Migrations

### Migration Locations

Migrations exist in multiple locations (this is a known gotcha):

| Location | Used By | Contents |
|----------|---------|----------|
| `crates/cja/migrations/` | `sqlx::migrate!()` macro in `db.rs` | **Source of truth** — all framework migrations |
| `migrations/` (root) | `sqlx migrate run` CLI | May be incomplete |
| `crates/cja.app/migrations/` | Example app's `sqlx::migrate!()` | May lag behind `crates/cja/` |

The `crates/cja/migrations/` directory is the authoritative source. When adding a new migration, copy it to all locations that need it.

### Adding a Migration

```bash
# Create a new migration in the framework
cd crates/cja
sqlx migrate add <MigrationName>

# Edit the generated file
# Then copy to other locations as needed
cp migrations/<timestamp>_<Name>.sql ../../migrations/
cp migrations/<timestamp>_<Name>.sql ../cja.app/migrations/
```

### SQLx Offline Cache

The `.sqlx/` directory contains cached query metadata for offline compilation. When you change SQL queries:

```bash
DATABASE_URL="postgres:///cja_dev" cargo sqlx prepare --workspace -- --all-targets
```

The `--all-targets` flag is important — without it, test-only queries won't be cached and CI will fail on `cargo sqlx prepare --check`.

## Project Structure

```
cja/
├── Cargo.toml              Workspace root
├── CLAUDE.md               AI assistant guidance
├── migrations/             Root-level migrations (CLI use)
├── crates/
│   ├── cja/                Core framework library
│   │   ├── src/            Framework source code
│   │   ├── migrations/     Framework migrations (source of truth)
│   │   ├── tests/          Integration tests
│   │   └── TESTING.md      Testing strategy
│   ├── cja.app/            Example application
│   │   ├── src/main.rs     Full working example
│   │   └── migrations/     App-specific migrations
│   └── cli/                Project scaffolding CLI
│       └── src/            CLI source code
└── docs/                   Developer documentation (you are here)
```
