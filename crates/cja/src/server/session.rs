use axum::extract::FromRequestParts;
use http::StatusCode;
use serde::{Deserialize, Serialize};

use crate::app_state::AppState;

use super::cookies::CookieJar;

#[derive(Debug, Clone, Deserialize, Serialize, sqlx::FromRow)]
pub struct CJASession {
    pub session_id: uuid::Uuid,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl CJASession {
    fn save_cookie<A: AppState>(&self, jar: &CookieJar<A>) {
        let cookie = tower_cookies::Cookie::build(("session_id", self.session_id.to_string()))
            .path("/")
            .http_only(true)
            .secure(true)
            .expires(None);

        jar.add(cookie.into());
    }
}

// #[async_trait::async_trait]
// impl<AppState: AS> FromRequestParts<AppState> for Session {
//     type Rejection = Redirect;

//     async fn from_request_parts(
//         parts: &mut http::request::Parts,
//         state: &AppState,
//     ) -> Result<Self, Self::Rejection> {
//         let cookies = CookieJar::from_request_parts(parts, state)
//             .await
//             .map_err(|_| Redirect::temporary("/login"))?;

//         let SessionCookie(session_id) = SessionCookie::from_request_parts(parts, state)
//             .await
//             .map_err(|_| Redirect::temporary("/login"))?;

//         let session_cookie = cookies.get("session_id");
//         let session = if let Some(session_cookie) = session_cookie {
//             let session_id = session_cookie.value().to_string();
//             let Ok(session_id) = uuid::Uuid::parse_str(&session_id) else {
//                 tracing::error!("Failed to parse session id: {session_id}");

//                 Err(Redirect::temporary("/login"))?
//             };

//             sqlx::query_as!(
//                 Session,
//                 r"
//         SELECT session_id, updated_at, created_at
//         FROM Sessions
//         WHERE session_id = $1
//         ",
//                 session_id
//             )
//             .fetch_one(state.db())
//             .await
//             .map_err(|e| {
//                 tracing::error!("Failed to fetch session: {e}");

//                 Redirect::temporary("/login")
//             })?
//         } else {
//             Session::create(state, &cookies)
//                 .await
//                 .map_err(|_| Redirect::temporary("/login"))?
//         };

//         Ok(session)
//     }
// }

// impl Session {
//     pub async fn create<AppState: AS>(
//         app_state: &AppState,
//         jar: &CookieJar<AppState>,
//     ) -> color_eyre::Result<Self> {
//         let session = sqlx::query_as!(
//             Session,
//             r"
//         INSERT INTO Sessions DEFAULT VALUES
//         RETURNING session_id, updated_at, created_at
//         ",
//         )
//         .fetch_one(app_state.db())
//         .await?;

//         let session_cookie =
//             tower_cookies::Cookie::build(("session_id", session.session_id.to_string()))
//                 .path("/")
//                 .http_only(true)
//                 .secure(true)
//                 .expires(None);
//         jar.add(session_cookie.into());

//         Ok(session)
//     }
// }

#[async_trait::async_trait]
pub trait AppSession: Sized {
    async fn from_db(pool: &sqlx::PgPool, session_id: uuid::Uuid) -> crate::Result<Self>;

    async fn create(pool: &sqlx::PgPool) -> crate::Result<Self>;

    fn from_inner(inner: CJASession) -> Self;

    fn inner(&self) -> &CJASession;

    fn session_id(&self) -> &uuid::Uuid {
        &self.inner().session_id
    }

    fn created_at(&self) -> &chrono::DateTime<chrono::Utc> {
        &self.inner().created_at
    }

    fn updated_at(&self) -> &chrono::DateTime<chrono::Utc> {
        &self.inner().updated_at
    }
}

pub struct Session<A: AppSession>(pub A);

impl<A: AppState + Send + Sync, S: AppSession + Send + Sync> FromRequestParts<A> for Session<S> {
    type Rejection = StatusCode;

    async fn from_request_parts(
        parts: &mut http::request::Parts,
        state: &A,
    ) -> Result<Self, Self::Rejection> {
        async fn create_session<A: AppState, S: AppSession>(
            state: &A,
            cookies: &CookieJar<A>,
        ) -> Result<Session<S>, StatusCode> {
            let session = S::create(state.db())
                .await
                .map(Session)
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

            session.0.inner().save_cookie(cookies);

            Ok(session)
        }

        let cookies = CookieJar::from_request_parts(parts, state)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        let Some(session_cookie) = cookies.get("session_id") else {
            return create_session(state, &cookies).await;
        };

        let session_id = session_cookie.value().to_string();
        let Ok(session_id) = uuid::Uuid::parse_str(&session_id) else {
            return create_session(state, &cookies).await;
        };

        let Ok(session) = S::from_db(state.db(), session_id).await.map(Session) else {
            return create_session(state, &cookies).await;
        };

        Ok(session)
    }
}
