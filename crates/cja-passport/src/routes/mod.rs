mod native;
mod web;

use crate::config::HasPassportConfig;
use axum::{
    Router,
    routing::{get, post},
};
use cja::app_state::AppState;

pub fn passport_router<S>() -> Router<S>
where
    S: AppState + HasPassportConfig + Clone + Send + Sync + 'static,
{
    Router::new()
        .route("/auth/login", get(web::login::<S>))
        .route("/auth/callback", get(web::callback::<S>))
        .route("/auth/logout", post(web::logout::<S>))
        .route("/auth/native/start", get(native::start::<S>))
        .route("/auth/native/exchange", post(native::exchange::<S>))
}
