use axum::extract::{Json, State};
use axum::http::StatusCode;
use cja::app_state::AppState;
use cja::server::session::{AppSession, Session};
use webauthn_rs::prelude::{Passkey, PublicKeyCredential, RequestChallengeResponse};

use crate::config::HasPasskeyConfig;
use crate::models::{credential, user};
use crate::session::{ChallengeState, PasskeySession};

#[derive(serde::Deserialize)]
pub struct AuthStartRequest {
    pub username: String,
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

    // Update credential counter (mandatory for cloning detection).
    let creds = credential::list_for_user(state.db(), user_id)
        .await
        .map_err(|err| {
            tracing::error!("list_for_user failed: {err}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let auth_cred_id = auth_result.cred_id();
    let mut found_pk: Option<(uuid::Uuid, Passkey)> = None;
    for c in creds {
        let pk: Passkey = match serde_json::from_value(c.credential_json.clone()) {
            Ok(p) => p,
            Err(err) => {
                tracing::error!(
                    "Passkey deserialize failed for credential {}: {err}",
                    c.credential_id_pk
                );
                continue;
            }
        };
        if pk.cred_id() == auth_cred_id {
            found_pk = Some((c.credential_id_pk, pk));
            break;
        }
    }

    if let Some((credential_id_pk, mut pk)) = found_pk {
        match pk.update_credential(&auth_result) {
            Some(true) => {
                let new_json = serde_json::to_value(&pk).map_err(|err| {
                    tracing::error!("serialize updated Passkey: {err}");
                    StatusCode::INTERNAL_SERVER_ERROR
                })?;
                credential::update_after_auth(
                    state.db(),
                    credential_id_pk,
                    new_json,
                    chrono::Utc::now(),
                )
                .await
                .map_err(|err| {
                    tracing::error!("update_after_auth failed: {err}");
                    StatusCode::INTERNAL_SERVER_ERROR
                })?;
            }
            Some(false) => {
                credential::update_last_used(state.db(), credential_id_pk)
                    .await
                    .map_err(|err| {
                        tracing::error!("update_last_used failed: {err}");
                        StatusCode::INTERNAL_SERVER_ERROR
                    })?;
            }
            None => {
                tracing::warn!("update_credential returned None for credential {credential_id_pk}");
            }
        }
    } else {
        tracing::warn!(
            "no matching credential found for cred_id during auth/finish for user {user_id}"
        );
    }

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
