use axum::extract::{Json, State};
use axum::http::StatusCode;
use cja::app_state::AppState;
use cja::server::session::{AppSession, Session};
use webauthn_rs::prelude::{CreationChallengeResponse, RegisterPublicKeyCredential};

use crate::config::HasPasskeyConfig;
use crate::models::{credential, user};
use crate::session::{ChallengeState, PasskeySession};

#[derive(serde::Deserialize)]
pub struct RegisterStartRequest {
    pub username: String,
    pub display_name: Option<String>,
}

pub async fn start<S>(
    State(state): State<S>,
    Session(session): Session<PasskeySession>,
    Json(body): Json<RegisterStartRequest>,
) -> Result<Json<CreationChallengeResponse>, StatusCode>
where
    S: AppState + HasPasskeyConfig,
{
    let RegisterStartRequest {
        username,
        display_name,
    } = body;

    if username.trim().is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let exists = user::username_exists(state.db(), &username)
        .await
        .map_err(|err| {
            tracing::error!("username_exists failed: {err}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    if exists {
        return Err(StatusCode::CONFLICT);
    }

    let temp_uuid = uuid::Uuid::new_v4();
    let display_for_webauthn = display_name.as_deref().unwrap_or(&username);

    let webauthn = state.passkey_config().webauthn.clone();
    let (challenge, registration_state) = webauthn
        .start_passkey_registration(temp_uuid, &username, display_for_webauthn, None)
        .map_err(|err| {
            tracing::error!("start_passkey_registration failed: {err}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let challenge_state = ChallengeState::Registration {
        registration_state,
        user_unique_id: temp_uuid,
        username,
        display_name,
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
    Json(credential_payload): Json<RegisterPublicKeyCredential>,
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
    let ChallengeState::Registration {
        registration_state,
        user_unique_id,
        username,
        display_name,
    } = challenge_state
    else {
        return Err(StatusCode::BAD_REQUEST);
    };

    let webauthn = state.passkey_config().webauthn.clone();
    let passkey = webauthn
        .finish_passkey_registration(&credential_payload, &registration_state)
        .map_err(|err| {
            tracing::warn!("finish_passkey_registration failed: {err}");
            StatusCode::BAD_REQUEST
        })?;

    let credential_bytes: Vec<u8> = passkey.cred_id().as_ref().to_vec();
    let credential_json = serde_json::to_value(&passkey).map_err(|err| {
        tracing::error!("serialize Passkey: {err}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let mut tx = state.db().begin().await.map_err(|err| {
        tracing::error!("begin tx: {err}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    if let Err(err) =
        user::create(&mut *tx, user_unique_id, &username, display_name.as_deref()).await
    {
        tracing::warn!("user::create failed (likely race): {err}");
        return Err(StatusCode::CONFLICT);
    }

    credential::create(
        &mut *tx,
        user_unique_id,
        credential_json,
        &credential_bytes,
        None,
    )
    .await
    .map_err(|err| {
        tracing::error!("credential::create failed: {err}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    tx.commit().await.map_err(|err| {
        tracing::error!("commit tx: {err}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let session_id = *session.session_id();
    PasskeySession::set_user_id(state.db(), session_id, user_unique_id)
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
