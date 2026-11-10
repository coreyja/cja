use axum::extract::FromRequestParts;
use http::StatusCode;
use serde::{Deserialize, Serialize};

use crate::app_state::AppState;

use super::cookies::CookieJar;

/// Name of the framework session cookie.
///
/// The `__Host-` prefix means browsers only accept this cookie when it is set
/// over HTTPS with `Path=/` and no `Domain` attribute, so a sibling subdomain
/// cannot overwrite the victim's session cookie.
pub const SESSION_COOKIE_NAME: &str = "__Host-session_id";

/// Write the private session cookie for `session_id` into `jar`.
///
/// Private (encrypted with the app's [`AppState::cookie_key`]), `HttpOnly`,
/// `Secure`, `Path=/`, `SameSite=Lax`, `__Host-` prefixed, and session-scoped
/// (no `Max-Age`/`Expires`).
pub fn save_session_cookie<A: AppState>(jar: &CookieJar<A>, session_id: uuid::Uuid) {
    let cookie = tower_cookies::Cookie::build((SESSION_COOKIE_NAME, session_id.to_string()))
        .path("/")
        .http_only(true)
        .secure(true)
        .same_site(tower_cookies::cookie::SameSite::Lax)
        .expires(None);

    jar.add(cookie.into());
}

/// Clear the framework session cookie so the browser drops it. Same attributes
/// as [`save_session_cookie`] (a `__Host-` replacement cookie must repeat
/// `Path=/` + `Secure`), with `Expires` at the epoch.
///
/// Pairs with deleting the session row server-side (see cja-passkey's
/// `POST /auth/logout`): an expired `Set-Cookie` works deterministically even
/// when the request carried no cookie, where `CookieJar::remove` needs the
/// matching original.
pub fn clear_session_cookie<A: AppState>(jar: &CookieJar<A>) {
    let cookie = tower_cookies::Cookie::build((SESSION_COOKIE_NAME, ""))
        .path("/")
        .http_only(true)
        .secure(true)
        .same_site(tower_cookies::cookie::SameSite::Lax)
        .expires(tower_cookies::cookie::time::OffsetDateTime::UNIX_EPOCH);

    jar.add(cookie.into());
}

/// Minimum time between `updated_at` refreshes performed by the `Session`
/// extractor. Keeps [`prune`]'s idle clock accurate without a write per request.
pub const ACTIVITY_REFRESH_MIN_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(60 * 60);

/// Delete sessions whose `updated_at` is older than `idle`. Returns rows deleted.
///
/// Schedule from a cron (the scaffold registers a daily 4am `prune-sessions`
/// example). `idle` must be comfortably above [`ACTIVITY_REFRESH_MIN_INTERVAL`]:
/// `updated_at` advances at most once per interval, so a smaller `idle` would
/// delete sessions in active use — this function refuses such values.
/// Deleting a row cascades consumer rows that reference `sessions(session_id)`
/// with `ON DELETE CASCADE` (see the cja-passkey README framework contract);
/// a pruned user simply appears anonymous on their next request.
///
/// To stop a scheduled prune, use the `CRON_DISABLED` env var or remove the
/// registration.
pub async fn prune(pool: &sqlx::PgPool, idle: std::time::Duration) -> crate::Result<u64> {
    if idle <= ACTIVITY_REFRESH_MIN_INTERVAL {
        return Err(color_eyre::eyre::eyre!(
            "prune idle ({idle:?}) must exceed ACTIVITY_REFRESH_MIN_INTERVAL \
             ({:?}): updated_at is refreshed at most once per interval, so a \
             smaller idle would delete sessions in active use",
            ACTIVITY_REFRESH_MIN_INTERVAL
        ));
    }
    let result =
        sqlx::query("DELETE FROM sessions WHERE updated_at < now() - make_interval(secs => $1)")
            .bind(idle.as_secs_f64())
            .execute(pool)
            .await?;
    Ok(result.rows_affected())
}

