use sqlx::Row;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PasskeyCredential {
    pub credential_id_pk: uuid::Uuid,
    pub user_id: uuid::Uuid,
    pub credential_json: serde_json::Value,
    pub credential_id: Vec<u8>,
    pub name: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub last_used_at: Option<chrono::DateTime<chrono::Utc>>,
}

fn row_to_credential(row: &sqlx::postgres::PgRow) -> sqlx::Result<PasskeyCredential> {
    Ok(PasskeyCredential {
        credential_id_pk: row.try_get("credential_id_pk")?,
        user_id: row.try_get("user_id")?,
        credential_json: row.try_get("credential_json")?,
        credential_id: row.try_get("credential_id")?,
        name: row.try_get("name")?,
        created_at: row.try_get("created_at")?,
        last_used_at: row.try_get("last_used_at")?,
    })
}

pub async fn list_for_user(
    executor: impl sqlx::Executor<'_, Database = sqlx::Postgres>,
    user_id: uuid::Uuid,
) -> cja::Result<Vec<PasskeyCredential>> {
    let rows = sqlx::query(
        "SELECT credential_id_pk, user_id, credential_json, credential_id, name, created_at, last_used_at \
         FROM passkey_credentials WHERE user_id = $1 ORDER BY created_at",
    )
    .bind(user_id)
    .fetch_all(executor)
    .await?;
    rows.iter()
        .map(row_to_credential)
        .collect::<sqlx::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub async fn find_by_credential_id(
    executor: impl sqlx::Executor<'_, Database = sqlx::Postgres>,
    credential_id: &[u8],
) -> cja::Result<Option<PasskeyCredential>> {
    let row = sqlx::query(
        "SELECT credential_id_pk, user_id, credential_json, credential_id, name, created_at, last_used_at \
         FROM passkey_credentials WHERE credential_id = $1",
    )
    .bind(credential_id)
    .fetch_optional(executor)
    .await?;
    row.map(|r| row_to_credential(&r))
        .transpose()
        .map_err(Into::into)
}

pub async fn create(
    executor: impl sqlx::Executor<'_, Database = sqlx::Postgres>,
    user_id: uuid::Uuid,
    credential_json: serde_json::Value,
    credential_id: &[u8],
    name: Option<&str>,
) -> cja::Result<PasskeyCredential> {
    // sqlx::query!() strips the ::jsonb cast, so we use the non-macro variant.
    let row = sqlx::query(
        "INSERT INTO passkey_credentials (user_id, credential_json, credential_id, name) \
         VALUES ($1, $2::jsonb, $3, $4) \
         RETURNING credential_id_pk, user_id, credential_json, credential_id, name, created_at, last_used_at",
    )
    .bind(user_id)
    .bind(credential_json)
    .bind(credential_id)
    .bind(name)
    .fetch_one(executor)
    .await?;
    row_to_credential(&row).map_err(Into::into)
}

/// Updates both the JSON blob (counter/backup state) and `last_used_at`.
pub async fn update_after_auth(
    executor: impl sqlx::Executor<'_, Database = sqlx::Postgres>,
    credential_id_pk: uuid::Uuid,
    credential_json: serde_json::Value,
    last_used_at: chrono::DateTime<chrono::Utc>,
) -> cja::Result<()> {
    sqlx::query(
        "UPDATE passkey_credentials \
         SET credential_json = $1::jsonb, last_used_at = $2 \
         WHERE credential_id_pk = $3",
    )
    .bind(credential_json)
    .bind(last_used_at)
    .bind(credential_id_pk)
    .execute(executor)
    .await?;
    Ok(())
}

/// Updates only `last_used_at`. Used when [`webauthn_rs::prelude::Passkey::update_credential`]
/// returns `Some(false)` (no state change required).
pub async fn update_last_used(
    executor: impl sqlx::Executor<'_, Database = sqlx::Postgres>,
    credential_id_pk: uuid::Uuid,
) -> cja::Result<()> {
    sqlx::query("UPDATE passkey_credentials SET last_used_at = now() WHERE credential_id_pk = $1")
        .bind(credential_id_pk)
        .execute(executor)
        .await?;
    Ok(())
}
