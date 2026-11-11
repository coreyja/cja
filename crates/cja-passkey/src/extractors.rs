use axum::extract::FromRequestParts;
use cja::app_state::AppState;
use cja::server::session::Session;
use http::StatusCode;

use crate::models::user::{self, User};
use crate::session::PasskeySession;

pub struct CurrentUser(pub User);
pub struct OptionalUser(pub Option<User>);

async fn load_user_from_session<A: AppState>(
    parts: &mut http::request::Parts,
    state: &A,
) -> Result<Option<User>, StatusCode> {
    let session = Session::<PasskeySession>::from_request_parts(parts, state).await?;
    let Some(user_id) = session.0.user_id else {
        return Ok(None);
    };
    let user = user::find_by_id(state.db(), user_id).await.map_err(|err| {
        tracing::error!("Failed to load user {user_id}: {err}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    // Record only after verification (the user row exists): a record
    // cannot be withdrawn, so an anonymous session must stay anonymous.
    // `false` = this stack has no RequestUserLayer (hand-rolled serve
    // loop); the event stays anonymous rather than mis-attributing.
    if user.is_some()
        && !cja::server::trace::record_request_user(user_id, cja::server::trace::AUTH_TYPE_PASSKEY)
    {
        tracing::debug!("passkey identity not recorded: request stack lacks RequestUserLayer");
    }
    Ok(user)
}

impl<A> FromRequestParts<A> for CurrentUser
where
    A: AppState,
{
    type Rejection = StatusCode;

    async fn from_request_parts(
        parts: &mut http::request::Parts,
        state: &A,
    ) -> Result<Self, Self::Rejection> {
        match load_user_from_session(parts, state).await? {
            Some(user) => Ok(Self(user)),
            None => Err(StatusCode::UNAUTHORIZED),
        }
    }
}

impl<A> FromRequestParts<A> for OptionalUser
where
    A: AppState,
{
    type Rejection = StatusCode;

    async fn from_request_parts(
        parts: &mut http::request::Parts,
        state: &A,
    ) -> Result<Self, Self::Rejection> {
        let user = load_user_from_session(parts, state).await?;
        Ok(Self(user))
    }
}