/// Core session data that all sessions must contain.
///
/// This struct represents the minimal session information stored in the database.
/// Custom session implementations should include this as a field and add any
/// additional data needed by the application.
///
/// # Example
///
/// ```rust
/// use cja::server::session::CJASession;
///
/// #[derive(Debug, Clone)]
/// struct MySession {
///     // Required: include the core session data
///     inner: CJASession,
///     
///     // Add custom fields for your application
///     user_id: Option<i32>,
///     theme: String,
///     last_page_visited: Option<String>,
/// }
/// ```
#[derive(Debug, Clone, Deserialize, Serialize, sqlx::FromRow)]
pub struct CJASession {
    /// Unique identifier for this session
    pub session_id: uuid::Uuid,
    /// Timestamp of last activity. Bumped by the `update_sessions_updated_at`
    /// DB trigger on every UPDATE; the `Session` extractor refreshes it at
    /// most once per [`ACTIVITY_REFRESH_MIN_INTERVAL`] while the session is
    /// in use, so it doubles as the idle clock for [`prune`].
    pub updated_at: chrono::DateTime<chrono::Utc>,
    /// Timestamp when the session was first created
    pub created_at: chrono::DateTime<chrono::Utc>,
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

/// A trait for implementing custom session types that integrate with the framework's session management.
///
/// Sessions are automatically created when needed and persisted across requests using secure cookies.
/// Your session type must include the `CJASession` as an inner field and can add any additional
/// fields needed by your application.
///
/// # Example
///
/// ```rust,no_run
/// use cja::server::session::{AppSession, CJASession};
/// use serde::{Serialize, Deserialize};
///
/// #[derive(Debug, Clone)]
/// struct UserSession {
///     inner: CJASession,
///     user_id: Option<i32>,
///     preferences: serde_json::Value,
/// }
///
/// #[async_trait::async_trait]
/// impl AppSession for UserSession {
///     async fn from_db(pool: &sqlx::PgPool, session_id: uuid::Uuid) -> cja::Result<Option<Self>> {
///         // Query your session data including any custom fields
///         // In a real app, you'd have extended the sessions table with these columns
///         let row = sqlx::query_as::<_, CJASession>(
///             "SELECT session_id, created_at, updated_at FROM sessions WHERE session_id = $1"
///         )
///         .bind(session_id)
///         .fetch_optional(pool)
///         .await?;
///
///         // `Ok(None)` means "no such session" — the extractor will create one.
///         // A missing row MUST be `Ok(None)`, never an `Err`.
///         // For this example, we're just using default values for custom fields
///         // In a real app, you'd query your extended session data here
///         Ok(row.map(|inner| Self {
///             inner,
///             user_id: None,
///             preferences: serde_json::json!({}),
///         }))
///     }
///
///     async fn create(pool: &sqlx::PgPool) -> cja::Result<Self> {
///         let row = sqlx::query_as::<_, CJASession>(
///             "INSERT INTO sessions DEFAULT VALUES RETURNING session_id, created_at, updated_at"
///         )
///         .fetch_one(pool)
///         .await?;
///         
///         Ok(Self {
///             inner: row,
///             user_id: None,
///             preferences: serde_json::json!({}),
///         })
///     }
///     
///     fn from_inner(inner: CJASession) -> Self {
///         Self {
///             inner,
///             user_id: None,
///             preferences: serde_json::json!({}),
///         }
///     }
///     
///     fn inner(&self) -> &CJASession {
///         &self.inner
///     }
/// }
/// ```
///
/// # Using Sessions in Handlers
///
/// ```rust
/// use axum::response::IntoResponse;
/// use cja::server::session::Session;
/// # use cja::server::session::{AppSession, CJASession};
/// #
/// # #[derive(Debug, Clone)]
/// # struct UserSession { inner: CJASession, user_id: Option<i32> }
/// #
/// # #[async_trait::async_trait]
/// # impl AppSession for UserSession {
/// #     async fn from_db(_: &sqlx::PgPool, _: uuid::Uuid) -> cja::Result<Option<Self>> { todo!() }
/// #     async fn create(_: &sqlx::PgPool) -> cja::Result<Self> { todo!() }
/// #     fn from_inner(inner: CJASession) -> Self { Self { inner, user_id: None } }
/// #     fn inner(&self) -> &CJASession { &self.inner }
/// # }
///
/// async fn handler(
///     Session(session): Session<UserSession>
/// ) -> impl IntoResponse {
///     format!("Session ID: {}, User: {:?}",
///             session.session_id(),
///             session.user_id)
/// }
/// ```
#[async_trait::async_trait]
pub trait AppSession: Sized {
    /// Load a session from the database by its ID.
    ///
    /// This method should fetch the session record and any associated data
    /// from your sessions table. Return `Ok(None)` when no row exists for
    /// `session_id` (the extractor then creates a fresh session); return
    /// `Err` only for genuine failures (pool outage, decode error), which the
    /// extractor surfaces as a 500. A missing row must never be an `Err`.
    async fn from_db(pool: &sqlx::PgPool, session_id: uuid::Uuid) -> crate::Result<Option<Self>>;

    /// Create a new session in the database.
    ///
    /// This method should insert a new session record with default values
    /// and return the created session.
    async fn create(pool: &sqlx::PgPool) -> crate::Result<Self>;

