# CJA — Cron, Jobs and Axum

> **ALPHA** — APIs may change between releases. Use in production at your own risk.

A Rust web framework for full-stack development that combines background job processing, cron scheduling, and an HTTP server built on [Axum](https://github.com/tokio-rs/axum).

## Features

- **Background Jobs** — PostgreSQL-backed queue with automatic retries, exponential backoff, priority scheduling, dead letter queue, and graceful shutdown
- **Cron Scheduling** — Interval-based and cron expression scheduling with timezone support
- **HTTP Server** — Axum with encrypted cookies, database-backed sessions, and zero-downtime reload
- **Integrated Observability** — Structured tracing with Sentry, Honeycomb, and Eyes support
- **Type Safety** — Pedantic Clippy lints, no unsafe code, compile-time SQL checking via SQLx

## Quick Start

```bash
cargo add cja
```

```rust
use cja::app_state::AppState;
use cja::server::cookies::CookieKey;

#[derive(Clone)]
struct App {
    db: sqlx::PgPool,
    cookie_key: CookieKey,
}

impl AppState for App {
    fn version(&self) -> &str { env!("CARGO_PKG_VERSION") }
    fn db(&self) -> &sqlx::PgPool { &self.db }
    fn cookie_key(&self) -> &CookieKey { &self.cookie_key }
}
```

## Documentation

- **[User Guide](crates/cja/README.md)** — Getting started, configuration, usage examples (also renders on crates.io)
- **[Architecture](docs/architecture.md)** — System design, module tree, database schema, data flow
- **[Development](docs/development.md)** — Setup, testing, hot reload, migrations
- **[Conventions](docs/conventions.md)** — Error handling, tracing, clippy, naming, extensibility patterns

API docs via `cargo doc`:

```bash
cargo doc --open -p cja
```

## Example Application

The [`crates/cja.app/`](crates/cja.app/) crate is a complete working example demonstrating all features — jobs, cron, sessions, and the HTTP server.

```bash
export DATABASE_URL="postgres:///cja_dev"
cargo run -p cja-site
```

## Project Scaffolding

Generate a new CJA project with the CLI:

```bash
cargo install cja-cli
cja new my-project
```

Supports `--no-jobs`, `--no-cron`, and `--no-sessions` flags.

## Development

### Prerequisites

- Rust (edition 2024) — install via [rustup](https://rustup.rs/)
- PostgreSQL
- systemfd (optional, for zero-downtime hot reload)

### Setup

```bash
git clone https://github.com/coreyja/cja.git
cd cja
createdb cja_dev
export DATABASE_URL="postgres:///cja_dev"
cargo build
```

### Testing

```bash
cargo test
```

See `crates/cja/TESTING.md` for the full testing philosophy. Tests use real
PostgreSQL with isolated databases per test.

### Linting

Clippy is configured as pedantic (deny level). Unsafe code is forbidden workspace-wide.

```bash
cargo clippy --workspace --all-targets
cargo fmt --check
```

## Tech Stack

- Rust 2024 edition
- PostgreSQL + SQLx (compile-time checked queries)
- Axum (HTTP framework)
- Maud (HTML templating)
- color-eyre (error handling)
- OpenTelemetry (distributed tracing)
