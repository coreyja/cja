# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Overview

CJA (Cron, Jobs and Axum) is a Rust web framework for full-stack development that combines background job processing, cron scheduling, and HTTP server functionality built on top of Axum. It's currently in ALPHA status.

## Architecture

The project uses a Cargo workspace with these main crates:
- `crates/cja/` - Core framework library providing jobs, cron, server, and session management
- `crates/cja.app/` - Example application demonstrating the framework
- `crates/tracing-common/` - Shared tracing utilities

### Key Components

**AppState Trait**: Central trait that applications must implement, providing access to database connection, cookie keys, and version info (`crates/cja/src/app_state.rs:3-9`).

**Jobs System**: Background job processing with database persistence. Jobs implement the `Job` trait with serialization support and are queued in PostgreSQL (`crates/cja/src/jobs/mod.rs:16-53`).

**Cron System**: Scheduled task execution using a registry pattern. Cron jobs are regular jobs scheduled at intervals (`crates/cja/src/cron/`).

**Server**: HTTP server built on Axum with cookie management, sessions, and tracing middleware (`crates/cja/src/server/mod.rs:27-75`).

**Sessions**: Database-backed session management with trait-based extension (`crates/cja.app/src/main.rs:120-164`).

## Common Commands

```bash
# Build the project
cargo build

# Run the example application
cargo run -p cja-site

# Run tests
cargo test

# Check for linting issues
cargo clippy

# Format code
cargo fmt
```

## Database

Uses PostgreSQL with SQLx for database operations. Migrations are in `migrations/` directories within each crate. The application automatically runs migrations on startup with advisory locking.

## Environment Variables

- `DATABASE_URL` - PostgreSQL connection string (required)
- `PORT` - Server port (default: 3000)
- `SERVER_DISABLED` - Disable HTTP server (default: false)
- `JOBS_DISABLED` - Disable job worker (default: false) 
- `CRON_DISABLED` - Disable cron worker (default: false)

## Development Notes

- Uses 2024 Rust edition with pedantic Clippy lints
- Forbids unsafe code at workspace level
- Framework supports zero-downtime reloading via systemfd
- Maud for HTML templating
- OpenTelemetry tracing integration
- Color-eyre for error handling