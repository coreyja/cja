//! OIDC relying party for human Tailscale identities.
pub mod config;
pub mod db;
pub mod extractors;
pub mod models;
pub mod oidc;
pub mod rejection;
pub mod routes;
pub mod session;
mod transport;

pub use config::{
    AllowedSubject, HasMcpOAuthConfig, HasPassportConfig, McpOAuthConfig, OAuthOptions,
    OAuthTimings, PassportConfig, SessionTimings,
};
pub use db::{prune_auth_artifacts, run_migrations};
pub use extractors::{BearerUser, CurrentUser, McpUser, OptionalUser};
pub use models::user::User;
pub use routes::{oauth_router, passport_router};
pub use session::PassportSession;
