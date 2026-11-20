use std::{collections::HashSet, time::Duration};

use axum::{
    Form, Json, Router,
    extract::{DefaultBodyLimit, FromRequestParts, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header, request::Parts},
    middleware,
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use cja::{
    app_state::AppState,
    server::{
        cookies::CookieJar,
        session::{SESSION_COOKIE_NAME, Session},
    },
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

use super::web::{self, Destination};
use crate::{
    config::{AllowedSubject, HasMcpOAuthConfig, HasPassportConfig, McpOAuthConfig},
    models::user,
    oidc::{self, TokenFailure},
    rejection::Rejection,
    session::PassportSession,
};

pub fn oauth_router<S>() -> Router<S>
where
    S: AppState + HasPassportConfig + HasMcpOAuthConfig + Clone + Send + Sync + 'static,
{
    Router::new()
        .route(
            "/.well-known/oauth-authorization-server",
            get(auth_metadata::<S>),
        )
        .route(
            "/.well-known/oauth-authorization-server/mcp",
            get(auth_metadata::<S>),
        )
        .route(
            "/.well-known/oauth-protected-resource",
            get(resource_metadata::<S>),
        )
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(resource_metadata::<S>),
        )
        .route(
            "/oauth/register",
            post(register::<S>).layer(DefaultBodyLimit::max(22_000)),
        )
        .route("/oauth/authorize", get(authorize::<S>))
        .route(
            "/oauth/token",
            post(token::<S>).layer(middleware::map_response(token_cache_headers)),
        )
        .route(
            "/oauth/consent",
            post(consent::<S>).layer(middleware::map_response(token_cache_headers)),
        )
}

fn no_store(response: &mut Response) {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
        .headers_mut()
        .insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
}
async fn token_cache_headers(mut response: Response) -> Response {
    no_store(&mut response);
    response
}
fn oauth_error(status: StatusCode, error: &'static str) -> Response {
    (status, Json(serde_json::json!({"error": error}))).into_response()
}
fn db_error(error: impl std::fmt::Display) -> Response {
    tracing::error!(error = %error, "MCP OAuth database failure");
    oauth_error(StatusCode::INTERNAL_SERVER_ERROR, "server_error")
}
fn plain_error(status: StatusCode) -> Rejection {
    status.into()
}
fn random() -> String {
    web::random_secret()
}
fn hash(raw: &str) -> Vec<u8> {
    web::hash(raw)
}
fn redirect(uri: &str, key: &str, value: &str, state: &str) -> Response {
    let mut url = url::Url::parse(uri).expect("stored validated redirect URI");
    url.query_pairs_mut()
        .append_pair(key, value)
        .append_pair("state", state);
    let mut response = Redirect::to(url.as_str()).into_response();
    no_store(&mut response);
    response
}
fn scopes(config: &McpOAuthConfig) -> Vec<String> {
    let mut values: Vec<_> = config.options().scopes.iter().cloned().collect();
    values.sort();
    values
}
async fn auth_metadata<S: HasMcpOAuthConfig>(State(state): State<S>) -> Json<serde_json::Value> {
    let config = state.mcp_oauth_config();
    Json(serde_json::json!({
        "issuer": config.public_base_url().as_str().trim_end_matches('/'),
        "authorization_endpoint": config.endpoint("/oauth/authorize"),
        "token_endpoint": config.endpoint("/oauth/token"),
        "registration_endpoint": config.endpoint("/oauth/register"),
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "response_types_supported": ["code"],
        "token_endpoint_auth_methods_supported": ["none"],
        "code_challenge_methods_supported": ["S256"],
        "scopes_supported": scopes(config),
    }))
}
async fn resource_metadata<S: HasMcpOAuthConfig>(
    State(state): State<S>,
) -> Json<serde_json::Value> {
    let config = state.mcp_oauth_config();
    Json(serde_json::json!({
        "resource": config.mcp_resource_url().as_str(),
        "authorization_servers": [config.public_base_url().as_str().trim_end_matches('/')],
        "scopes_supported": scopes(config),
        "bearer_methods_supported": ["header"],
    }))
}

struct DcrAdmission;
impl<S: HasMcpOAuthConfig + Send + Sync> FromRequestParts<S> for DcrAdmission {
    type Rejection = Response;
    async fn from_request_parts(_: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        if state.mcp_oauth_config().limiter().take().await {
            Ok(Self)
        } else {
            let mut response =
                oauth_error(StatusCode::TOO_MANY_REQUESTS, "temporarily_unavailable");
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("60"));
            Err(response)
        }
    }
}
#[derive(Deserialize)]
struct Registration {
    redirect_uris: Vec<String>,
    client_name: Option<String>,
    token_endpoint_auth_method: Option<String>,
    grant_types: Option<Vec<String>>,
    response_types: Option<Vec<String>>,
}
#[derive(Serialize)]
struct Registered {
    client_id: String,
    client_name: String,
    redirect_uris: Vec<String>,
    token_endpoint_auth_method: &'static str,
    grant_types: [&'static str; 2],
    response_types: [&'static str; 1],
}
fn valid_registration(r: &Registration, config: &McpOAuthConfig) -> bool {
    let unique: HashSet<_> = r.redirect_uris.iter().collect();
    !r.redirect_uris.is_empty()
        && r.redirect_uris.len() <= 10
        && unique.len() == r.redirect_uris.len()
        && r.redirect_uris
            .iter()
            .all(|u| u.len() <= 2048 && config.options().allowed_redirect_uris.contains(u))
        && r.client_name.as_deref().is_none_or(|n| n.len() <= 200)
        && r.token_endpoint_auth_method
            .as_deref()
            .is_none_or(|m| m == "none")
        && r.grant_types.as_ref().is_none_or(|v| {
            !v.is_empty()
                && v.iter()
                    .all(|g| g == "authorization_code" || g == "refresh_token")
        })
        && r.response_types.as_ref().is_none_or(|v| v == &["code"])
}
async fn prune_clients(tx: &mut Transaction<'_, Postgres>, hours: i32) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM passport_oauth_clients c WHERE GREATEST(c.created_at, COALESCE(c.last_used_at,c.created_at)) < now()-make_interval(hours => $1) AND NOT EXISTS (SELECT 1 FROM passport_oauth_grants g WHERE g.oauth_client_id=c.id) AND NOT EXISTS (SELECT 1 FROM passport_oauth_login_transactions t WHERE t.oauth_client_id=c.id) AND NOT EXISTS (SELECT 1 FROM passport_oauth_consents s WHERE s.oauth_client_id=c.id) AND NOT EXISTS (SELECT 1 FROM passport_oauth_pending_consents p WHERE p.oauth_client_id=c.id)")
        .bind(hours).execute(&mut **tx).await?;
    Ok(())
}
async fn register<S: AppState + HasMcpOAuthConfig>(
    State(state): State<S>,
    _: DcrAdmission,
    body: Result<Json<Registration>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(r) = match body {
        Ok(body) => body,
        Err(error) if error.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            return error.into_response();
        }
        Err(_) => return oauth_error(StatusCode::BAD_REQUEST, "invalid_client_metadata"),
    };
    let config = state.mcp_oauth_config();
    if !valid_registration(&r, config) {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_client_metadata");
    }
    let mut tx = match state.db().begin().await {
        Ok(tx) => tx,
        Err(e) => return db_error(e),
    };
    if let Err(e) = sqlx::query("SELECT pg_advisory_xact_lock(2573)")
        .execute(&mut *tx)
        .await
    {
        return db_error(e);
    }
    if let Err(e) = prune_clients(&mut tx, 6).await {
        return db_error(e);
    }
    let mut count: i64 = match sqlx::query_scalar("SELECT count(*) FROM passport_oauth_clients")
        .fetch_one(&mut *tx)
        .await
    {
        Ok(n) => n,
        Err(e) => return db_error(e),
    };
    if count >= config.options().client_cap {
        if let Err(e) = prune_clients(&mut tx, 1).await {
            return db_error(e);
        }
        count = match sqlx::query_scalar("SELECT count(*) FROM passport_oauth_clients")
            .fetch_one(&mut *tx)
            .await
        {
            Ok(n) => n,
            Err(e) => return db_error(e),
        };
        if count >= config.options().client_cap {
            return oauth_error(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable");
        }
    }
    let client_id = random();
    let client_name = r.client_name.unwrap_or_else(|| "Claude".to_owned());
    if let Err(e) = sqlx::query(
        "INSERT INTO passport_oauth_clients(client_id,client_name,redirect_uris) VALUES($1,$2,$3)",
    )
    .bind(&client_id)
    .bind(&client_name)
    .bind(&r.redirect_uris)
    .execute(&mut *tx)
    .await
    {
        return db_error(e);
    }
    if let Err(e) = tx.commit().await {
        return db_error(e);
    }
    (
        StatusCode::CREATED,
        Json(Registered {
            client_id,
            client_name,
            redirect_uris: r.redirect_uris,
            token_endpoint_auth_method: "none",
            grant_types: ["authorization_code", "refresh_token"],
            response_types: ["code"],
        }),
    )
        .into_response()
}

#[derive(Deserialize)]
struct Authorize {
    response_type: Option<String>,
    client_id: Option<String>,
    redirect_uri: Option<String>,
    scope: Option<String>,
    state: Option<String>,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
    resource: Option<String>,
}
fn valid_challenge(s: &str) -> bool {
    s.len() == 43
        && URL_SAFE_NO_PAD
            .decode(s)
            .is_ok_and(|b| b.len() == 32 && URL_SAFE_NO_PAD.encode(b) == s)
}
fn valid_verifier(s: &str) -> bool {
    (43..=128).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
}
async fn authorize<S: AppState + HasPassportConfig + HasMcpOAuthConfig>(
    State(state): State<S>,
    Session(session): Session<PassportSession>,
    Query(q): Query<Authorize>,
) -> Response {
    let config = state.mcp_oauth_config();
    let (Some(cid), Some(uri), Some(resource), Some(scope), Some(client_state), Some(challenge)) = (
        q.client_id,
        q.redirect_uri,
        q.resource,
        q.scope,
        q.state,
        q.code_challenge,
    ) else {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_request");
    };
    let requested: Vec<String> = scope.split_whitespace().map(str::to_owned).collect();
    let unique: HashSet<_> = requested.iter().collect();
    if q.response_type.as_deref() != Some("code")
        || q.code_challenge_method.as_deref() != Some("S256")
        || resource != config.mcp_resource_url().as_str()
        || client_state.is_empty()
        || !valid_challenge(&challenge)
        || !requested.iter().any(|s| s == "mcp")
        || unique.len() != requested.len()
        || !requested
            .iter()
            .all(|s| config.options().scopes.contains(s))
        || !config.options().allowed_redirect_uris.contains(&uri)
    {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_request");
    }
    let row =
        sqlx::query("SELECT id, redirect_uris FROM passport_oauth_clients WHERE client_id=$1")
            .bind(cid)
            .fetch_optional(state.db())
            .await;
    let row = match row {
        Ok(Some(row)) => row,
        Ok(None) => return oauth_error(StatusCode::BAD_REQUEST, "invalid_client"),
        Err(e) => return db_error(e),
    };
    let id: Uuid = match row.try_get("id") {
        Ok(v) => v,
        Err(e) => return db_error(e),
    };
    let uris: Vec<String> = match row.try_get("redirect_uris") {
        Ok(v) => v,
        Err(e) => return db_error(e),
    };
    if !uris.contains(&uri) {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_request");
    }
    let transaction_id = Uuid::new_v4();
    if let Err(e) = sqlx::query("INSERT INTO passport_oauth_login_transactions(id,oauth_client_id,redirect_uri,resource,scopes,client_state,code_challenge,code_ttl_secs,grant_idle_ttl_secs,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,now()+interval '5 minutes')")
        .bind(transaction_id).bind(id).bind(&uri).bind(&resource).bind(&requested).bind(&client_state).bind(&challenge)
        .bind(config.options().timings.code_ttl.as_secs_f64()).bind(config.options().timings.grant_idle_ttl.as_secs_f64())
        .execute(state.db()).await { return db_error(e); }
    match web::begin(&state, &session, Destination::McpGrant(transaction_id)).await {
        Ok(response) => response,
        Err(rejection) => {
            let _ = sqlx::query("DELETE FROM passport_oauth_login_transactions WHERE id=$1")
                .bind(transaction_id)
                .execute(state.db())
                .await;
            rejection.into_response()
        }
    }
}

struct LoginSnapshot {
    oauth_client_id: Uuid,
    redirect_uri: String,
    resource: String,
    scopes: Vec<String>,
    client_state: String,
    code_challenge: String,
    code_ttl_secs: f64,
    grant_idle_ttl_secs: f64,
}
fn snapshot(row: &sqlx::postgres::PgRow) -> sqlx::Result<LoginSnapshot> {
    Ok(LoginSnapshot {
        oauth_client_id: row.try_get("oauth_client_id")?,
        redirect_uri: row.try_get("redirect_uri")?,
        resource: row.try_get("resource")?,
        scopes: row.try_get("scopes")?,
        client_state: row.try_get("client_state")?,
        code_challenge: row.try_get("code_challenge")?,
        code_ttl_secs: row.try_get("code_ttl_secs")?,
        grant_idle_ttl_secs: row.try_get("grant_idle_ttl_secs")?,
    })
}
async fn grant_code(
    tx: &mut Transaction<'_, Postgres>,
    s: &LoginSnapshot,
    user_id: Uuid,
    refresh: &str,
    checked_at: Option<jiff_sqlx::Timestamp>,
) -> sqlx::Result<String> {
    sqlx::query(
        "DELETE FROM passport_oauth_grants WHERE oauth_client_id=$1 AND user_id=$2 AND resource=$3",
    )
    .bind(s.oauth_client_id)
    .bind(user_id)
    .bind(&s.resource)
    .execute(&mut **tx)
    .await?;
    let grant_id: Uuid = sqlx::query_scalar("INSERT INTO passport_oauth_grants(oauth_client_id,user_id,resource,scopes,upstream_refresh_token,last_upstream_check_at,expires_at) VALUES($1,$2,$3,$4,$5,COALESCE($6,now()),now()+make_interval(secs => $7)) RETURNING id")
        .bind(s.oauth_client_id).bind(user_id).bind(&s.resource).bind(&s.scopes).bind(refresh).bind(checked_at).bind(s.grant_idle_ttl_secs)
        .fetch_one(&mut **tx).await?;
    let code = random();
    sqlx::query("INSERT INTO passport_oauth_authorization_codes(code_hash,grant_id,redirect_uri,code_challenge,expires_at) VALUES($1,$2,$3,$4,now()+make_interval(secs => $5))")
        .bind(hash(&code)).bind(grant_id).bind(&s.redirect_uri).bind(&s.code_challenge).bind(s.code_ttl_secs)
        .execute(&mut **tx).await?;
    sqlx::query("UPDATE passport_oauth_clients SET last_used_at=now() WHERE id=$1")
        .bind(s.oauth_client_id)
        .execute(&mut **tx)
        .await?;
    Ok(code)
}
fn escape_html(raw: &str) -> String {
    let mut result = String::with_capacity(raw.len());
    for c in raw.chars() {
        match c {
            '&' => result.push_str("&amp;"),
            '<' => result.push_str("&lt;"),
            '>' => result.push_str("&gt;"),
            '"' => result.push_str("&quot;"),
            '\'' => result.push_str("&#39;"),
            _ => result.push(c),
        }
    }
    result
}
fn consent_page(
    name: &str,
    resource: &str,
    scopes: &[String],
    uri: &str,
    user: &str,
    pending: Uuid,
    csrf: &str,
) -> Response {
    let body = format!(
        "<!doctype html><html><head><title>Allow MCP connector</title></head><body><h1>Allow MCP connector?</h1><p>Client: {} (self-reported)</p><p>App/resource: {}</p><p>Scopes: {}</p><p>Redirect URI: {}</p><p>Signed in as: {}</p><form method=\"post\" action=\"/oauth/consent\"><input type=\"hidden\" name=\"pending_id\" value=\"{}\"><input type=\"hidden\" name=\"csrf_token\" value=\"{}\"><button type=\"submit\" name=\"action\" value=\"allow\">Allow</button><button type=\"submit\" name=\"action\" value=\"deny\">Deny</button></form></body></html>",
        escape_html(name),
        escape_html(resource),
        escape_html(&scopes.join(" ")),
        escape_html(uri),
        escape_html(user),
        pending,
        escape_html(csrf)
    );
    let mut response = Html(body).into_response();
    response
        .headers_mut()
        .insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("frame-ancestors 'none'"),
    );
    no_store(&mut response);
    response
}
pub(super) async fn complete_login<S: AppState + HasPassportConfig>(
    state: &S,
    identity: oidc::VerifiedIdentity,
    upstream_refresh_token: String,
    transaction_id: Uuid,
    session_id: Uuid,
) -> Result<Response, Rejection> {
    let mut tx = state.db().begin().await.map_err(plain_internal)?;
    let row = sqlx::query("DELETE FROM passport_oauth_login_transactions WHERE id=$1 AND expires_at>now() RETURNING oauth_client_id,redirect_uri,resource,scopes,client_state,code_challenge,code_ttl_secs,grant_idle_ttl_secs")
        .bind(transaction_id).fetch_optional(&mut *tx).await.map_err(plain_internal)?
        .ok_or_else(|| plain_error(StatusCode::BAD_REQUEST))?;
    let s = snapshot(&row).map_err(plain_internal)?;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sessions WHERE session_id=$1)")
            .bind(session_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(plain_internal)?;
    if !exists {
        return Err(plain_error(StatusCode::BAD_REQUEST));
    }
    let user = user::upsert(
        &mut tx,
        &identity.issuer,
        &identity.subject,
        identity.email.as_deref(),
        identity.preferred_username.as_deref(),
        identity.picture.as_deref(),
    )
    .await
    .map_err(plain_internal)?;
    let has_consent: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM passport_oauth_consents WHERE oauth_client_id=$1 AND user_id=$2)")
        .bind(s.oauth_client_id).bind(user.user_id).fetch_one(&mut *tx).await.map_err(plain_internal)?;
    if has_consent {
        let code = grant_code(&mut tx, &s, user.user_id, &upstream_refresh_token, None)
            .await
            .map_err(plain_internal)?;
        tx.commit().await.map_err(plain_internal)?;
        return Ok(redirect(&s.redirect_uri, "code", &code, &s.client_state));
    }
    let client_name: String =
        sqlx::query_scalar("SELECT client_name FROM passport_oauth_clients WHERE id=$1")
            .bind(s.oauth_client_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(plain_internal)?;
    let pending_id = Uuid::new_v4();
    let csrf = random();
    sqlx::query("INSERT INTO passport_oauth_pending_consents(id,oauth_client_id,user_id,session_id,redirect_uri,resource,scopes,client_state,code_challenge,code_ttl_secs,grant_idle_ttl_secs,upstream_refresh_token,last_upstream_check_at,csrf_hash,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,now(),$13,now()+interval '5 minutes')")
        .bind(pending_id).bind(s.oauth_client_id).bind(user.user_id).bind(session_id)
        .bind(&s.redirect_uri).bind(&s.resource).bind(&s.scopes).bind(&s.client_state).bind(&s.code_challenge)
        .bind(s.code_ttl_secs).bind(s.grant_idle_ttl_secs).bind(upstream_refresh_token).bind(hash(&csrf))
        .execute(&mut *tx).await.map_err(plain_internal)?;
    tx.commit().await.map_err(plain_internal)?;
    let display = user
        .email
        .as_deref()
        .or(user.preferred_username.as_deref())
        .unwrap_or(&user.subject);
    Ok(consent_page(
        &client_name,
        &s.resource,
        &s.scopes,
        &s.redirect_uri,
        display,
        pending_id,
        &csrf,
    ))
}
fn plain_internal(error: impl std::fmt::Display) -> Rejection {
    tracing::error!(error = %error, "MCP OAuth login database failure");
    plain_error(StatusCode::INTERNAL_SERVER_ERROR)
}

#[derive(Deserialize)]
struct ConsentForm {
    pending_id: Uuid,
    csrf_token: String,
    action: String,
}
async fn consent<S: AppState>(
    State(state): State<S>,
    jar: CookieJar<S>,
    Form(form): Form<ConsentForm>,
) -> Response {
    let mut response = consent_inner(&state, &jar, form)
        .await
        .unwrap_or_else(IntoResponse::into_response);
    no_store(&mut response);
    response
}
async fn consent_inner<S: AppState>(
    state: &S,
    jar: &CookieJar<S>,
    form: ConsentForm,
) -> Result<Response, Rejection> {
    let cookie_id = jar
        .get(SESSION_COOKIE_NAME)
        .and_then(|c| Uuid::parse_str(c.value()).ok())
        .ok_or_else(|| plain_error(StatusCode::BAD_REQUEST))?;
    if form.action != "allow" && form.action != "deny" {
        return Err(plain_error(StatusCode::BAD_REQUEST));
    }
    let mut tx = state.db().begin().await.map_err(plain_internal)?;
    let row = sqlx::query("SELECT oauth_client_id,redirect_uri,resource,scopes,client_state,code_challenge,code_ttl_secs,grant_idle_ttl_secs,user_id,session_id,upstream_refresh_token,last_upstream_check_at,csrf_hash,expires_at>now() AS live FROM passport_oauth_pending_consents WHERE id=$1 FOR UPDATE")
        .bind(form.pending_id).fetch_optional(&mut *tx).await.map_err(plain_internal)?
        .ok_or_else(|| plain_error(StatusCode::BAD_REQUEST))?;
    let session_id: Uuid = row.try_get("session_id").map_err(plain_internal)?;
    let csrf_hash: Vec<u8> = row.try_get("csrf_hash").map_err(plain_internal)?;
    if cookie_id != session_id || csrf_hash != hash(&form.csrf_token) {
        return Err(plain_error(StatusCode::BAD_REQUEST));
    }
    let s = snapshot(&row).map_err(plain_internal)?;
    let user_id: Uuid = row.try_get("user_id").map_err(plain_internal)?;
    let refresh: String = row
        .try_get("upstream_refresh_token")
        .map_err(plain_internal)?;
    let checked_at: jiff_sqlx::Timestamp = row
        .try_get("last_upstream_check_at")
        .map_err(plain_internal)?;
    let live: bool = row.try_get("live").map_err(plain_internal)?;
    sqlx::query("DELETE FROM passport_oauth_pending_consents WHERE id=$1")
        .bind(form.pending_id)
        .execute(&mut *tx)
        .await
        .map_err(plain_internal)?;
    if !live || form.action == "deny" {
        tx.commit().await.map_err(plain_internal)?;
        return Ok(redirect(
            &s.redirect_uri,
            "error",
            "access_denied",
            &s.client_state,
        ));
    }
    sqlx::query("INSERT INTO passport_oauth_consents(oauth_client_id,user_id) VALUES($1,$2) ON CONFLICT DO NOTHING")
        .bind(s.oauth_client_id).bind(user_id).execute(&mut *tx).await.map_err(plain_internal)?;
    let code = grant_code(&mut tx, &s, user_id, &refresh, Some(checked_at))
        .await
        .map_err(plain_internal)?;
    tx.commit().await.map_err(plain_internal)?;
    Ok(redirect(&s.redirect_uri, "code", &code, &s.client_state))
}

#[derive(Deserialize)]
struct TokenForm {
    grant_type: String,
    code: Option<String>,
    redirect_uri: Option<String>,
    client_id: Option<String>,
    code_verifier: Option<String>,
    refresh_token: Option<String>,
    resource: Option<String>,
    client_secret: Option<String>,
}
async fn token<S: AppState + HasPassportConfig + HasMcpOAuthConfig>(
    State(state): State<S>,
    headers: HeaderMap,
    Form(form): Form<TokenForm>,
) -> Response {
    if headers.contains_key(header::AUTHORIZATION) || form.client_secret.is_some() {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_request");
    }
    match form.grant_type.as_str() {
        "authorization_code" => exchange(&state, &form).await,
        "refresh_token" => refresh(&state, &form).await,
        _ => oauth_error(StatusCode::BAD_REQUEST, "unsupported_grant_type"),
    }
}
async fn issue_tokens(
    tx: &mut Transaction<'_, Postgres>,
    grant_id: Uuid,
    scopes: &[String],
    access_ttl: Duration,
    consumed_refresh: Option<Uuid>,
) -> sqlx::Result<serde_json::Value> {
    let access = random();
    let refresh = random();
    sqlx::query("INSERT INTO passport_oauth_access_tokens(token_hash,grant_id,expires_at) VALUES($1,$2,LEAST(now()+make_interval(secs => $3),(SELECT expires_at FROM passport_oauth_grants WHERE id=$2)))")
        .bind(hash(&access)).bind(grant_id).bind(access_ttl.as_secs_f64()).execute(&mut **tx).await?;
    sqlx::query("INSERT INTO passport_oauth_refresh_tokens(token_hash,grant_id,expires_at) VALUES($1,$2,(SELECT expires_at FROM passport_oauth_grants WHERE id=$2))")
        .bind(hash(&refresh)).bind(grant_id).execute(&mut **tx).await?;
    if let Some(id) = consumed_refresh {
        // Deliberately the last SQL mutation before commit: a request already waiting on
        // the grant lock sees a concurrent duplicate, while a later replay revokes it.
        sqlx::query(
            "UPDATE passport_oauth_refresh_tokens SET consumed_at=clock_timestamp() WHERE id=$1",
        )
        .bind(id)
        .execute(&mut **tx)
        .await?;
    }
    let grant_remaining: i64 = sqlx::query_scalar(
        "SELECT GREATEST(0, FLOOR(EXTRACT(EPOCH FROM expires_at - clock_timestamp())))::bigint FROM passport_oauth_grants WHERE id=$1",
    )
    .bind(grant_id)
    .fetch_one(&mut **tx)
    .await?;
    // Report only whole seconds that both the access TTL and grant can cover.
    // A subsecond token must not claim a full second of validity.
    let seconds = access_ttl
        .as_secs()
        .min(u64::try_from(grant_remaining).unwrap_or_default());
    Ok(
        serde_json::json!({"access_token":access,"token_type":"Bearer","expires_in":seconds,
        "refresh_token":refresh,"scope":scopes.join(" ")}),
    )
}
async fn exchange<S: AppState + HasMcpOAuthConfig>(state: &S, form: &TokenForm) -> Response {
    let (Some(code), Some(client_id), Some(uri), Some(verifier), Some(resource)) = (
        &form.code,
        &form.client_id,
        &form.redirect_uri,
        &form.code_verifier,
        &form.resource,
    ) else {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_request");
    };
    if !valid_verifier(verifier) || resource != state.mcp_oauth_config().mcp_resource_url().as_str()
    {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant");
    }
    let mut tx = match state.db().begin().await {
        Ok(tx) => tx,
        Err(e) => return db_error(e),
    };
    let row = sqlx::query("SELECT ac.id,ac.grant_id,ac.redirect_uri,ac.code_challenge,g.resource,g.scopes,g.expires_at>now() AS grant_live,c.client_id,c.redirect_uris FROM passport_oauth_authorization_codes ac JOIN passport_oauth_grants g ON g.id=ac.grant_id JOIN passport_oauth_clients c ON c.id=g.oauth_client_id WHERE ac.code_hash=$1 AND ac.consumed_at IS NULL AND ac.expires_at>now() FOR UPDATE OF ac,g")
        .bind(hash(code)).fetch_optional(&mut *tx).await;
    let row = match row {
        Ok(Some(row)) => row,
        Ok(None) => return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant"),
        Err(e) => return db_error(e),
    };
    let check = (|| -> sqlx::Result<(Uuid, Uuid, Vec<String>, bool)> {
        let challenge: String = row.try_get("code_challenge")?;
        let computed = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let stored_uri: String = row.try_get("redirect_uri")?;
        let stored_resource: String = row.try_get("resource")?;
        let stored_client: String = row.try_get("client_id")?;
        let uris: Vec<String> = row.try_get("redirect_uris")?;
        let valid = challenge == computed
            && stored_uri == *uri
            && uris.contains(uri)
            && state
                .mcp_oauth_config()
                .options()
                .allowed_redirect_uris
                .contains(uri)
            && stored_resource == *resource
            && stored_client == *client_id
            && row.try_get::<bool, _>("grant_live")?;
        Ok((
            row.try_get("id")?,
            row.try_get("grant_id")?,
            row.try_get("scopes")?,
            valid,
        ))
    })();
    let (id, grant_id, scopes, valid) = match check {
        Ok(v) => v,
        Err(e) => return db_error(e),
    };
    if !valid {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant");
    }
    if let Err(e) =
        sqlx::query("UPDATE passport_oauth_authorization_codes SET consumed_at=now() WHERE id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await
    {
        return db_error(e);
    }
    let payload = match issue_tokens(
        &mut tx,
        grant_id,
        &scopes,
        state.mcp_oauth_config().options().timings.access_ttl,
        None,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return db_error(e),
    };
    if let Err(e) = tx.commit().await {
        return db_error(e);
    }
    Json(payload).into_response()
}

pub(crate) fn is_concurrent_duplicate(
    consumed_at: jiff::Timestamp,
    request_started_at: jiff::Timestamp,
) -> bool {
    consumed_at >= request_started_at
}
struct RefreshData {
    token_id: Uuid,
    user_id: Uuid,
    issuer: String,
    subject: String,
    upstream: String,
    client_id: String,
    resource: String,
    scopes: Vec<String>,
    live: bool,
}
impl RefreshData {
    fn decode(grant: &sqlx::postgres::PgRow, token: &sqlx::postgres::PgRow) -> sqlx::Result<Self> {
        Ok(Self {
            token_id: token.try_get("id")?,
            user_id: grant.try_get("user_id")?,
            issuer: grant.try_get("issuer")?,
            subject: grant.try_get("subject")?,
            upstream: grant.try_get("upstream_refresh_token")?,
            client_id: grant.try_get("client_id")?,
            resource: grant.try_get("resource")?,
            scopes: grant.try_get("scopes")?,
            live: grant.try_get::<bool, _>("live")? && token.try_get::<bool, _>("live")?,
        })
    }
}
async fn revoke_and_commit(mut tx: Transaction<'_, Postgres>, grant_id: Uuid) -> Response {
    if let Err(error) = sqlx::query("DELETE FROM passport_oauth_grants WHERE id=$1")
        .bind(grant_id)
        .execute(&mut *tx)
        .await
    {
        return db_error(error);
    }
    if let Err(error) = tx.commit().await {
        return db_error(error);
    }
    oauth_error(StatusCode::BAD_REQUEST, "invalid_grant")
}
async fn refresh<S: AppState + HasPassportConfig + HasMcpOAuthConfig>(
    state: &S,
    form: &TokenForm,
) -> Response {
    let (Some(raw), Some(client_id), Some(resource)) =
        (&form.refresh_token, &form.client_id, &form.resource)
    else {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_request");
    };
    if resource != state.mcp_oauth_config().mcp_resource_url().as_str() {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant");
    }
    let request_started: jiff_sqlx::Timestamp = match sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(state.db())
        .await
    {
        Ok(v) => v,
        Err(e) => return db_error(e),
    };
    let mut tx = match state.db().begin().await {
        Ok(tx) => tx,
        Err(e) => return db_error(e),
    };
    let grant_id: Option<Uuid> = match sqlx::query_scalar(
        "SELECT grant_id FROM passport_oauth_refresh_tokens WHERE token_hash=$1",
    )
    .bind(hash(raw))
    .fetch_optional(&mut *tx)
    .await
    {
        Ok(v) => v,
        Err(e) => return db_error(e),
    };
    let Some(grant_id) = grant_id else {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant");
    };
    let grant = sqlx::query("SELECT g.user_id,g.resource,g.scopes,g.upstream_refresh_token,g.expires_at>now() AS live,u.issuer,u.subject,c.client_id FROM passport_oauth_grants g JOIN passport_users u ON u.user_id=g.user_id JOIN passport_oauth_clients c ON c.id=g.oauth_client_id WHERE g.id=$1 FOR UPDATE OF g")
        .bind(grant_id).fetch_optional(&mut *tx).await;
    let grant = match grant {
        Ok(Some(v)) => v,
        Ok(None) => return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant"),
        Err(e) => return db_error(e),
    };
    // Read after locking the grant so a waiter sees committed consumption.
    let token = sqlx::query("SELECT id,consumed_at,expires_at>now() AS live FROM passport_oauth_refresh_tokens WHERE token_hash=$1 AND grant_id=$2")
        .bind(hash(raw)).bind(grant_id).fetch_optional(&mut *tx).await;
    let token = match token {
        Ok(Some(v)) => v,
        Ok(None) => return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant"),
        Err(e) => return db_error(e),
    };
    let consumed: Option<jiff_sqlx::Timestamp> = match token.try_get("consumed_at") {
        Ok(v) => v,
        Err(e) => return db_error(e),
    };
    if let Some(consumed) = consumed {
        if is_concurrent_duplicate(consumed.to_jiff(), request_started.to_jiff()) {
            return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant");
        }
        return revoke_and_commit(tx, grant_id).await;
    }
    let data = match RefreshData::decode(&grant, &token) {
        Ok(v) => v,
        Err(e) => return db_error(e),
    };
    if !data.live || data.client_id != *client_id || data.resource != *resource {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant");
    }
    let allowed = AllowedSubject::new(&data.subject).is_ok_and(|subject| {
        data.issuer == state.passport_config().issuer.as_str()
            && state.passport_config().allowlist.contains(&subject)
    });
    if !allowed {
        return revoke_and_commit(tx, grant_id).await;
    }
    finish_refresh(state, tx, grant_id, data).await
}
async fn finish_refresh<S: AppState + HasPassportConfig + HasMcpOAuthConfig>(
    state: &S,
    mut tx: Transaction<'_, Postgres>,
    grant_id: Uuid,
    data: RefreshData,
) -> Response {
    match oidc::refresh(
        state.passport_config(),
        &data.upstream,
        (&data.issuer, &data.subject),
    )
    .await
    {
        Ok(tokens) => {
            if let Some(identity) = tokens.identity
                && let Err(error) = user::refresh_display(
                    &mut tx,
                    data.user_id,
                    identity.email.as_deref(),
                    identity.preferred_username.as_deref(),
                    identity.picture.as_deref(),
                )
                .await
            {
                return db_error(error);
            }
            if let Err(error) = sqlx::query("UPDATE passport_oauth_grants SET upstream_refresh_token=$2,last_upstream_check_at=clock_timestamp(),expires_at=now()+make_interval(secs => $3),last_used_at=now() WHERE id=$1")
                .bind(grant_id).bind(tokens.refresh_token).bind(state.mcp_oauth_config().options().timings.grant_idle_ttl.as_secs_f64())
                .execute(&mut *tx).await { return db_error(error); }
        }
        Err(TokenFailure::InvalidGrant | TokenFailure::Rejected) => {
            return revoke_and_commit(tx, grant_id).await;
        }
        Err(TokenFailure::Transient) => {
            let within: bool = match sqlx::query_scalar("SELECT last_upstream_check_at > clock_timestamp()-make_interval(secs => $2) FROM passport_oauth_grants WHERE id=$1")
                .bind(grant_id).bind(state.mcp_oauth_config().options().timings.max_upstream_outage.as_secs_f64())
                .fetch_one(&mut *tx).await { Ok(v) => v, Err(error) => return db_error(error) };
            if !within {
                return revoke_and_commit(tx, grant_id).await;
            }
            if let Err(error) = sqlx::query("UPDATE passport_oauth_grants SET expires_at=now()+make_interval(secs => $2),last_used_at=now() WHERE id=$1")
                .bind(grant_id).bind(state.mcp_oauth_config().options().timings.grant_idle_ttl.as_secs_f64())
                .execute(&mut *tx).await { return db_error(error); }
        }
    }
    let payload = match issue_tokens(
        &mut tx,
        grant_id,
        &data.scopes,
        state.mcp_oauth_config().options().timings.access_ttl,
        Some(data.token_id),
    )
    .await
    {
        Ok(v) => v,
        Err(error) => return db_error(error),
    };
    if let Err(error) = tx.commit().await {
        return db_error(error);
    }
    Json(payload).into_response()
}
