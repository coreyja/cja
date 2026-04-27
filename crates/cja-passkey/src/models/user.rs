use sqlx::Row;

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct User {
    pub user_id: uuid::Uuid,
    pub username: String,
    pub display_name: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

fn row_to_user(row: &sqlx::postgres::PgRow) -> sqlx::Result<User> {
    Ok(User {
        user_id: row.try_get("user_id")?,
        username: row.try_get("username")?,
        display_name: row.try_get("display_name")?,
        created_at: row.try_get("created_at")?,
    })
}

pub async fn find_by_id(
    executor: impl sqlx::Executor<'_, Database = sqlx::Postgres>,
    user_id: uuid::Uuid,
) -> cja::Result<Option<User>> {
    let row = sqlx::query(
        "SELECT user_id, username, display_name, created_at \
         FROM passkey_users WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_optional(executor)
    .await?;
    row.map(|r| row_to_user(&r)).transpose().map_err(Into::into)
}

pub async fn find_by_username(
    executor: impl sqlx::Executor<'_, Database = sqlx::Postgres>,
    username: &str,
) -> cja::Result<Option<User>> {
    let row = sqlx::query(
        "SELECT user_id, username, display_name, created_at \
         FROM passkey_users WHERE username = $1",
    )
    .bind(username)
    .fetch_optional(executor)
    .await?;
    row.map(|r| row_to_user(&r)).transpose().map_err(Into::into)
}

/// Creates a user with the explicitly provided `user_id`.
///
/// `user_id` is supplied by the caller (it comes from the registration challenge state)
/// rather than auto-generated, so the `WebAuthn` user handle remains consistent across
/// the registration ceremony.
pub async fn create(
    executor: impl sqlx::Executor<'_, Database = sqlx::Postgres>,
    user_id: uuid::Uuid,
    username: &str,
    display_name: Option<&str>,
) -> cja::Result<User> {
    let row = sqlx::query(
        "INSERT INTO passkey_users (user_id, username, display_name) \
         VALUES ($1, $2, $3) \
         RETURNING user_id, username, display_name, created_at",
    )
    .bind(user_id)
    .bind(username)
    .bind(display_name)
    .fetch_one(executor)
    .await?;
    row_to_user(&row).map_err(Into::into)
}

pub async fn username_exists(
    executor: impl sqlx::Executor<'_, Database = sqlx::Postgres>,
    username: &str,
) -> cja::Result<bool> {
    let row = sqlx::query("SELECT 1 AS exists_marker FROM passkey_users WHERE username = $1")
        .bind(username)
        .fetch_optional(executor)
        .await?;
    Ok(row.is_some())
}
