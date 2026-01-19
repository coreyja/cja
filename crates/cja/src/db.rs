use crate::app_state::DbPool;

/// Run database migrations.
///
/// Note: Migration support is being reworked. For now, users should run migrations
/// directly using their migration tool of choice (e.g., refinery, sqlx-cli, or raw SQL).
pub async fn run_migrations(_db_pool: &DbPool) -> crate::Result<()> {
    // TODO: Implement migration support with refinery or similar
    // For now, migrations should be run externally
    tracing::warn!("run_migrations is a no-op - run migrations with your migration tool");
    Ok(())
}
