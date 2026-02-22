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

See [`crates/cja/README.md`](crates/cja/README.md) for the full user guide covering jobs, cron, server, sessions, and configuration.

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

## Documentation

- **[User Guide](crates/cja/README.md)** — Getting started, configuration, job system, cron, server
- **[Architecture](docs/architecture.md)** — System design, module tree, data flow, database schema
- **[Development](docs/development.md)** — Setup, testing, linting, migrations
- **[Conventions](docs/conventions.md)** — Error handling, tracing, feature flags, naming

## Tech Stack

- Rust 2024 edition
- PostgreSQL + SQLx (compile-time checked queries)
- Axum (HTTP framework)
- Maud (HTML templating)
- color-eyre (error handling)
- OpenTelemetry (distributed tracing)
