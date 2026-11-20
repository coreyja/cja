use sqlx::PgPool;

pub async fn run_migrations(pool: &PgPool) -> cja::Result<()> {
    cja::db::run_migrations(pool).await?;
    let mut migrator = sqlx::migrate!("./migrations");
    migrator.set_ignore_missing(true);
    migrator.run(pool).await?;
    Ok(())
}

pub async fn prune_auth_artifacts(pool: &PgPool) -> cja::Result<u64> {
    let mut count = 0;
    for query in [
        "DELETE FROM passport_oauth_login_transactions WHERE expires_at <= now()",
        "DELETE FROM passport_oauth_pending_consents WHERE expires_at <= now()",
        "DELETE FROM passport_oauth_authorization_codes WHERE expires_at <= now()",
        "DELETE FROM passport_oauth_access_tokens WHERE expires_at <= now()",
    ] {
        count += sqlx::query(query).execute(pool).await?.rows_affected();
    }
    count += sqlx::query("DELETE FROM passport_oauth_refresh_tokens WHERE expires_at <= now() AND consumed_at IS NULL")
        .execute(pool).await?.rows_affected();
    count += sqlx::query("DELETE FROM passport_oauth_grants WHERE expires_at <= now()")
        .execute(pool)
        .await?
        .rows_affected();
    count += sqlx::query("DELETE FROM passport_oauth_clients c WHERE GREATEST(c.created_at,COALESCE(c.last_used_at,c.created_at)) < now()-interval '6 hours' AND NOT EXISTS (SELECT 1 FROM passport_oauth_grants g WHERE g.oauth_client_id=c.id) AND NOT EXISTS (SELECT 1 FROM passport_oauth_login_transactions t WHERE t.oauth_client_id=c.id) AND NOT EXISTS (SELECT 1 FROM passport_oauth_consents s WHERE s.oauth_client_id=c.id) AND NOT EXISTS (SELECT 1 FROM passport_oauth_pending_consents p WHERE p.oauth_client_id=c.id)")
        .execute(pool).await?.rows_affected();
    let attempts = sqlx::query("DELETE FROM passport_login_attempts WHERE expires_at <= now()")
        .execute(pool)
        .await?
        .rows_affected();
    let handoffs = sqlx::query("DELETE FROM passport_handoffs WHERE expires_at <= now()")
        .execute(pool)
        .await?
        .rows_affected();
    Ok(count + attempts + handoffs)
}
