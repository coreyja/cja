use axum::extract::{Json, State};
use axum::http::StatusCode;
use cja::app_state::AppState;
use cja::server::cookies::CookieJar;
use cja::server::session::{AppSession, CJASession, Session};
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
    let (mut challenge, registration_state) = webauthn
        .start_passkey_registration(temp_uuid, &username, display_for_webauthn, None)
        .map_err(|err| {
            tracing::error!("start_passkey_registration failed: {err}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let selection = challenge
        .public_key
        .authenticator_selection
        .as_mut()
        .ok_or_else(|| {
            tracing::error!("registration challenge has no authenticator_selection");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    selection.resident_key = Some(webauthn_rs_proto::ResidentKeyRequirement::Preferred);

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

    super::store_challenge_state(state.db(), *session.session_id(), challenge_json).await?;

    Ok(Json(challenge))
}

pub async fn finish<S>(
    State(state): State<S>,
    Session(session): Session<PasskeySession>,
    jar: CookieJar<S>,
    Json(credential_payload): Json<RegisterPublicKeyCredential>,
) -> Result<StatusCode, StatusCode>
where
    S: AppState + HasPasskeyConfig,
{
    let mut tx = state.db().begin().await.map_err(|err| {
        tracing::error!("begin tx: {err}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    match register_finish_tx(&state, &mut tx, *session.session_id(), &credential_payload).await {
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

async fn register_finish_tx<S>(
    state: &S,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: uuid::Uuid,
    credential_payload: &RegisterPublicKeyCredential,
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
        .finish_passkey_registration(credential_payload, &registration_state)
        .map_err(|err| {
            tracing::warn!("finish_passkey_registration failed: {err}");
            StatusCode::BAD_REQUEST
        })?;
    let credential_bytes: Vec<u8> = passkey.cred_id().as_ref().to_vec();
    let credential_json = serde_json::to_value(&passkey).map_err(|err| {
        tracing::error!("serialize Passkey: {err}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // user + credential + session rotation all in ONE tx (the lock already opened it):
    // a failure anywhere leaves neither "account exists, session anonymous" nor a
    // half-consumed challenge — and the rotation's cascade removes consumer
    // pre-auth rows (e.g. chess's registration_authorizations) atomically.
    if let Err(err) = user::create(
        &mut **tx,
        user_unique_id,
        &username,
        display_name.as_deref(),
    )
    .await
    {
        let unique = err
            .chain()
            .find_map(|cause| cause.downcast_ref::<sqlx::Error>())
            .and_then(sqlx::Error::as_database_error)
            .is_some_and(|db| db.is_unique_violation());
        if unique {
            tracing::warn!("user::create unique violation (registration race): {err:#}");
            return Err(StatusCode::CONFLICT);
        }
        tracing::error!("user::create failed: {err:#}");
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    credential::create(
        &mut **tx,
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
    PasskeySession::rotate_session_id(&mut **tx, session_id, user_unique_id)
        .await
        .map_err(|err| {
            tracing::error!("rotate_session_id: {err:#}");
            StatusCode::INTERNAL_SERVER_ERROR
        })
}
