//! Database utilities including migrations support.
//!
//! This module provides migration support similar to sqlx-cli or Rails,
//! using raw SQL files stored in a migrations directory.
//!
//! # Migration File Format
//!
//! Migrations are stored as SQL files in a `migrations/` directory. Files can be:
//!
//! - `YYYYMMDDHHMMSS_name.sql` - Single migration file (up only)
//! - `YYYYMMDDHHMMSS_name.up.sql` - Up migration (for reversible migrations)
//! - `YYYYMMDDHHMMSS_name.down.sql` - Down migration (optional)
//!
//! The timestamp prefix ensures migrations run in chronological order.
//!
//! # Example
//!
//! ```rust,no_run
//! use cja::db::{Migrator, MigrationSource};
//!
//! # async fn example() -> cja::Result<()> {
//! // Load migrations from a directory
//! let migrator = Migrator::from_path("./migrations")?;
//!
//! // Or embed migrations at compile time
//! let migrator = Migrator::from_embedded(&[
//!     MigrationSource {
//!         version: 20240101000000,
//!         name: "create_users",
//!         sql: "CREATE TABLE users (id SERIAL PRIMARY KEY, name TEXT NOT NULL);",
//!     },
//! ]);
//!
//! // Run migrations against a database connection
//! let client = todo!(); // Get a tokio_postgres::Client
//! migrator.run(&client).await?;
//! # Ok(())
//! # }
//! ```

mod migrations;

pub use migrations::*;

use crate::app_state::DbPool;

/// Run database migrations.
///
/// Note: Migration support is being reworked. For now, users should run migrations
/// directly using their migration tool of choice (e.g., refinery, sqlx-cli, or raw SQL).
pub fn run_migrations(_db_pool: &DbPool) -> crate::Result<()> {
    // TODO: Implement migration support with refinery or similar
    // For now, migrations should be run externally
    tracing::warn!("run_migrations is a no-op - run migrations with your migration tool");
    Ok(())
}
