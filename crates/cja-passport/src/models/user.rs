use sqlx::Row;

#[derive(Clone, Debug, serde::Serialize)]
pub struct User {
    pub user_id: uuid::Uuid,
    pub issuer: String,
    pub subject: String,
    pub email: Option<String>,
    pub preferred_username: Option<String>,
    pub picture: Option<String>,
    pub created_at: jiff::Timestamp,
}

fn decode(row: &sqlx::postgres::PgRow) -> cja::Result<User> {
    Ok(User {
        user_id: row.try_get("user_id")?,
        issuer: row.try_get("issuer")?,
        subject: row.try_get("subject")?,
        email: row.try_get("email")?,
        preferred_username: row.try_get("preferred_username")?,
        picture: row.try_get("picture")?,
        created_at: row
            .try_get::<jiff_sqlx::Timestamp, _>("created_at")?
            .to_jiff(),
    })
}

pub async fn find_by_id(
    executor: impl sqlx::Executor<'_, Database = sqlx::Postgres>,
    id: uuid::Uuid,
) -> cja::Result<Option<User>> {
    sqlx::query("SELECT user_id, issuer, subject, email, preferred_username, picture, created_at FROM passport_users WHERE user_id = $1")
        .bind(id).fetch_optional(executor).await?.as_ref().map(decode).transpose()
}

pub async fn upsert(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    issuer: &str,
    subject: &str,
    email: Option<&str>,
    preferred_username: Option<&str>,
    picture: Option<&str>,
) -> cja::Result<User> {
    let row = sqlx::query(
        "INSERT INTO passport_users (issuer, subject, email, preferred_username, picture)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (issuer, subject) DO UPDATE SET
         email = EXCLUDED.email, preferred_username = EXCLUDED.preferred_username,
         picture = EXCLUDED.picture
         RETURNING user_id, issuer, subject, email, preferred_username, picture, created_at",
    )
    .bind(issuer)
    .bind(subject)
    .bind(email)
    .bind(preferred_username)
    .bind(picture)
    .fetch_one(&mut **tx)
    .await?;
    decode(&row)
}
