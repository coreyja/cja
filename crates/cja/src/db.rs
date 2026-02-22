//! Database utilities and migration management.
//!
//! CJA ships with migrations for its core tables (`jobs`, `dead_letter_jobs`,
//! `crons`, `sessions`). Use [`run_migrations`] to apply them on startup.
//!
//! # Migration Locations
//!
//! Migrations exist in multiple locations in the repository:
//!
//! | Location | Purpose |
//! |----------|---------|
//! | `crates/cja/migrations/` | **Source of truth** — used by [`run_migrations`] |
//! | `migrations/` (root) | Copy for `sqlx migrate run` CLI |
//! | `crates/cja.app/migrations/` | Example app's copy |
//!
//! When adding a new framework migration, copy it to all three locations.
//!
//! # Advisory Locking
//!
//! `SQLx` handles advisory locking internally during migrations. Do **not** wrap
//! [`run_migrations`] with custom `pg_advisory_lock`/`pg_advisory_unlock` calls —
//! this is redundant and potentially harmful with connection poolers like `PgBouncer`.

/// Run CJA's built-in database migrations.
///
/// This applies all framework migrations (jobs, crons, sessions, `dead_letter_jobs`)
/// using `SQLx`'s built-in advisory locking for safe concurrent startup.
pub async fn run_migrations(db_pool: &sqlx::PgPool) -> crate::Result<()> {
    sqlx::migrate!("./migrations").run(db_pool).await?;
    Ok(())
}
