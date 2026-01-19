use crate::server::cookies::CookieKey;

/// Type alias for the database pool used throughout the application.
pub type DbPool = deadpool_postgres::Pool;

/// A trait representing the application state that must be implemented by users of the cja framework.
///
/// This trait provides access to core components like the database pool and cookie encryption key.
/// The `AppState` is cloned for each request, so it should be cheap to clone (usually by wrapping
/// expensive resources in Arc).
///
/// # Example
///
/// ```rust
/// use cja::app_state::{AppState, DbPool};
/// use cja::server::cookies::CookieKey;
///
/// #[derive(Clone)]
/// struct MyAppState {
///     db: DbPool,
///     cookie_key: CookieKey,
/// }
///
/// impl AppState for MyAppState {
///     fn version(&self) -> &str {
///         "1.0.0"
///     }
///
///     fn db(&self) -> &DbPool {
///         &self.db
///     }
///
///     fn cookie_key(&self) -> &CookieKey {
///         &self.cookie_key
///     }
/// }
/// ```
pub trait AppState: Clone + Send + Sync + 'static {
    /// Returns the version string for the application.
    /// This is useful for health checks and debugging.
    fn version(&self) -> &str;

    /// Returns a reference to the database connection pool.
    /// The pool is typically shared across all requests.
    fn db(&self) -> &DbPool;

    /// Returns the cookie encryption key used for secure cookies.
    /// This key should be consistent across application restarts
    /// to maintain session continuity.
    fn cookie_key(&self) -> &CookieKey;
}
