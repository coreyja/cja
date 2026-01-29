//! Mock GitHub OAuth server for testing.
//!
//! This module provides a mock OAuth server that simulates GitHub's OAuth flow
//! for integration and E2E testing. It supports:
//!
//! - `/login/oauth/authorize` - OAuth authorization (immediate redirect with code)
//! - `/login/oauth/access_token` - Exchange code for token
//! - `/user` - Return mock user data for token
//! - `/_admin/set-user-for-state` - Pre-register users for specific OAuth states
//!
//! # Usage
//!
//! ## As a library (recommended)
//!
//! Use `create_router()` to get an Axum router you can compose or serve:
//!
//! ```rust,ignore
//! use cja::testing::mock_oauth;
//!
//! let app = mock_oauth::create_router();
//! let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
//! axum::serve(listener, app).await?;
//! ```
//!
//! ## As a standalone server
//!
//! Use `run_server()` for a simple standalone mock OAuth server:
//!
//! ```rust,ignore
//! use cja::testing::mock_oauth;
//!
//! mock_oauth::run_server(8081).await?;
//! ```
//!
//! ## Controlling test users
//!
//! Pre-register a user for a specific OAuth state before the test starts:
//!
//! ```rust,ignore
//! // POST to /_admin/set-user-for-state
//! let body = json!({
//!     "state": "test-state-123",
//!     "user": {
//!         "id": 42,
//!         "login": "testuser",
//!         "name": "Test User",
//!         "email": "test@example.com",
//!         "avatar_url": "https://example.com/avatar.png"
//!     }
//! });
//! ```

mod routes;
mod state;
mod types;

use axum::{
    Router,
    routing::{get, post},
};
use state::MockOAuthState;

pub use types::MockUserConfig;

/// Create the mock OAuth server router.
///
/// The router is self-contained with its own state and does not require
/// external configuration.
pub fn create_router() -> Router {
    let state = MockOAuthState::new();

    Router::new()
        // OAuth endpoints (mimic GitHub)
        .route("/login/oauth/authorize", get(routes::authorize))
        .route("/login/oauth/access_token", post(routes::access_token))
        .route("/user", get(routes::get_user))
        // Admin endpoint for test control
        .route(
            "/_admin/set-user-for-state",
            post(routes::set_user_for_state),
        )
        .with_state(state)
}

/// Run the mock OAuth server on the specified port.
///
/// This is a convenience function for running a standalone mock server.
/// For more control, use `create_router()` directly.
pub async fn run_server(port: u16) -> color_eyre::Result<()> {
    let app = create_router();
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await?;
    tracing::info!("Mock GitHub OAuth server running on port {}", port);
    axum::serve(listener, app).await?;
    Ok(())
}
