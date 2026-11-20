use sqlx::PgPool;

pub async fn run_migrations(pool: &PgPool) -> cja::Result<()> {
    cja::db::run_migrations(pool).await?;
    let mut migrator = sqlx::migrate!("./migrations");
    migrator.set_ignore_missing(true);
    migrator.run(pool).await?;
    Ok(())
}

pub async fn prune_auth_artifacts(pool: &PgPool) -> cja::Result<u64> {
    let attempts = sqlx::query("DELETE FROM passport_login_attempts WHERE expires_at <= now()")
        .execute(pool)
        .await?
        .rows_affected();
    let handoffs = sqlx::query("DELETE FROM passport_handoffs WHERE expires_at <= now()")
        .execute(pool)
        .await?
        .rows_affected();
    Ok(attempts + handoffs)
}
