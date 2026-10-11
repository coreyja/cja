//! Reusable passkey/WebAuthn authentication module for cja apps.
//!
//! Adds passkey-based registration and login to any Axum app built on `cja` by:
//!
//! 1. Implementing the [`config::HasPasskeyConfig`] trait on your `AppState`.
//! 2. Calling [`db::run_migrations`] (after [`cja::db::run_migrations`]).
//! 3. Mounting [`routes::passkey_router`] on your router.
//! 4. Adding [`tower_cookies::CookieManagerLayer`] to the router.
//! 5. Mounting [`passkey_assets::MANIFEST`] and rendering its hashed client URL.
//!
//! Use [`extractors::CurrentUser`] / [`extractors::OptionalUser`] in handlers
//! to read the authenticated user.
//!
//! # Reference integration
//!
//! ```rust,ignore
//! use std::sync::Arc;
//! use cja::server::cookies::CookieKey;
//! use cja_passkey::{HasPasskeyConfig, PasskeyConfig};
//! use url::Url;
//!
//! #[derive(Clone)]
//! struct AppState {
//!     db: sqlx::PgPool,
//!     cookie_key: CookieKey,
//!     passkey: PasskeyConfig,
//! }
//!
//! impl cja::app_state::AppState for AppState {
//!     fn version(&self) -> &str { "1.0.0" }
//!     fn db(&self) -> &sqlx::PgPool { &self.db }
//!     fn cookie_key(&self) -> &CookieKey { &self.cookie_key }
//! }
//!
//! impl HasPasskeyConfig for AppState {
//!     fn passkey_config(&self) -> &PasskeyConfig { &self.passkey }
//! }
//!
//! async fn page() -> axum::response::Html<String> {
//!     axum::response::Html(format!(
//!         r#"<script src="{}"></script>"#,
//!         cja_passkey::passkey_assets::asset_url("passkey-client.js"),
//!     ))
//! }
//!
//! async fn build() -> cja::Result<axum::Router> {
//!     let db = sqlx::PgPool::connect(&std::env::var("DATABASE_URL")?).await?;
//!     cja::db::run_migrations(&db).await?;
//!     cja_passkey::run_migrations(&db).await?;
//!
//!     let passkey = PasskeyConfig::new("example.com", &Url::parse("https://example.com")?)?;
//!     let state = AppState { db, cookie_key: CookieKey::generate(), passkey };
//!
//!     Ok(axum::Router::new()
//!         .route("/", axum::routing::get(page))
//!         .merge(cja::assets::router::<AppState>(&[&cja_passkey::passkey_assets::MANIFEST])?)
//!         .merge(cja_passkey::passkey_router::<AppState>())
//!         .layer(tower_cookies::CookieManagerLayer::new())
//!         .with_state(state))
//! }
//! ```

cja::include_assets!(passkey_assets);

pub mod config;
pub mod db;
pub mod extractors;
pub mod models;
pub mod routes;
pub mod session;

pub use config::{HasPasskeyConfig, PasskeyConfig};
pub use db::run_migrations;
pub use extractors::{CurrentUser, OptionalUser};
pub use models::user::User;
pub use routes::passkey_router;
pub use session::PasskeySession;
