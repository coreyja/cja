use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use cja::{app_state::AppState, server::session::Session};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use crate::{config::HasPassportConfig, rejection::Rejection, session::PassportSession};

use super::web::{Destination, begin, hash};

#[derive(Deserialize)]
pub(crate) struct NativeQuery {
    redirect_uri: String,
}

fn bad_request() -> Rejection {
    (StatusCode::BAD_REQUEST, "Invalid native redirect")
        .into_response()
        .into()
}

pub(crate) async fn start<S: AppState + HasPassportConfig>(
    State(state): State<S>,
    Session(session): Session<PassportSession>,
    Query(query): Query<NativeQuery>,
) -> Result<Response, Rejection> {
    let uri = url::Url::parse(&query.redirect_uri).map_err(|_| bad_request())?;
    if !state
        .passport_config()
        .native_schemes
        .contains(uri.scheme())
        || !uri.username().is_empty()
        || uri.password().is_some()
        || uri.fragment().is_some()
        || uri.query_pairs().any(|(key, _)| key == "code")
    {
        return Err(bad_request());
    }
    begin(&state, &session, Destination::Native(uri.to_string())).await
}

#[derive(Deserialize)]
pub(crate) struct ExchangeBody {
    code: String,
}

#[derive(Serialize)]
pub(crate) struct ExchangeResponse {
    token: String,
    token_type: &'static str,
    expires_at: jiff::Timestamp,
}

pub(crate) async fn exchange<S: AppState + HasPassportConfig>(
    State(state): State<S>,
    Json(body): Json<ExchangeBody>,
) -> Result<Response, Rejection> {
    if body.code.is_empty() {
        return Err(bad_request());
    }
    let mut tx = state.db().begin().await.map_err(internal)?;
    let row = sqlx::query(
        "DELETE FROM passport_handoffs
         WHERE code_hash = $1 AND expires_at > now()
         RETURNING session_id",
    )
    .bind(hash(&body.code))
    .fetch_optional(&mut *tx)
    .await
    .map_err(internal)?
    .ok_or_else(bad_request)?;
    let id: Uuid = row.try_get("session_id").map_err(internal)?;
    let expires_at: jiff_sqlx::Timestamp = sqlx::query_scalar(
        "SELECT authenticated_at + make_interval(secs => $2)
         FROM passport_sessions WHERE session_id = $1
         AND authenticated_at + make_interval(secs => $2) > now()",
    )
    .bind(id)
    .bind(state.passport_config().timings.max_age.as_secs_f64())
    .fetch_optional(&mut *tx)
    .await
    .map_err(internal)?
    .ok_or_else(bad_request)?;
    let token = super::web::random_secret();
    sqlx::query("UPDATE passport_sessions SET bearer_hash = $2 WHERE session_id = $1")
        .bind(id)
        .bind(hash(&token))
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
    tx.commit().await.map_err(internal)?;
    Ok((
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Json(ExchangeResponse {
            token,
            token_type: "Bearer",
            expires_at: expires_at.to_jiff(),
        }),
    )
        .into_response())
}

fn internal(error: impl std::fmt::Display) -> Rejection {
    tracing::error!(error = %error, "native handoff failed");
    StatusCode::INTERNAL_SERVER_ERROR.into_response().into()
}