    /// Create a session instance from the inner `CJASession`.
    ///
    /// This is used internally when reconstructing sessions. Custom fields
    /// should be initialized with default values.
    fn from_inner(inner: CJASession) -> Self;

    /// Get a reference to the inner `CJASession`.
    ///
    /// This provides access to the core session fields like ID and timestamps.
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

/// An Axum extractor that provides access to the current session.
///
/// This extractor automatically handles session creation and loading based on cookies.
/// If no session exists, a new one is created automatically.
///
/// # Example
///
/// ```rust
/// use axum::{Router, routing::get, response::IntoResponse};
/// use cja::server::session::Session;
/// # use cja::server::session::{AppSession, CJASession};
/// #
/// # #[derive(Debug, Clone)]
/// # struct UserSession {
/// #     inner: CJASession,
/// #     user_id: Option<i32>,
/// #     login_count: i32,
/// # }
/// #
/// # #[async_trait::async_trait]
/// # impl AppSession for UserSession {
/// #     async fn from_db(_: &sqlx::PgPool, _: uuid::Uuid) -> cja::Result<Option<Self>> { todo!() }
/// #     async fn create(_: &sqlx::PgPool) -> cja::Result<Self> { todo!() }
/// #     fn from_inner(inner: CJASession) -> Self {
/// #         Self { inner, user_id: None, login_count: 0 }
/// #     }
/// #     fn inner(&self) -> &CJASession { &self.inner }
/// # }
///
/// async fn profile_handler(
///     Session(session): Session<UserSession>
/// ) -> impl IntoResponse {
///     match session.user_id {
///         Some(id) => format!("Welcome back, user {}! Login count: {}", id, session.login_count),
///         None => "Please log in".to_string(),
///     }
/// }
///
/// async fn login_handler(
///     Session(mut session): Session<UserSession>,
///     // ... other extractors for login data
/// ) -> impl IntoResponse {
///     // After successful authentication:
///     session.user_id = Some(123);
///     session.login_count += 1;
///     // Don't forget to save the session!
///     
///     "Logged in successfully"
/// }
///
/// # #[derive(Clone)]
/// # struct MyAppState;
/// # impl cja::app_state::AppState for MyAppState {
/// #     fn version(&self) -> &str { "1.0.0" }
/// #     fn db(&self) -> &sqlx::PgPool { todo!() }
/// #     fn cookie_key(&self) -> &cja::server::cookies::CookieKey { todo!() }
/// # }
/// # fn app() -> Router {
/// let app = Router::new()
///     .route("/profile", get(profile_handler))
///     .route("/login", get(login_handler))
///     .with_state(MyAppState);
/// # app
/// # }
/// ```
#[derive(Clone)]
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

            save_session_cookie(cookies, session.0.inner().session_id);

            Ok(session)
        }

        let cookies = CookieJar::from_request_parts(parts, state)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        let Some(session_cookie) = cookies.get(SESSION_COOKIE_NAME) else {
            return create_session(state, &cookies).await;
        };

        let session_id = session_cookie.value().to_string();
        let Ok(session_id) = uuid::Uuid::parse_str(&session_id) else {
            return create_session(state, &cookies).await;
        };

        match S::from_db(state.db(), session_id).await {
            Ok(Some(session)) => {
                touch_if_stale(state.db(), session.inner()).await;
                Ok(Session(session))
            }
            Ok(None) => create_session::<A, S>(state, &cookies).await,
            Err(err) => {
                tracing::error!("failed to load session {session_id}: {err:#}");
                Err(StatusCode::INTERNAL_SERVER_ERROR)
            }
        }
    }
}

/// Refresh `updated_at` at most once per [`ACTIVITY_REFRESH_MIN_INTERVAL`]
/// so [`prune`] measures real activity. Best-effort: a failure is logged,
/// never fatal for the request (pinned by `failed_activity_refresh_...`).
async fn touch_if_stale(pool: &sqlx::PgPool, session: &CJASession) {
    if session.updated_at + ACTIVITY_REFRESH_MIN_INTERVAL > chrono::Utc::now() {
        return;
    }
    let refreshed = sqlx::query(
        "UPDATE sessions SET updated_at = now() \
         WHERE session_id = $1 AND updated_at < now() - make_interval(secs => $2)",
    )
    .bind(session.session_id)
    .bind(ACTIVITY_REFRESH_MIN_INTERVAL.as_secs_f64())
    .execute(pool)
    .await;
    if let Err(err) = refreshed {
        tracing::warn!("session activity refresh failed (non-fatal): {err:#}");
    }
}
