use axum::extract::FromRequestParts;
use http::StatusCode;
use serde::{Deserialize, Serialize};

use crate::app_state::AppState;

use super::cookies::CookieJar;

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
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CJASession {
    /// Unique identifier for this session
    pub session_id: uuid::Uuid,
    /// Timestamp of last activity (updated on each request)
    pub updated_at: chrono::DateTime<chrono::Utc>,
    /// Timestamp when the session was first created
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl CJASession {
    /// Create a CJASession from a tokio_postgres Row.
    /// Expected columns: session_id (0), updated_at (1), created_at (2)
    pub fn from_row(row: &tokio_postgres::Row) -> Self {
        Self {
            session_id: row.get(0),
            updated_at: row.get(1),
            created_at: row.get(2),
        }
    }
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
///     async fn from_db(pool: &sqlx::PgPool, session_id: uuid::Uuid) -> cja::Result<Self> {
///         // Query your session data including any custom fields
///         // In a real app, you'd have extended the sessions table with these columns
///         let row = sqlx::query_as::<_, CJASession>(
///             "SELECT session_id, created_at, updated_at FROM sessions WHERE session_id = $1"
///         )
///         .bind(session_id)
///         .fetch_one(pool)
///         .await?;
///         
///         // For this example, we're just using default values for custom fields
///         // In a real app, you'd query your extended session data here
///         Ok(Self {
///             inner: row,
///             user_id: None,
///             preferences: serde_json::json!({}),
///         })
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
/// #     async fn from_db(_: &sqlx::PgPool, _: uuid::Uuid) -> cja::Result<Self> { todo!() }
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
    /// from your sessions table.
    async fn from_db(pool: &crate::app_state::DbPool, session_id: uuid::Uuid) -> crate::Result<Self>;

    /// Create a new session in the database.
    ///
    /// This method should insert a new session record with default values
    /// and return the created session.
    async fn create(pool: &crate::app_state::DbPool) -> crate::Result<Self>;

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
/// #     async fn from_db(_: &sqlx::PgPool, _: uuid::Uuid) -> cja::Result<Self> { todo!() }
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
