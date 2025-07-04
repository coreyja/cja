use axum::{extract::FromRequestParts, http::request::Parts, response::Response};
use maud::Render;

use crate::app_state::AppState;

use super::Page;

pub struct Factory<A: AppState> {
    #[allow(dead_code)]
    state: A,
}

impl<A: AppState> Factory<A> {
    pub fn create_page<C: Render + 'static>(self, content: C) -> Page {
        Page {
            content: Box::new(content),
        }
    }
}

impl<A: AppState> FromRequestParts<A> for Factory<A> {
    type Rejection = Response;

    async fn from_request_parts(_: &mut Parts, state: &A) -> Result<Self, Self::Rejection> {
        Ok(Self {
            state: state.clone(),
        })
    }
}
