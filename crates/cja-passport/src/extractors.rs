use axum::{
    extract::FromRequestParts,
    http::{
        StatusCode,
        header::{ACCEPT, AUTHORIZATION, WWW_AUTHENTICATE},
        request::Parts,
    },
    response::IntoResponse,
};
use cja::{
    app_state::AppState,
    server::session::{AppSession, Session},
};
use sha2::{Digest as _, Sha256};
use sqlx::Row;

use crate::{
    config::{AllowedSubject, HasMcpOAuthConfig, HasPassportConfig},
    models::user::User,
    rejection::Rejection,
    session::{PassportSession, resolve_user},
};

pub struct CurrentUser(pub User);
pub struct OptionalUser(pub Option<User>);
pub struct BearerUser(pub User);
pub struct McpUser(pub User);

fn mcp_unauthorized<A: HasMcpOAuthConfig>(state: &A) -> Rejection {
    let challenge = format!(
        "Bearer resource_metadata=\"{}\"",
        state
            .mcp_oauth_config()
            .endpoint("/.well-known/oauth-protected-resource/mcp")
    );
    (StatusCode::UNAUTHORIZED, [(WWW_AUTHENTICATE, challenge)])
        .into_response()
        .into()
}

impl<A: AppState + HasPassportConfig + HasMcpOAuthConfig + Send + Sync> FromRequestParts<A>
    for McpUser
{
    type Rejection = Rejection;
    async fn from_request_parts(parts: &mut Parts, state: &A) -> Result<Self, Rejection> {
        let mut headers = parts.headers.get_all(AUTHORIZATION).iter();
        let raw = headers
            .next()
            .and_then(|h| h.to_str().ok())
            .ok_or_else(|| mcp_unauthorized(state))?;
        if headers.next().is_some() {
            return Err(mcp_unauthorized(state));
        }
        let token = raw
            .strip_prefix("Bearer ")
            .filter(|s| !s.is_empty() && !s.bytes().any(|b| b.is_ascii_whitespace()))
            .ok_or_else(|| mcp_unauthorized(state))?;
        let digest = Sha256::digest(token.as_bytes()).to_vec();
        let id: Option<uuid::Uuid> = sqlx::query_scalar("SELECT g.user_id FROM passport_oauth_access_tokens a JOIN passport_oauth_grants g ON g.id=a.grant_id WHERE a.token_hash=$1 AND a.expires_at>now() AND g.expires_at>now() AND g.resource=$2 AND 'mcp'=ANY(g.scopes)")
            .bind(digest).bind(state.mcp_oauth_config().mcp_resource_url().as_str())
            .fetch_optional(state.db()).await.map_err(db_error)?;
        let id = id.ok_or_else(|| mcp_unauthorized(state))?;
        let user = crate::models::user::find_by_id(state.db(), id)
            .await
            .map_err(db_error)?
            .ok_or_else(|| mcp_unauthorized(state))?;
        let allowed = AllowedSubject::new(&user.subject).is_ok_and(|subject| {
            user.issuer == state.passport_config().issuer.as_str()
                && state.passport_config().allowlist.contains(&subject)
        });
        if !allowed {
            return Err(mcp_unauthorized(state));
        }
        if !cja::server::trace::record_request_user(
            user.user_id,
            cja::server::trace::AUTH_TYPE_PASSPORT,
        ) {
            tracing::debug!("passport request user could not be recorded outside request scope");
        }
        Ok(Self(user))
    }
}

