use axum::{extract::FromRequestParts, response::Redirect};
use serde::{Deserialize, Serialize};

use crate::app_state::AppState as AS;

use super::cookies::CookieJar;

#[derive(Debug, Clone, Deserialize, Serialize, sqlx::FromRow)]
pub struct Session {
    pub session_id: uuid::Uuid,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[async_trait::async_trait]
impl<AppState: AS> FromRequestParts<AppState> for Session {
    type Rejection = Redirect;

    async fn from_request_parts(
        parts: &mut http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let cookies = CookieJar::from_request_parts(parts, state)
            .await
            .map_err(|_| Redirect::temporary("/login"))?;

        let session_cookie = cookies.get("session_id");
        let session = if let Some(session_cookie) = session_cookie {
            let session_id = session_cookie.value().to_string();
            let Ok(session_id) = uuid::Uuid::parse_str(&session_id) else {
                tracing::error!("Failed to parse session id: {session_id}");

                Err(Redirect::temporary("/login"))?
            };

            sqlx::query_as!(
                Session,
                r"
        SELECT session_id, updated_at, created_at
        FROM Sessions
        WHERE session_id = $1
        ",
                session_id
            )
            .fetch_one(state.db())
            .await
            .map_err(|e| {
                tracing::error!("Failed to fetch session: {e}");

                Redirect::temporary("/login")
            })?
        } else {
            Session::create(state, &cookies)
                .await
                .map_err(|_| Redirect::temporary("/login"))?
        };

        Ok(session)
    }
}

impl Session {
    pub async fn create<AppState: AS>(
        app_state: &AppState,
        jar: &CookieJar<AppState>,
    ) -> color_eyre::Result<Self> {
        let session = sqlx::query_as!(
            Session,
            r"
        INSERT INTO Sessions DEFAULT VALUES
        RETURNING session_id, updated_at, created_at
        ",
        )
        .fetch_one(app_state.db())
        .await?;

        let session_cookie =
            tower_cookies::Cookie::build(("session_id", session.session_id.to_string()))
                .path("/")
                .http_only(true)
                .secure(true)
                .expires(None);
        jar.add(session_cookie.into());

        Ok(session)
    }
}
