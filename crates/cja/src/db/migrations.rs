//! Migration runner for cja applications.
//!
//! Inspired by sqlx-cli and Rails migrations, this module provides a simple
//! way to manage database schema changes using raw SQL files.

use std::path::Path;
use thiserror::Error;

/// Error type for migration operations.
#[derive(Debug, Error)]
pub enum MigrationError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Invalid migration filename: {0}")]
    InvalidFilename(String),

    #[error("Database error: {0}")]
    Database(#[from] tokio_postgres::Error),

    #[error("Migration {version} ({name}) failed: {message}")]
    MigrationFailed {
        version: i64,
        name: String,
        message: String,
    },
}

/// A single migration loaded from a file or embedded at compile time.
#[derive(Debug, Clone)]
pub struct Migration {
    /// The version number (timestamp) of the migration
    pub version: i64,
    /// The name of the migration (without version prefix)
    pub name: String,
    /// The SQL to execute for the "up" migration
    pub up_sql: String,
    /// The SQL to execute for the "down" migration (optional)
    pub down_sql: Option<String>,
}

/// Source for embedding migrations at compile time.
#[derive(Debug, Clone)]
pub struct MigrationSource {
    pub version: i64,
    pub name: &'static str,
    pub sql: &'static str,
}

/// Migration runner that loads and executes SQL migrations.
#[derive(Debug, Clone)]
pub struct Migrator {
    migrations: Vec<Migration>,
}

impl Migrator {
    /// Create a new empty migrator.
    pub fn new() -> Self {
        Self {
            migrations: Vec::new(),
        }
    }

    /// Create a migrator from embedded migration sources.
    ///
    /// This is useful for embedding migrations at compile time.
    pub fn from_embedded(sources: &[MigrationSource]) -> Self {
        let migrations = sources
            .iter()
            .map(|s| Migration {
                version: s.version,
                name: s.name.to_string(),
                up_sql: s.sql.to_string(),
                down_sql: None,
            })
            .collect();

        Self { migrations }
    }

    /// Load migrations from a directory path.
    ///
    /// Looks for files matching the patterns:
    /// - `YYYYMMDDHHMMSS_name.sql`
    /// - `YYYYMMDDHHMMSS_name.up.sql` / `YYYYMMDDHHMMSS_name.down.sql`
    pub fn from_path<P: AsRef<Path>>(path: P) -> Result<Self, MigrationError> {
        let path = path.as_ref();
        let mut migrations: std::collections::HashMap<i64, Migration> =
            std::collections::HashMap::new();

        if !path.exists() {
            return Ok(Self::new());
        }

        let mut entries: Vec<_> = std::fs::read_dir(path)?.filter_map(Result::ok).collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);

        for entry in entries {
            let file_name = entry.file_name();
            let file_name_str = file_name.to_string_lossy();

            // Skip non-SQL files
            if !file_name_str.ends_with(".sql") {
                continue;
            }

            let (version, name, is_down) = parse_migration_filename(&file_name_str)?;
            let sql = std::fs::read_to_string(entry.path())?;

            let migration = migrations.entry(version).or_insert_with(|| Migration {
                version,
                name: name.clone(),
                up_sql: String::new(),
                down_sql: None,
            });

            if is_down {
                migration.down_sql = Some(sql);
            } else {
                migration.up_sql = sql;
            }
        }

        let mut migrations: Vec<_> = migrations.into_values().collect();
        migrations.sort_by_key(|m| m.version);

