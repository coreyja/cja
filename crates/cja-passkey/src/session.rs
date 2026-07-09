use cja::server::session::{AppSession, CJASession};
use sqlx::Row;

#[derive(Debug, Clone)]
pub struct PasskeySession {
    inner: CJASession,
    pub user_id: Option<uuid::Uuid>,
    pub challenge_state: Option<serde_json::Value>,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
pub enum ChallengeState {
    #[serde(rename = "registration")]
    Registration {
        registration_state: webauthn_rs::prelude::PasskeyRegistration,
        user_unique_id: uuid::Uuid,
        username: String,
        display_name: Option<String>,
    },
    #[serde(rename = "authentication")]
    Authentication {
        auth_state: webauthn_rs::prelude::PasskeyAuthentication,
        user_id: uuid::Uuid,
    },
}

#[async_trait::async_trait]
impl AppSession for PasskeySession {
    async fn from_db(pool: &sqlx::PgPool, session_id: uuid::Uuid) -> cja::Result<Self> {
        let row = sqlx::query(
            "SELECT session_id, created_at, updated_at, user_id, challenge_state \
             FROM sessions WHERE session_id = $1",
        )
        .bind(session_id)
        .fetch_one(pool)
        .await?;
        let inner = CJASession {
            session_id: row.try_get("session_id")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        };
        let user_id: Option<uuid::Uuid> = row.try_get("user_id")?;
        let challenge_state: Option<serde_json::Value> = row.try_get("challenge_state")?;
        Ok(Self {
            inner,
            user_id,
            challenge_state,
        })
    }

    async fn create(pool: &sqlx::PgPool) -> cja::Result<Self> {
        let row = sqlx::query(
            "INSERT INTO sessions DEFAULT VALUES \
             RETURNING session_id, created_at, updated_at, user_id, challenge_state",
        )
        .fetch_one(pool)
        .await?;
        let inner = CJASession {
            session_id: row.try_get("session_id")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        };
        let user_id: Option<uuid::Uuid> = row.try_get("user_id")?;
        let challenge_state: Option<serde_json::Value> = row.try_get("challenge_state")?;
        Ok(Self {
            inner,
            user_id,
            challenge_state,
        })
    }

    fn from_inner(inner: CJASession) -> Self {
        Self {
            inner,
            user_id: None,
            challenge_state: None,
        }
    }

    fn inner(&self) -> &CJASession {
        &self.inner
    }
}

impl PasskeySession {
    pub async fn set_user_id(
        pool: &sqlx::PgPool,
        session_id: uuid::Uuid,
        user_id: uuid::Uuid,
    ) -> cja::Result<()> {
        sqlx::query("UPDATE sessions SET user_id = $1 WHERE session_id = $2")
            .bind(user_id)
            .bind(session_id)
            .execute(pool)
            .await?;
        Ok(())
    }

    pub async fn set_challenge_state(
        pool: &sqlx::PgPool,
        session_id: uuid::Uuid,
        state: serde_json::Value,
    ) -> cja::Result<()> {
        sqlx::query("UPDATE sessions SET challenge_state = $1::jsonb WHERE session_id = $2")
            .bind(state)
            .bind(session_id)
            .execute(pool)
            .await?;
        Ok(())
    }

    pub async fn clear_challenge_state(
        pool: &sqlx::PgPool,
        session_id: uuid::Uuid,
    ) -> cja::Result<()> {
        sqlx::query("UPDATE sessions SET challenge_state = NULL WHERE session_id = $1")
            .bind(session_id)
            .execute(pool)
            .await?;
        Ok(())
    }
}
