//! Database utilities and migration management.
//!
//! CJA ships with migrations for its core tables (`jobs`, `dead_letter_jobs`,
//! `crons`, `sessions`). Use [`run_migrations`] to apply them on startup.
//!
//! # Migration Locations
//!
//! `crates/cja/migrations/` is the only source of truth; [`run_migrations`]
//! embeds it via `sqlx::migrate!("./migrations")`. The `migrations/` dir at
//! the workspace root and `crates/cja.app/migrations/` are partial,
//! best-effort convenience copies — do not rely on them being complete.
//! CLI-generated apps carry no copies: they call [`run_migrations`] at boot.
//!
//! # Advisory Locking
//!
//! `SQLx` handles advisory locking internally during migrations. Do **not** wrap
//! [`run_migrations`] with custom `pg_advisory_lock`/`pg_advisory_unlock` calls —
//! this is redundant and potentially harmful with connection poolers like `PgBouncer`.
//!
//! # App migrations
//!
//! Apps with their own migrations share the `_sqlx_migrations` table with cja.
//! Run both migrators at boot, each with `set_ignore_missing(true)` so neither
//! rejects the other's rows. Checksums are still verified for versions both
//! know, so an edited copy of a cja migration still fails.
//!
//! ```rust,ignore
//! cja::db::run_migrations(&pool).await?;
//! let mut app = sqlx::migrate!();
//! app.set_ignore_missing(true);
//! app.run(&pool).await?;
//! ```

/// Run CJA's built-in database migrations.
///
/// This applies all framework migrations (jobs, crons, sessions, `dead_letter_jobs`)
/// using `SQLx`'s built-in advisory locking for safe concurrent startup.
///
/// Tolerates foreign rows in `_sqlx_migrations` (your app's own migrations,
/// or those of sibling crates like `cja-passkey`), so this composes with
/// other migrators over the same table — see the module-level docs
/// ("App migrations").
///
/// Two protections are traded away for that tolerance: rows recorded by a
/// NEWER cja are also ignored, so booting an older binary against a database
/// migrated by a newer cja is no longer refused at boot (the older binary
/// simply leaves the newer rows alone); and a deleted cja migration (a
/// version recorded in the database whose file was removed from
/// `crates/cja/migrations/`) is no longer detected. Checksum verification for
/// versions both sides know still applies, so an edited copy of a cja
/// migration still fails loudly.
pub async fn run_migrations(db_pool: &sqlx::PgPool) -> crate::Result<()> {
    let mut migrator = sqlx::migrate!("./migrations");
    migrator.set_ignore_missing(true);
    migrator.run(db_pool).await?;
    Ok(())
}