        Ok(Self { migrations })
    }

    /// Add a migration to this migrator.
    pub fn add_migration(&mut self, migration: Migration) {
        self.migrations.push(migration);
        self.migrations.sort_by_key(|m| m.version);
    }

    /// Get all migrations in this migrator.
    pub fn migrations(&self) -> &[Migration] {
        &self.migrations
    }

    /// Run all pending migrations against the database.
    ///
    /// This will:
    /// 1. Create the `_cja_migrations` table if it doesn't exist
    /// 2. Determine which migrations have already been applied
    /// 3. Run any pending migrations in order
    pub async fn run(&self, client: &tokio_postgres::Client) -> Result<(), MigrationError> {
        // Create migrations tracking table
        client
            .execute(
                "
                CREATE TABLE IF NOT EXISTS _cja_migrations (
                    version BIGINT PRIMARY KEY,
                    name TEXT NOT NULL,
                    applied_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
                )
                ",
                &[],
            )
            .await?;

        // Get already-applied migrations
        let applied: std::collections::HashSet<i64> = client
            .query("SELECT version FROM _cja_migrations", &[])
            .await?
            .iter()
            .map(|row| row.get(0))
            .collect();

        // Run pending migrations
        for migration in &self.migrations {
            if applied.contains(&migration.version) {
                tracing::debug!(
                    version = migration.version,
                    name = %migration.name,
                    "Migration already applied, skipping"
                );
                continue;
            }

            tracing::info!(
                version = migration.version,
                name = %migration.name,
                "Running migration"
            );

            // Execute the migration SQL
            if let Err(e) = client.batch_execute(&migration.up_sql).await {
                return Err(MigrationError::MigrationFailed {
                    version: migration.version,
                    name: migration.name.clone(),
                    message: e.to_string(),
                });
            }

            // Record the migration as applied
            client
                .execute(
                    "INSERT INTO _cja_migrations (version, name) VALUES ($1, $2)",
                    &[&migration.version, &migration.name],
                )
                .await?;

            tracing::info!(
                version = migration.version,
                name = %migration.name,
                "Migration completed"
            );
        }

        Ok(())
    }

    /// Rollback the last applied migration.
    ///
    /// Returns an error if the migration doesn't have a down script.
    pub async fn rollback(&self, client: &tokio_postgres::Client) -> Result<(), MigrationError> {
        // Get the last applied migration
        let row = client
            .query_opt(
                "SELECT version, name FROM _cja_migrations ORDER BY version DESC LIMIT 1",
                &[],
            )
            .await?;

        let Some(row) = row else {
            tracing::info!("No migrations to rollback");
            return Ok(());
        };

        let version: i64 = row.get(0);
        let name: String = row.get(1);

        // Find the migration
        let migration = self
            .migrations
            .iter()
            .find(|m| m.version == version)
            .ok_or_else(|| MigrationError::MigrationFailed {
                version,
                name: name.clone(),
                message: "Migration not found in migrator".to_string(),
            })?;

        let down_sql =
            migration
                .down_sql
                .as_ref()
                .ok_or_else(|| MigrationError::MigrationFailed {
                    version,
                    name: name.clone(),
                    message: "No down migration available".to_string(),
                })?;

        tracing::info!(version, name = %name, "Rolling back migration");

        // Execute the down migration
        if let Err(e) = client.batch_execute(down_sql).await {
            return Err(MigrationError::MigrationFailed {
                version,
                name,
                message: e.to_string(),
            });
        }

        // Remove from tracking table
        client
            .execute(
                "DELETE FROM _cja_migrations WHERE version = $1",
                &[&version],
            )
            .await?;

        tracing::info!(version, name = %migration.name, "Rollback completed");

        Ok(())
    }

    /// Get the list of pending migrations (not yet applied).
    pub async fn pending(
        &self,
        client: &tokio_postgres::Client,
    ) -> Result<Vec<&Migration>, MigrationError> {
        // Create table if it doesn't exist (for checking status before running)
        client
            .execute(
                "
                CREATE TABLE IF NOT EXISTS _cja_migrations (
                    version BIGINT PRIMARY KEY,
                    name TEXT NOT NULL,
                    applied_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
                )
                ",
                &[],
            )
            .await?;

        let applied: std::collections::HashSet<i64> = client
            .query("SELECT version FROM _cja_migrations", &[])
            .await?
            .iter()
            .map(|row| row.get(0))
            .collect();

        Ok(self
            .migrations
            .iter()
            .filter(|m| !applied.contains(&m.version))
            .collect())
    }
}

impl Default for Migrator {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse a migration filename to extract version, name, and whether it's a down migration.
fn parse_migration_filename(filename: &str) -> Result<(i64, String, bool), MigrationError> {
    use std::path::Path;

    // Remove .sql extension
    let without_sql = filename.trim_end_matches(".sql");

    // Check for .up or .down suffix using Path for case-insensitive comparison
    let path = Path::new(without_sql);
    let (base, is_down) = if path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("up"))
    {
        (without_sql.trim_end_matches(".up"), false)
    } else if path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("down"))
    {
        (without_sql.trim_end_matches(".down"), true)
    } else {
        (without_sql, false)
    };

    // Split on underscore to get version and name
    let parts: Vec<&str> = base.splitn(2, '_').collect();
    if parts.len() != 2 {
        return Err(MigrationError::InvalidFilename(filename.to_string()));
    }

    let version: i64 = parts[0]
        .parse()
        .map_err(|_| MigrationError::InvalidFilename(filename.to_string()))?;

    let name = parts[1].to_string();

    Ok((version, name, is_down))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_migration_filename_simple() {
        let (version, name, is_down) =
            parse_migration_filename("20240101000000_create_users.sql").unwrap();
        assert_eq!(version, 20240101000000);
        assert_eq!(name, "create_users");
        assert!(!is_down);
    }

    #[test]
    fn test_parse_migration_filename_up() {
        let (version, name, is_down) =
            parse_migration_filename("20240101000000_create_users.up.sql").unwrap();
        assert_eq!(version, 20240101000000);
        assert_eq!(name, "create_users");
        assert!(!is_down);
    }

    #[test]
    fn test_parse_migration_filename_down() {
        let (version, name, is_down) =
            parse_migration_filename("20240101000000_create_users.down.sql").unwrap();
        assert_eq!(version, 20240101000000);
        assert_eq!(name, "create_users");
        assert!(is_down);
    }

    #[test]
    fn test_parse_migration_filename_invalid() {
        assert!(parse_migration_filename("invalid.sql").is_err());
        assert!(parse_migration_filename("nounderscore.sql").is_err());
    }
}
