use axum::extract::{Json, State};
use axum::http::StatusCode;
use cja::app_state::AppState;
use cja::server::cookies::CookieJar;
use cja::server::session::{AppSession, CJASession, Session};
use webauthn_rs::prelude::{
    AuthenticationResult, DiscoverableKey, Passkey, PublicKeyCredential, RequestChallengeResponse,
};

use crate::config::HasPasskeyConfig;
use crate::models::{credential, user};
use crate::session::{ChallengeState, PasskeySession};

#[derive(serde::Deserialize)]
pub struct AuthStartRequest {
    pub username: String,
}

/// Delete the current session row and clear the session cookie.
///
/// Also the revocation story for stolen cookies: the row (and, via the
/// `ON DELETE CASCADE` contract, its consumer rows) is gone, so the old
/// cookie value no longer authenticates even though it was never rotated.
pub async fn logout<S>(
    State(state): State<S>,
    Session(session): Session<PasskeySession>,
    jar: CookieJar<S>,
) -> Result<StatusCode, StatusCode>
where
    S: AppState,
{
    sqlx::query("DELETE FROM sessions WHERE session_id = $1")
        .bind(*session.session_id())
        .execute(state.db())
        .await
        .map_err(|err| {
            tracing::error!(
                "logout: failed to delete session {}: {err:#}",
                session.session_id()
            );
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    cja::server::session::clear_session_cookie(&jar);
    Ok(StatusCode::OK)
}

pub async fn start_discoverable<S>(
    State(state): State<S>,
    Session(session): Session<PasskeySession>,
) -> Result<Json<RequestChallengeResponse>, StatusCode>
where
    S: AppState + HasPasskeyConfig,
{
    let (challenge, auth_state) = state
        .passkey_config()
        .webauthn
        .start_discoverable_authentication()
        .map_err(|err| {
            tracing::error!("start_discoverable_authentication failed: {err}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let challenge_json =
        serde_json::to_value(ChallengeState::DiscoverableAuthentication { auth_state }).map_err(
            |err| {
                tracing::error!("serialize ChallengeState: {err}");
                StatusCode::INTERNAL_SERVER_ERROR
            },
        )?;
    super::store_challenge_state(state.db(), *session.session_id(), challenge_json).await?;
    Ok(Json(challenge))
}

pub async fn start<S>(
    State(state): State<S>,
    Session(session): Session<PasskeySession>,
    Json(body): Json<AuthStartRequest>,
) -> Result<Json<RequestChallengeResponse>, StatusCode>
where
    S: AppState + HasPasskeyConfig,
{
    let user_record = user::find_by_username(state.db(), &body.username)
        .await
        .map_err(|err| {
            tracing::error!("find_by_username failed: {err}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;

    let creds = credential::list_for_user(state.db(), user_record.user_id)
        .await
        .map_err(|err| {
            tracing::error!("list_for_user failed: {err}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    if creds.is_empty() {
        return Err(StatusCode::NOT_FOUND);
    }

    let passkeys: Vec<Passkey> = creds
        .iter()
        .map(|c| serde_json::from_value::<Passkey>(c.credential_json.clone()))
        .collect::<Result<_, _>>()
        .map_err(|err| {
            tracing::error!("Passkey deserialize failed: {err}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let webauthn = state.passkey_config().webauthn.clone();
    let (challenge, auth_state) =
        webauthn
            .start_passkey_authentication(&passkeys)
            .map_err(|err| {
                tracing::error!("start_passkey_authentication failed: {err}");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;

    // Note: deliberately do NOT set sessions.user_id here. Doing so would let any
    // caller knowing a username appear authenticated before completing the challenge.
    let challenge_state = ChallengeState::Authentication {
        auth_state,
        user_id: user_record.user_id,
    };
    let challenge_json = serde_json::to_value(&challenge_state).map_err(|err| {
        tracing::error!("serialize ChallengeState: {err}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    super::store_challenge_state(state.db(), *session.session_id(), challenge_json).await?;

    Ok(Json(challenge))
}

pub async fn finish<S>(
    State(state): State<S>,
    Session(session): Session<PasskeySession>,
    jar: CookieJar<S>,
    Json(credential_payload): Json<PublicKeyCredential>,
) -> Result<StatusCode, StatusCode>
where
    S: AppState + HasPasskeyConfig,
{
    let mut tx = state.db().begin().await.map_err(|err| {
        tracing::error!("begin tx: {err}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    match finish_named_tx(&state, &mut tx, *session.session_id(), &credential_payload).await {
        Ok(new_session) => {
            tx.commit().await.map_err(|err| {
                tracing::error!("commit finish tx: {err}");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
            cja::server::session::save_session_cookie(&jar, new_session.session_id);
            Ok(StatusCode::OK)
        }
        Err(status) => {
            if let Err(err) = tx.rollback().await {
                tracing::warn!("rollback finish tx: {err}");
            }
            Err(status)
        }
    }
}

async fn finish_named_tx<S>(
    state: &S,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: uuid::Uuid,
    credential_payload: &PublicKeyCredential,
) -> Result<CJASession, StatusCode>
where
    S: AppState + HasPasskeyConfig,
{
    // 1. Lock the row and read the challenge (NOT the extractor snapshot).
    let Some(challenge_value) = PasskeySession::lock_challenge_state(&mut **tx, session_id)
        .await
        .map_err(|err| {
            tracing::error!("lock_challenge_state: {err:#}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
    else {
        return Err(StatusCode::BAD_REQUEST); // no challenge / already consumed
    };
    // 2. Deserialize + variant-check (same logic as before, but on the locked value).
    let challenge_state: ChallengeState =
        serde_json::from_value(challenge_value).map_err(|err| {
            tracing::warn!("invalid challenge_state JSON: {err}");
            StatusCode::BAD_REQUEST
        })?;
    let ChallengeState::Authentication {
        auth_state,
        user_id,
    } = challenge_state
    else {
        return Err(StatusCode::BAD_REQUEST);
    };
    // 3. Verify the ceremony (on failure the outer half rolls back,
    //    so the challenge survives a 401 — retry semantics).
    let webauthn = state.passkey_config().webauthn.clone();
    let auth_result = webauthn
        .finish_passkey_authentication(credential_payload, &auth_state)
        .map_err(|err| {
            tracing::warn!("finish_passkey_authentication failed: {err}");
            StatusCode::UNAUTHORIZED
        })?;
    let row = credential::find_by_credential_id(&mut **tx, auth_result.cred_id().as_ref())
        .await
        .map_err(|err| {
            tracing::error!("find_by_credential_id failed: {err}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .filter(|row| row.user_id == user_id)
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let passkey: Passkey = serde_json::from_value(row.credential_json.clone()).map_err(|err| {
        tracing::error!("Passkey deserialize failed: {err}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    persist_authenticated_credential(&mut **tx, row, passkey, &auth_result).await?;
    // 4. Rotate + consume, atomically with everything above.
    PasskeySession::rotate_session_id(&mut **tx, session_id, user_id)
        .await
        .map_err(|err| {
            tracing::error!("rotate_session_id: {err:#}");
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

pub async fn finish_discoverable<S>(
    State(state): State<S>,
    Session(session): Session<PasskeySession>,
    jar: CookieJar<S>,
    Json(credential_payload): Json<PublicKeyCredential>,
) -> Result<StatusCode, StatusCode>
where
    S: AppState + HasPasskeyConfig,
{
    let mut tx = state.db().begin().await.map_err(|err| {
        tracing::error!("begin tx: {err}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    match finish_discoverable_tx(&state, &mut tx, *session.session_id(), &credential_payload).await
    {
        Ok(new_session) => {
            tx.commit().await.map_err(|err| {
                tracing::error!("commit finish tx: {err}");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
            cja::server::session::save_session_cookie(&jar, new_session.session_id);
            Ok(StatusCode::OK)
        }
        Err(status) => {
            if let Err(err) = tx.rollback().await {
                tracing::warn!("rollback finish tx: {err}");
            }
            Err(status)
        }
    }
}

async fn finish_discoverable_tx<S>(
    state: &S,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: uuid::Uuid,
    credential_payload: &PublicKeyCredential,
) -> Result<CJASession, StatusCode>
where
    S: AppState + HasPasskeyConfig,
{
    let Some(challenge_value) = PasskeySession::lock_challenge_state(&mut **tx, session_id)
        .await
        .map_err(|err| {
            tracing::error!("lock_challenge_state: {err:#}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
    else {
        return Err(StatusCode::BAD_REQUEST); // no challenge / already consumed
    };
    let challenge_state: ChallengeState =
        serde_json::from_value(challenge_value).map_err(|err| {
            tracing::warn!("invalid challenge_state JSON: {err}");
            StatusCode::BAD_REQUEST
        })?;
    let ChallengeState::DiscoverableAuthentication { auth_state } = challenge_state else {
        return Err(StatusCode::BAD_REQUEST);
    };

    let webauthn = &state.passkey_config().webauthn;
    let (user_id, credential_id) = webauthn
        .identify_discoverable_authentication(credential_payload)
        .map_err(|err| {
            tracing::warn!("identify_discoverable_authentication failed: {err}");
            StatusCode::UNAUTHORIZED
        })?;
    let row = credential::find_by_credential_id(&mut **tx, credential_id)
        .await
        .map_err(|err| {
            tracing::error!("find_by_credential_id failed: {err}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .filter(|row| row.user_id == user_id)
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let passkey: Passkey = serde_json::from_value(row.credential_json.clone()).map_err(|err| {
        tracing::error!("Passkey deserialize failed: {err}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let auth_result = webauthn
        .finish_discoverable_authentication(
            credential_payload,
            auth_state,
            &[DiscoverableKey::from(&passkey)],
        )
        .map_err(|err| {
            tracing::warn!("finish_discoverable_authentication failed: {err}");
            StatusCode::UNAUTHORIZED
        })?;
    persist_authenticated_credential(&mut **tx, row, passkey, &auth_result).await?;

    PasskeySession::rotate_session_id(&mut **tx, session_id, user_id)
        .await
        .map_err(|err| {
            tracing::error!("rotate_session_id: {err:#}");
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

async fn persist_authenticated_credential(
    executor: impl sqlx::Executor<'_, Database = sqlx::Postgres>,
    row: credential::PasskeyCredential,
    mut passkey: Passkey,
    result: &AuthenticationResult,
) -> Result<(), StatusCode> {
    if passkey.cred_id() != result.cred_id() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    match passkey.update_credential(result) {
        Some(true) => {
            let credential_json = serde_json::to_value(&passkey).map_err(|err| {
                tracing::error!("serialize updated Passkey: {err}");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
            credential::update_after_auth(
                executor,
                row.credential_id_pk,
                credential_json,
                chrono::Utc::now(),
            )
            .await
            .map_err(|err| {
                tracing::error!("update_after_auth failed: {err}");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
        }
        Some(false) => {
            credential::update_last_used(executor, row.credential_id_pk)
                .await
                .map_err(|err| {
                    tracing::error!("update_last_used failed: {err}");
                    StatusCode::INTERNAL_SERVER_ERROR
                })?;
        }
        None => return Err(StatusCode::UNAUTHORIZED),
    }
    Ok(())
}
