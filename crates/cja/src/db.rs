pub async fn run_migrations(db_pool: &sqlx::PgPool) -> crate::Result<()> {
    sqlx::migrate!("./migrations").run(db_pool).await?;
    Ok(())
}
