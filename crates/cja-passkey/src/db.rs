/// Run cja-passkey migrations.
///
/// Must be called AFTER [`cja::db::run_migrations`] — passkey migrations ALTER the sessions table.
///
/// `SQLx` uses a single `_sqlx_migrations` table per database. Because cja and cja-passkey
/// each ship their own migration directories, we tell the migrator to ignore migration
/// rows it doesn't recognise (i.e. the cja core migrations) so that running both in
/// sequence is allowed.
pub async fn run_migrations(pool: &sqlx::PgPool) -> cja::Result<()> {
    let mut migrator = sqlx::migrate!("./migrations");
    migrator.set_ignore_missing(true);
    migrator.run(pool).await?;
    Ok(())
}