async fn bearer<A: AppState + HasPassportConfig>(
    parts: &Parts,
    state: &A,
) -> Result<Option<User>, Rejection> {
    let raw = parts
        .headers
        .get(AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| Rejection::from(StatusCode::UNAUTHORIZED))?;
    let token = raw
        .strip_prefix("Bearer ")
        .filter(|value| !value.is_empty() && !value.bytes().any(|b| b.is_ascii_whitespace()))
        .ok_or_else(|| Rejection::from(StatusCode::UNAUTHORIZED))?;
    let digest = Sha256::digest(token.as_bytes()).to_vec();
    let row = sqlx::query("SELECT session_id FROM passport_sessions WHERE bearer_hash = $1")
        .bind(digest)
        .fetch_optional(state.db())
        .await
        .map_err(db_error)?;
    let Some(row) = row else {
        return Err(Rejection::from(StatusCode::UNAUTHORIZED));
    };
    let id = row.try_get("session_id").map_err(db_error)?;
    let user = resolve_user(state.db(), state.passport_config(), id)
        .await
        .map_err(db_error)?
        .ok_or_else(|| Rejection::from(StatusCode::UNAUTHORIZED))?;
    Ok(Some(user))
}

fn db_error(error: impl std::fmt::Display) -> Rejection {
    tracing::error!(error = %error, "passport database failure");
    StatusCode::INTERNAL_SERVER_ERROR.into()
}

async fn load<A: AppState + HasPassportConfig + Send + Sync>(
    parts: &mut Parts,
    state: &A,
    require_bearer: bool,
) -> Result<Option<User>, Rejection> {
    let user = if parts.headers.contains_key(AUTHORIZATION) || require_bearer {
        bearer(parts, state).await?
    } else {
        let session = Session::<PassportSession>::from_request_parts(parts, state)
            .await
            .map_err(Rejection::from)?;
        match session.0.user_id {
            Some(_) => resolve_user(state.db(), state.passport_config(), *session.0.session_id())
                .await
                .map_err(db_error)?,
            None => None,
        }
    };
    if let Some(ref user) = user
        && !cja::server::trace::record_request_user(
            user.user_id,
            cja::server::trace::AUTH_TYPE_PASSPORT,
        )
    {
        tracing::debug!("passport request user could not be recorded outside request scope");
    }
    Ok(user)
}

impl<A: AppState + HasPassportConfig + Send + Sync> FromRequestParts<A> for CurrentUser {
    type Rejection = Rejection;
    async fn from_request_parts(parts: &mut Parts, state: &A) -> Result<Self, Rejection> {
        if let Some(user) = load(parts, state, false).await? {
            return Ok(Self(user));
        }
        if !parts.headers.contains_key(AUTHORIZATION)
            && parts.method == axum::http::Method::GET
            && parts
                .headers
                .get(ACCEPT)
                .and_then(|h| h.to_str().ok())
                .is_some_and(|v| v.contains("text/html"))
        {
            let mut url = url::Url::parse("https://localhost/auth/login").expect("literal URL");
            let original = parts
                .extensions
                .get::<axum::extract::OriginalUri>()
                .map_or(&parts.uri, |original| &original.0);
            let return_to = original.path_and_query().map_or("/", |pq| pq.as_str());
            url.query_pairs_mut().append_pair("return_to", return_to);
            let location = format!("{}?{}", url.path(), url.query().unwrap_or_default());
            return Err((
                StatusCode::SEE_OTHER,
                [(axum::http::header::LOCATION, location)],
            )
                .into_response()
                .into());
        }
        Err(Rejection::from(StatusCode::UNAUTHORIZED))
    }
}

impl<A: AppState + HasPassportConfig + Send + Sync> FromRequestParts<A> for OptionalUser {
    type Rejection = Rejection;
    async fn from_request_parts(parts: &mut Parts, state: &A) -> Result<Self, Rejection> {
        Ok(Self(load(parts, state, false).await?))
    }
}

impl<A: AppState + HasPassportConfig + Send + Sync> FromRequestParts<A> for BearerUser {
    type Rejection = Rejection;
    async fn from_request_parts(parts: &mut Parts, state: &A) -> Result<Self, Rejection> {
        load(parts, state, true)
            .await?
            .map(Self)
            .ok_or_else(|| Rejection::from(StatusCode::UNAUTHORIZED))
    }
}
