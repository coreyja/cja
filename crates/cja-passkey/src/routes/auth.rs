use axum::extract::{Json, State};
use axum::http::StatusCode;
use cja::app_state::AppState;
use cja::server::session::{AppSession, Session};
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
    PasskeySession::set_challenge_state(state.db(), *session.session_id(), challenge_json)
        .await
        .map_err(|err| {
            tracing::error!("set_challenge_state failed: {err}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
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
    PasskeySession::set_challenge_state(state.db(), *session.session_id(), challenge_json)
        .await
        .map_err(|err| {
            tracing::error!("set_challenge_state failed: {err}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    Ok(Json(challenge))
}

pub async fn finish<S>(
    State(state): State<S>,
    Session(session): Session<PasskeySession>,
    Json(credential_payload): Json<PublicKeyCredential>,
) -> Result<StatusCode, StatusCode>
where
    S: AppState + HasPasskeyConfig,
{
    let Some(challenge_value) = session.challenge_state.clone() else {
        return Err(StatusCode::BAD_REQUEST);
    };
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

    let webauthn = state.passkey_config().webauthn.clone();
    let auth_result = webauthn
        .finish_passkey_authentication(&credential_payload, &auth_state)
        .map_err(|err| {
            tracing::warn!("finish_passkey_authentication failed: {err}");
            StatusCode::UNAUTHORIZED
        })?;

    let row = credential::find_by_credential_id(state.db(), auth_result.cred_id().as_ref())
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
    persist_authenticated_credential(state.db(), row, passkey, &auth_result).await?;

    let session_id = *session.session_id();
    PasskeySession::set_user_id(state.db(), session_id, user_id)
        .await
        .map_err(|err| {
            tracing::error!("set_user_id: {err}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    PasskeySession::clear_challenge_state(state.db(), session_id)
        .await
        .map_err(|err| {
            tracing::error!("clear_challenge_state: {err}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    Ok(StatusCode::OK)
}

pub async fn finish_discoverable<S>(
    State(state): State<S>,
    Session(session): Session<PasskeySession>,
    Json(credential_payload): Json<PublicKeyCredential>,
) -> Result<StatusCode, StatusCode>
where
    S: AppState + HasPasskeyConfig,
{
    let challenge_value = session
        .challenge_state
        .clone()
        .ok_or(StatusCode::BAD_REQUEST)?;
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
        .identify_discoverable_authentication(&credential_payload)
        .map_err(|_| StatusCode::UNAUTHORIZED)?;
    let row = credential::find_by_credential_id(state.db(), credential_id)
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
            &credential_payload,
            auth_state,
            &[DiscoverableKey::from(&passkey)],
        )
        .map_err(|err| {
            tracing::warn!("finish_discoverable_authentication failed: {err}");
            StatusCode::UNAUTHORIZED
        })?;
    persist_authenticated_credential(state.db(), row, passkey, &auth_result).await?;

    let session_id = *session.session_id();
    PasskeySession::set_user_id(state.db(), session_id, user_id)
        .await
        .map_err(|err| {
            tracing::error!("set_user_id: {err}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    PasskeySession::clear_challenge_state(state.db(), session_id)
        .await
        .map_err(|err| {
            tracing::error!("clear_challenge_state: {err}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    Ok(StatusCode::OK)
}

async fn persist_authenticated_credential(
    db: &sqlx::PgPool,
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
                db,
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
            credential::update_last_used(db, row.credential_id_pk)
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
