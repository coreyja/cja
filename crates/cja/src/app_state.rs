use crate::server::cookies::CookieKey;

/// A trait representing the application state that must be implemented by users of the cja framework.
///
/// This trait provides access to core components like the database pool and cookie encryption key.
/// The `AppState` is cloned for each request, so it should be cheap to clone (usually by wrapping
/// expensive resources in Arc).
///
/// # Example
///
/// ```rust
/// use cja::app_state::AppState;
/// use cja::server::cookies::CookieKey;
/// use sqlx::PgPool;
///
/// #[derive(Clone)]
/// struct MyAppState {
///     db: PgPool,
///     cookie_key: CookieKey,
/// }
///
/// impl AppState for MyAppState {
///     fn version(&self) -> &str {
///         "1.0.0"
///     }
///     
///     fn db(&self) -> &PgPool {
///         &self.db
///     }
///     
///     fn cookie_key(&self) -> &CookieKey {
///         &self.cookie_key
///     }
/// }
/// ```
///
/// # Using with Axum
///
/// ```rust
/// use cja::app_state::AppState;
/// use cja::server::cookies::CookieKey;
/// use sqlx::postgres::PgPoolOptions;
/// use axum::{Router, routing::get, extract::State};
///
/// #[derive(Clone)]
/// struct MyAppState {
///     db: sqlx::PgPool,
///     cookie_key: CookieKey,
/// }
///
/// impl AppState for MyAppState {
///     fn version(&self) -> &str { "1.0.0" }
///     fn db(&self) -> &sqlx::PgPool { &self.db }
///     fn cookie_key(&self) -> &CookieKey { &self.cookie_key }
/// }
///
/// async fn handler(State(state): State<MyAppState>) -> String {
///     format!("App version: {}", state.version())
/// }
///
///  async fn example() -> Result<(), Box<dyn std::error::Error>> {
///  use sqlx::postgres::PgPoolOptions;
/// let db = PgPoolOptions::new()
///      .connect(&std::env::var("DATABASE_URL").unwrap_or_else(|_| "postgres://test".to_string()))
///      .await
///      .map_err(|_| "Database connection failed")?;
///     
/// let state = MyAppState {
///     db,
///     cookie_key: CookieKey::from_env_or_generate()?,
/// };
///
/// let app: Router = Router::new()
///     .route("/", get(handler))
///     .with_state(state);
///
/// // Run the server
/// # Ok(())
/// # }
/// ```
pub trait AppState: Clone + Send + Sync + 'static {
    /// Returns the version string for the application.
    /// This is useful for health checks and debugging.
    fn version(&self) -> &str;

    /// Returns a reference to the database connection pool.
    /// The pool is typically shared across all requests.
    fn db(&self) -> &sqlx::PgPool;

    /// Returns the cookie encryption key used for secure cookies.
    /// This key should be consistent across application restarts
    /// to maintain session continuity.
    fn cookie_key(&self) -> &CookieKey;
}
