pub mod auth;
pub mod register;

pub fn passkey_router<S>() -> axum::Router<S>
where
    S: cja::app_state::AppState + crate::config::HasPasskeyConfig,
{
    axum::Router::new()
        .route("/register/start", axum::routing::post(register::start::<S>))
        .route(
            "/register/finish",
            axum::routing::post(register::finish::<S>),
        )
        .route("/auth/start", axum::routing::post(auth::start::<S>))
        .route("/auth/finish", axum::routing::post(auth::finish::<S>))
        .route(
            "/passkey-client.js",
            axum::routing::get(crate::js::passkey_js_handler),
        )
}
