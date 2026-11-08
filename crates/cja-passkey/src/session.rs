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
    #[serde(rename = "discoverable_authentication")]
    DiscoverableAuthentication {
        auth_state: webauthn_rs::prelude::DiscoverableAuthentication,
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

    /// Lock the session row and read its challenge state, making the WebAuthn
    /// ceremony single-use: the row stays locked until the caller commits
    /// (consuming the challenge via [`Self::rotate_session_id`]) or rolls back
    /// (leaving the challenge untouched for a retry). Nothing is consumed by
    /// this call itself — the pattern is lock, then verify, then rotate; commit
    /// consumes, rollback preserves.
    ///
    /// Returns `None` when the session row is gone or no challenge is set —
    /// the ceremony was already consumed (or never started); callers return 400.
    ///
    /// Must run inside the same transaction as the subsequent
    /// [`Self::rotate_session_id`]; pass `&mut *tx` as the executor.
    pub async fn lock_challenge_state(
        executor: impl sqlx::Executor<'_, Database = sqlx::Postgres>,
        session_id: uuid::Uuid,
    ) -> cja::Result<Option<serde_json::Value>> {
        let row = sqlx::query(
            "SELECT challenge_state FROM sessions \
             WHERE session_id = $1 AND challenge_state IS NOT NULL \
             FOR UPDATE",
        )
        .bind(session_id)
        .fetch_optional(executor)
        .await?;
        match row {
            Some(row) => Ok(Some(row.try_get("challenge_state")?)),
            None => Ok(None),
        }
    }

    /// Replace the session with a fresh authenticated row and bind it to
    /// `user_id`, consuming the challenge. The old session row is DELETED —
    /// consumer rows referencing `sessions(session_id)` must therefore use
    /// `ON DELETE CASCADE` (see the README framework contract) — and a new row
    /// is inserted with a new primary key, `challenge_state` NULL, `created_at`
    /// carried over, and `updated_at` set to now by its default.
    ///
    /// Call inside the transaction that locked the row via
    /// [`Self::lock_challenge_state`]: the row lock guarantees exactly one
    /// rotation per ceremony and makes the pre-rotation session id permanently
    /// invalid (its row no longer exists).
    pub async fn rotate_session_id(
        executor: impl sqlx::Executor<'_, Database = sqlx::Postgres>,
        session_id: uuid::Uuid,
        user_id: uuid::Uuid,
    ) -> cja::Result<CJASession> {
        let row = sqlx::query(
            "WITH deleted AS ( \
                 DELETE FROM sessions \
                 WHERE session_id = $1 AND challenge_state IS NOT NULL \
                 RETURNING created_at \
             ) \
             INSERT INTO sessions (user_id, challenge_state, created_at) \
             SELECT $2, NULL, created_at FROM deleted \
             RETURNING session_id, created_at, updated_at",
        )
        .bind(session_id)
        .bind(user_id)
        .fetch_one(executor)
        .await?;
        Ok(CJASession {
            session_id: row.try_get("session_id")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}
