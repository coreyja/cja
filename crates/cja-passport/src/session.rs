use std::time::Duration;

use cja::server::session::{AppSession, CJASession};
use sqlx::Row;
use uuid::Uuid;

use crate::{
    config::{AllowedSubject, PassportConfig},
    models::user::{self, User},
    oidc::{self, TokenFailure},
};

#[derive(Debug, Clone)]
pub struct PassportSession {
    inner: CJASession,
    pub user_id: Option<Uuid>,
}

#[async_trait::async_trait]
impl AppSession for PassportSession {
    async fn from_db(pool: &sqlx::PgPool, id: Uuid) -> cja::Result<Option<Self>> {
        let row = sqlx::query(
            "SELECT s.session_id, s.created_at, s.updated_at, p.user_id
             FROM sessions s LEFT JOIN passport_sessions p USING (session_id)
             WHERE s.session_id = $1",
        )
        .bind(id)
        .fetch_optional(pool)
        .await?;
        row.map(|row| -> cja::Result<Self> {
            Ok(Self {
                inner: CJASession {
                    session_id: row.try_get("session_id")?,
                    created_at: row
                        .try_get::<jiff_sqlx::Timestamp, _>("created_at")?
                        .to_jiff(),
                    updated_at: row
                        .try_get::<jiff_sqlx::Timestamp, _>("updated_at")?
                        .to_jiff(),
                },
                user_id: row.try_get("user_id")?,
            })
        })
        .transpose()
    }

    async fn create(pool: &sqlx::PgPool) -> cja::Result<Self> {
        let inner = sqlx::query_as::<_, CJASession>(
            "INSERT INTO sessions DEFAULT VALUES RETURNING session_id, created_at, updated_at",
        )
        .fetch_one(pool)
        .await?;
        Ok(Self {
            inner,
            user_id: None,
        })
    }

    fn from_inner(inner: CJASession) -> Self {
        Self {
            inner,
            user_id: None,
        }
    }

    fn inner(&self) -> &CJASession {
        &self.inner
    }
}

fn allowed(config: &PassportConfig, user: &User) -> bool {
    user.issuer == config.issuer.as_str()
        && AllowedSubject::new(&user.subject)
            .is_ok_and(|subject| config.allowlist.contains(&subject))
}

async fn select_user(
    pool: &sqlx::PgPool,
    config: &PassportConfig,
    id: Uuid,
) -> cja::Result<Option<User>> {
    let row = sqlx::query(
        "SELECT p.user_id FROM passport_sessions p
         WHERE p.session_id = $1
         AND p.authenticated_at + make_interval(secs => $2) > now()",
    )
    .bind(id)
    .bind(config.timings.max_age.as_secs_f64())
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let user_id: Uuid = row.try_get("user_id")?;
    let user = user::find_by_id(pool, user_id).await?;
    Ok(user.filter(|u| allowed(config, u)))
}

async fn refresh_locked(
    pool: &sqlx::PgPool,
    config: &PassportConfig,
    id: Uuid,
) -> cja::Result<Option<User>> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        "SELECT p.user_id, p.refresh_token,
                p.authenticated_at + make_interval(secs => $2) <= now() AS expired,
                p.next_refresh_at <= now() AS due
         FROM passport_sessions p WHERE p.session_id = $1 FOR UPDATE SKIP LOCKED",
    )
    .bind(id)
    .bind(config.timings.max_age.as_secs_f64())
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        tx.rollback().await?;
        return select_user(pool, config, id).await;
    };
    let expired: bool = row.try_get("expired")?;
    if expired {
        sqlx::query("DELETE FROM sessions WHERE session_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(None);
    }
    let user_id: Uuid = row.try_get("user_id")?;
    let Some(user) = user::find_by_id(&mut *tx, user_id).await? else {
        tx.rollback().await?;
        return Ok(None);
    };
    if !allowed(config, &user) {
        tx.rollback().await?;
        return Ok(None);
    }
    let due: bool = row.try_get("due")?;
    if due {
        let refresh_token: String = row.try_get("refresh_token")?;
        match oidc::refresh(config, &refresh_token, (&user.issuer, &user.subject)).await {
            Ok(tokens) => {
                if let Some(identity) = tokens.identity {
                    user::refresh_display(
                        &mut tx,
                        user_id,
                        identity.email.as_deref(),
                        identity.preferred_username.as_deref(),
                        identity.picture.as_deref(),
                    )
                    .await?;
                }
                sqlx::query(
                    "UPDATE passport_sessions SET refresh_token = $2, last_refresh_at = now(),
                     next_refresh_at = now() + make_interval(secs => $3) WHERE session_id = $1",
                )
                .bind(id)
                .bind(tokens.refresh_token)
                .bind(config.timings.refresh_interval.as_secs_f64())
                .execute(&mut *tx)
                .await?;
            }
            Err(TokenFailure::Transient) => {
                sqlx::query(
                    "UPDATE passport_sessions SET next_refresh_at = now() + make_interval(secs => $2)
                     WHERE session_id = $1",
                ).bind(id).bind(config.timings.transient_retry.as_secs_f64())
                    .execute(&mut *tx).await?;
            }
            Err(TokenFailure::InvalidGrant | TokenFailure::Rejected) => {
                sqlx::query("DELETE FROM sessions WHERE session_id = $1")
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                return Ok(None);
            }
        }
    }
    // `now()` is fixed at transaction start. The token call may have crossed
    // the hard cap while this row was locked, so check the live database clock.
    let expired_after_refresh: bool = sqlx::query_scalar(
        "SELECT authenticated_at + make_interval(secs => $2) <= clock_timestamp()
         FROM passport_sessions WHERE session_id = $1",
    )
    .bind(id)
    .bind(config.timings.max_age.as_secs_f64())
    .fetch_one(&mut *tx)
    .await?;
    if expired_after_refresh {
        sqlx::query("DELETE FROM sessions WHERE session_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(None);
    }
    tx.commit().await?;
    Ok(Some(user))
}

pub async fn resolve_user(
    pool: &sqlx::PgPool,
    config: &PassportConfig,
    id: Uuid,
) -> cja::Result<Option<User>> {
    // Most requests are not due. Avoid a row lock and transaction until a
    // refresh or hard-cap expiry needs to mutate the binding.
    let due_or_expired: Option<bool> = sqlx::query_scalar(
        "SELECT next_refresh_at <= now() OR
                authenticated_at + make_interval(secs => $2) <= now()
         FROM passport_sessions WHERE session_id = $1",
    )
    .bind(id)
    .bind(config.timings.max_age.as_secs_f64())
    .fetch_optional(pool)
    .await?;
    if due_or_expired == Some(false) {
        return select_user(pool, config, id).await;
    }
    if due_or_expired.is_none() {
        return Ok(None);
    }
    // Discovery, token exchange, and unknown-kid JWKS reload can each take
    // five seconds. Leave time for the database write and commit.
    tokio::time::timeout(Duration::from_secs(20), refresh_locked(pool, config, id))
        .await
        .map_err(|_| cja::color_eyre::eyre::eyre!("passport session refresh timed out"))?
}
