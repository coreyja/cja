use std::time::Duration;

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::{IntoResponse, Response},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use cja::{
    app_state::AppState,
    server::{
        cookies::CookieJar,
        session::{
            AppSession, SESSION_COOKIE_NAME, Session, clear_session_cookie, save_session_cookie,
        },
    },
};
use openidconnect::{CsrfToken, Nonce, PkceCodeChallenge, Scope, core::CoreAuthenticationFlow};
use rand::RngCore as _;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use sqlx::Row;

use crate::{
    config::{AllowedSubject, HasPassportConfig},
    models::user,
    oidc::{self, TokenFailure},
    rejection::Rejection,
    session::PassportSession,
};

#[derive(Clone)]
pub(crate) enum Destination {
    Browser(String),
    Native(String),
    McpGrant(uuid::Uuid),
}

#[derive(Deserialize)]
pub(crate) struct LoginQuery {
    pub return_to: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct CallbackQuery {
    pub state: Option<String>,
    pub code: Option<String>,
    pub error: Option<String>,
}

fn response(code: StatusCode, body: &'static str) -> Rejection {
    (code, body).into_response().into()
}

pub(super) fn random_secret() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

pub(crate) fn hash(value: &str) -> Vec<u8> {
    Sha256::digest(value.as_bytes()).to_vec()
}

pub(crate) fn safe_return_to(path: &str) -> bool {
    path.starts_with('/')
        && !path.starts_with("//")
        && !path.contains(['\\', '#'])
        && !path.bytes().any(|b| b.is_ascii_control())
}

pub(crate) async fn begin<S: AppState + HasPassportConfig>(
    state: &S,
    session: &PassportSession,
    destination: Destination,
) -> Result<Response, Rejection> {
    let config = state.passport_config();
    let metadata = oidc::metadata(config, false)
        .await
        .map_err(|_| response(StatusCode::BAD_GATEWAY, "Identity provider unavailable"))?;
    let secret = config.client_secret.borrow().clone();
    let client = openidconnect::core::CoreClient::from_provider_metadata(
        metadata,
        config.client_id.clone(),
        Some(openidconnect::ClientSecret::new((*secret).clone())),
    )
    .set_redirect_uri(config.callback.clone());
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let (url, state_token, nonce) = client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .add_scope(Scope::new("email".into()))
        .add_scope(Scope::new("profile".into()))
        .set_pkce_challenge(challenge)
        .url();
    let (return_to, native_uri, oauth_transaction_id) = match destination {
        Destination::Browser(path) => (Some(path), None, None),
        Destination::Native(uri) => (None, Some(uri), None),
        Destination::McpGrant(id) => (None, None, Some(id)),
    };
    sqlx::query(
        "INSERT INTO passport_login_attempts
         (state_hash, session_id, nonce, pkce_verifier, return_to, native_redirect_uri, oauth_transaction_id, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, now() + interval '5 minutes')",
    )
    .bind(hash(state_token.secret()))
    .bind(*session.session_id())
    .bind(nonce.secret())
    .bind(verifier.secret())
    .bind(return_to)
    .bind(native_uri)
    .bind(oauth_transaction_id)
    .execute(state.db())
    .await
    .map_err(internal)?;
    Ok((
        StatusCode::SEE_OTHER,
        [(axum::http::header::LOCATION, url.to_string())],
    )
        .into_response())
}

fn internal(error: impl std::fmt::Display) -> Rejection {
    tracing::error!(error = %error, "passport request failed");
    response(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Authentication service error",
    )
}

pub(crate) async fn login<S: AppState + HasPassportConfig>(
    State(state): State<S>,
    Session(session): Session<PassportSession>,
    Query(query): Query<LoginQuery>,
) -> Result<Response, Rejection> {
    let path = query.return_to.unwrap_or_else(|| "/".to_owned());
    if !safe_return_to(&path) {
        return Err(response(StatusCode::BAD_REQUEST, "Invalid return path"));
    }
    begin(&state, &session, Destination::Browser(path)).await
}

pub(crate) async fn callback<S: AppState + HasPassportConfig + Send + Sync>(
    State(state): State<S>,
    jar: CookieJar<S>,
    Query(query): Query<CallbackQuery>,
) -> Result<Response, Rejection> {
    let state_value = query
        .state
        .as_deref()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| response(StatusCode::BAD_REQUEST, "Missing state"))?;
    let old_id = jar
        .get(SESSION_COOKIE_NAME)
        .and_then(|c| uuid::Uuid::parse_str(c.value()).ok())
        .ok_or_else(|| response(StatusCode::BAD_REQUEST, "Missing login session"))?;
    let mut tx = state.db().begin().await.map_err(internal)?;
    let attempt = sqlx::query(
        "DELETE FROM passport_login_attempts
         WHERE state_hash = $1 AND session_id = $2 AND expires_at > now()
         RETURNING nonce, pkce_verifier, return_to, native_redirect_uri, oauth_transaction_id",
    )
    .bind(hash(state_value))
    .bind(old_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(internal)?
    .ok_or_else(|| response(StatusCode::BAD_REQUEST, "Invalid or expired state"))?;
    let nonce: String = attempt.try_get("nonce").map_err(internal)?;
    let pkce: String = attempt.try_get("pkce_verifier").map_err(internal)?;
    let return_to: Option<String> = attempt.try_get("return_to").map_err(internal)?;
    let native_uri: Option<String> = attempt.try_get("native_redirect_uri").map_err(internal)?;
    let oauth_id: Option<uuid::Uuid> = attempt.try_get("oauth_transaction_id").map_err(internal)?;
    tx.commit().await.map_err(internal)?;

    if let Some(error) = query.error.as_deref() {
        return if error == "access_denied" {
            Err(response(
                StatusCode::FORBIDDEN,
                "A human Tailscale user is required",
            ))
        } else {
            Err(response(
                StatusCode::BAD_REQUEST,
                "Identity provider rejected login",
            ))
        };
    }
    let code = query
        .code
        .as_deref()
        .ok_or_else(|| response(StatusCode::BAD_REQUEST, "Missing authorization code"))?;
    let (identity, refresh_token) =
        oidc::exchange_code(state.passport_config(), code, &pkce, &nonce)
            .await
            .map_err(|failure| match failure {
                TokenFailure::Transient => {
                    response(StatusCode::BAD_GATEWAY, "Identity provider unavailable")
                }
                _ => response(StatusCode::UNAUTHORIZED, "Identity verification failed"),
            })?;
    let subject = AllowedSubject::new(&identity.subject)
        .map_err(|_| response(StatusCode::FORBIDDEN, "A human Tailscale user is required"))?;
    let config = state.passport_config();
    if identity.issuer != config.issuer.as_str() || !config.allowlist.contains(&subject) {
        return Err(response(StatusCode::FORBIDDEN, "User is not allowed"));
    }

    let destination = match (return_to, native_uri, oauth_id) {
        (Some(path), None, None) => Destination::Browser(path),
        (None, Some(uri), None) => Destination::Native(uri),
        (None, None, Some(id)) => Destination::McpGrant(id),
        _ => {
            return Err(response(
                StatusCode::BAD_REQUEST,
                "Invalid login destination",
            ));
        }
    };
    match destination {
        Destination::McpGrant(id) => {
            super::oauth::complete_login(&state, identity, refresh_token, id, old_id).await
        }
        other => finish_login(&state, &jar, old_id, identity, refresh_token, other).await,
    }
}

async fn finish_login<S: AppState + HasPassportConfig>(
    state: &S,
    jar: &CookieJar<S>,
    old_id: uuid::Uuid,
    identity: oidc::VerifiedIdentity,
    refresh_token: String,
    destination: Destination,
) -> Result<Response, Rejection> {
    let config = state.passport_config();
    let mut tx = state.db().begin().await.map_err(internal)?;
    let user = user::upsert(
        &mut tx,
        &identity.issuer,
        &identity.subject,
        identity.email.as_deref(),
        identity.preferred_username.as_deref(),
        identity.picture.as_deref(),
    )
    .await
    .map_err(internal)?;
    let deleted = sqlx::query("DELETE FROM sessions WHERE session_id = $1 RETURNING created_at")
        .bind(old_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(internal)?
        .ok_or_else(|| response(StatusCode::BAD_REQUEST, "Login session expired"))?;
    let created_at: jiff_sqlx::Timestamp = deleted.try_get("created_at").map_err(internal)?;
    let new_id: uuid::Uuid =
        sqlx::query_scalar("INSERT INTO sessions (created_at) VALUES ($1) RETURNING session_id")
            .bind(created_at)
            .fetch_one(&mut *tx)
            .await
            .map_err(internal)?;
    sqlx::query(
        "INSERT INTO passport_sessions
         (session_id, user_id, authenticated_at, last_refresh_at, next_refresh_at, refresh_token)
         VALUES ($1, $2, now(), now(), now() + make_interval(secs => $3), $4)",
    )
    .bind(new_id)
    .bind(user.user_id)
    .bind(config.timings.refresh_interval.as_secs_f64())
    .bind(refresh_token)
    .execute(&mut *tx)
    .await
    .map_err(internal)?;

    let target = match destination {
        Destination::Browser(path) => path,
        Destination::Native(native_uri) => {
            let handoff = random_secret();
            sqlx::query(
                "INSERT INTO passport_handoffs (code_hash, session_id, expires_at)
                 VALUES ($1, $2, now() + interval '60 seconds')",
            )
            .bind(hash(&handoff))
            .bind(new_id)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
            let mut uri = url::Url::parse(&native_uri).map_err(internal)?;
            uri.query_pairs_mut().append_pair("code", &handoff);
            uri.to_string()
        }
        Destination::McpGrant(_) => unreachable!("MCP logins complete without a browser session"),
    };
    tx.commit().await.map_err(internal)?;
    save_session_cookie(jar, new_id);
    Ok((
        StatusCode::SEE_OTHER,
        [
            (axum::http::header::LOCATION, target),
            (axum::http::header::CACHE_CONTROL, "no-store".to_owned()),
        ],
    )
        .into_response())
}

pub(crate) async fn logout<S: AppState + HasPassportConfig + Send + Sync>(
    State(state): State<S>,
    headers: HeaderMap,
    jar: CookieJar<S>,
) -> Result<Response, Rejection> {
    let id = if headers.contains_key(AUTHORIZATION) {
        let raw = headers
            .get(AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .ok_or_else(|| response(StatusCode::UNAUTHORIZED, "Invalid bearer"))?;
        let token = raw
            .strip_prefix("Bearer ")
            .filter(|v| !v.is_empty() && !v.bytes().any(|b| b.is_ascii_whitespace()))
            .ok_or_else(|| response(StatusCode::UNAUTHORIZED, "Invalid bearer"))?;
        sqlx::query_scalar::<_, uuid::Uuid>(
            "SELECT session_id FROM passport_sessions WHERE bearer_hash = $1",
        )
        .bind(hash(token))
        .fetch_optional(state.db())
        .await
        .map_err(internal)?
        .ok_or_else(|| response(StatusCode::UNAUTHORIZED, "Invalid bearer"))?
    } else {
        jar.get(SESSION_COOKIE_NAME)
            .and_then(|c| uuid::Uuid::parse_str(c.value()).ok())
            .ok_or_else(|| response(StatusCode::UNAUTHORIZED, "Missing session"))?
    };
    tokio::time::timeout(
        Duration::from_secs(12),
        sqlx::query("DELETE FROM sessions WHERE session_id = $1")
            .bind(id)
            .execute(state.db()),
    )
    .await
    .map_err(|_| response(StatusCode::GATEWAY_TIMEOUT, "Logout timed out"))?
    .map_err(internal)?;
    if !headers.contains_key(AUTHORIZATION) {
        clear_session_cookie(&jar);
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}
