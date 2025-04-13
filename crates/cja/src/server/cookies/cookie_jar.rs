use axum::{
    extract::FromRequestParts,
    http::request::Parts,
    response::{IntoResponse as _, Response},
};
use http::StatusCode;
use tracing::error;

use crate::app_state::AppState;

pub struct CookieJar<AS> {
    cookies: tower_cookies::Cookies,
    state: AS,
}

#[async_trait::async_trait]
impl<A> FromRequestParts<A> for CookieJar<A>
where
    A: Send + Sync,
    A: AppState,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &A) -> Result<Self, Self::Rejection> {
        let Ok(cookies) = tower_cookies::Cookies::from_request_parts(parts, state).await else {
            error!("Failed to extract cookies from request");
            return Err(StatusCode::INTERNAL_SERVER_ERROR.into_response());
        };

        Ok(CookieJar {
            cookies,
            state: state.clone(),
        })
    }
}

impl<AS> CookieJar<AS>
where
    AS: AppState,
{
    /// Add a new private cookie
    pub fn add(&self, cookie: tower_cookies::Cookie<'static>) {
        let private = self.cookies.private(self.state.cookie_key());
        private.add(cookie);
    }

    /// Get a private cookie by name
    pub fn get(&self, name: &str) -> Option<tower_cookies::Cookie<'static>> {
        let private = self.cookies.private(self.state.cookie_key());
        private.get(name)
    }

    /// Removes the `cookie` from the jar.
    pub fn remove(&self, cookie: tower_cookies::Cookie<'static>) {
        let private = self.cookies.private(self.state.cookie_key());
        private.remove(cookie);
    }
}
