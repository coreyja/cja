pub mod auth;
pub mod register;

/// Store a challenge on the session row, mapping "row gone" to 412.
///
/// `set_challenge_state` reports rows affected; a session row that vanished
/// between the extractor's load and this write (rotated by a concurrent
/// finish, deleted by logout/prune) must not be reported as success — the
/// user would complete the authenticator prompt and then fail the finish.
pub(crate) async fn store_challenge_state(
    pool: &sqlx::PgPool,
    session_id: uuid::Uuid,
    challenge_json: serde_json::Value,
) -> Result<(), axum::http::StatusCode> {
    let affected =
        crate::session::PasskeySession::set_challenge_state(pool, session_id, challenge_json)
            .await
            .map_err(|err| {
                tracing::error!("set_challenge_state failed: {err:#}");
                axum::http::StatusCode::INTERNAL_SERVER_ERROR
            })?;
    if affected == 0 {
        tracing::warn!(
            "set_challenge_state matched no row: session {session_id} \
             was rotated or deleted concurrently"
        );
        return Err(axum::http::StatusCode::PRECONDITION_FAILED);
    }
    Ok(())
}

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
        .route("/auth/logout", axum::routing::post(auth::logout::<S>))
        .route(
            "/auth/discoverable/start",
            axum::routing::post(auth::start_discoverable::<S>),
        )
        .route(
            "/auth/discoverable/finish",
            axum::routing::post(auth::finish_discoverable::<S>),
        )
}
