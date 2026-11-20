//! OIDC relying party for human Tailscale identities.
pub mod config;
pub mod db;
pub mod extractors;
pub mod models;
pub mod oidc;
pub mod rejection;
pub mod routes;
pub mod session;

pub use config::{AllowedSubject, HasPassportConfig, PassportConfig, SessionTimings};
pub use db::{prune_auth_artifacts, run_migrations};
pub use extractors::{BearerUser, CurrentUser, OptionalUser};
pub use models::user::User;
pub use routes::passport_router;
pub use session::PassportSession;
