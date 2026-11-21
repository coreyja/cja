#![cfg(feature = "tailnet")]
pub mod common;

use axum::http::StatusCode;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use common::{Fixture, cookie, jwt};
use httpmock::Method::POST;
use serde_json::{Value, json};
use setec::{Connection, DialFuture, Dialer};
use sha2::{Digest as _, Sha256};
use std::{
    io,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::net::{TcpListener, TcpStream};

struct RecordingDialer {
    target: Mutex<Option<SocketAddr>>,
    calls: Mutex<Vec<(String, u16)>>,
    enabled: AtomicBool,
}

impl RecordingDialer {
    fn new() -> Self {
        Self {
            target: Mutex::new(None),
            calls: Mutex::new(Vec::new()),
            enabled: AtomicBool::new(true),
        }
    }
    fn set_target(&self, target: SocketAddr) {
        *self.target.lock().unwrap() = Some(target);
    }
    fn calls(&self) -> Vec<(String, u16)> {
        self.calls.lock().unwrap().clone()
    }
    fn fail(&self) {
        self.enabled.store(false, Ordering::SeqCst);
    }
}

impl Dialer for RecordingDialer {
    fn dial<'a>(&'a self, host: &'a str, port: u16) -> DialFuture<'a> {
        Box::pin(async move {
            self.calls.lock().unwrap().push((host.to_owned(), port));
            if !self.enabled.load(Ordering::SeqCst) {
                return Err(io::Error::from(io::ErrorKind::ConnectionRefused));
            }
            let target = self
                .target
                .lock()
                .unwrap()
                .expect("mock target initialized");
            let stream = TcpStream::connect(target).await?;
            Ok(Box::new(stream) as Box<dyn Connection>)
        })
    }
}

async fn fixture() -> (Fixture, Arc<RecordingDialer>, TcpListener) {
    // Holding this listener makes host-TCP fallback hang rather than reach the
    // mock IdP. The dialer forwards to the separate mock server port.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let dialer = Arc::new(RecordingDialer::new());
    let f = Fixture::with_dialer(format!("http://127.0.0.1:{port}"), dialer.clone()).await;
    dialer.set_target(*f.server.address());
    (f, dialer, listener)
}

#[tokio::test]
async fn browser_discovery_jwks_code_and_cookie_refresh_use_dialer() {
    let (f, dialer, _listener) = fixture().await;
    let port = url::Url::parse(&f.issuer).unwrap().port().unwrap();
    let (_, old_cookie, state) = f.start("/auth/login?return_to=%2Fprivate").await;
    let nonce = f.nonce().await;
    let code = f
        .token_mock(jwt(&f.issuer, &nonce, "userid:123"), "refresh-1")
        .await;
    let callback = f.callback(&old_cookie, &state).await;
    assert_eq!(callback.status(), StatusCode::SEE_OTHER);
    assert_eq!(callback.headers()["location"], "/private");
    code.assert_calls_async(1).await;
    let session_cookie = cookie(&callback);
    let upstream =
        f.server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/token")
                    .form_urlencoded_tuple("grant_type", "refresh_token")
                    .form_urlencoded_tuple("refresh_token", "refresh-1");
                then.status(200).header("content-type", "application/json").json_body(json!({
            "access_token":"new-access", "token_type":"Bearer", "refresh_token":"refresh-2"
        }));
            })
            .await;
    sqlx::query("UPDATE passport_sessions SET next_refresh_at = now() - interval '1 second'")
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(
        f.request("GET", "/private", Some(&session_cookie), None, None)
            .await
            .status(),
        StatusCode::OK
    );
    upstream.assert_calls_async(1).await;
    let stored: String = sqlx::query_scalar("SELECT refresh_token FROM passport_sessions")
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(stored, "refresh-2");
    assert_eq!(dialer.calls(), vec![("127.0.0.1".into(), port); 4]);
    f.close().await;
}

#[tokio::test]
async fn advertised_token_endpoint_uses_its_own_original_host() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let issuer = format!("http://127.0.0.1:{port}");
    let dialer = Arc::new(RecordingDialer::new());
    let f = Fixture::with_dialer_and_token_endpoint(
        issuer,
        format!("http://localhost:{port}/token"),
        dialer.clone(),
    )
    .await;
    dialer.set_target(*f.server.address());
    let (_, cookie, state) = f.start("/auth/login").await;
    let nonce = f.nonce().await;
    f.token_mock(jwt(&f.issuer, &nonce, "userid:123"), "refresh-1")
        .await;
    let callback = f.callback(&cookie, &state).await;
    assert_eq!(callback.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        dialer.calls(),
        vec![
            ("127.0.0.1".into(), port),
            ("127.0.0.1".into(), port),
            ("localhost".into(), port),
        ]
    );
    f.close().await;
}

#[tokio::test]
async fn refused_dial_has_no_host_fallback_and_token_failure_keeps_no_user() {
    let (f, dialer, _listener) = fixture().await;
    dialer.fail();
    assert_eq!(
        f.request("GET", "/auth/login", None, None, None)
            .await
            .status(),
        StatusCode::BAD_GATEWAY
    );
    assert_eq!(dialer.calls().len(), 1);
    f.close().await;

    let (f, dialer, _listener) = fixture().await;
    let (_, cookie, state) = f.start("/auth/login").await;
    dialer.fail();
    assert_eq!(
        f.callback(&cookie, &state).await.status(),
        StatusCode::BAD_GATEWAY
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM passport_users")
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert_eq!(dialer.calls().len(), 3);
    f.close().await;
}

#[tokio::test]
async fn native_callback_and_bearer_refresh_use_dialer_but_handoff_is_local() {
    let (f, dialer, _listener) = fixture().await;
    let (_, cookie, state) = f
        .start("/auth/native/start?redirect_uri=testapp%3A%2F%2Fauth")
        .await;
    let nonce = f.nonce().await;
    let code = f
        .token_mock(jwt(&f.issuer, &nonce, "userid:123"), "native-refresh-1")
        .await;
    let callback = f.callback(&cookie, &state).await;
    assert_eq!(callback.status(), StatusCode::SEE_OTHER);
    code.assert_calls_async(1).await;
    let location = url::Url::parse(callback.headers()["location"].to_str().unwrap()).unwrap();
    let handoff = location
        .query_pairs()
        .find(|(name, _)| name == "code")
        .unwrap()
        .1
        .to_string();
    let before_handoff = dialer.calls();
    let exchange = f
        .request(
            "POST",
            "/auth/native/exchange",
            None,
            None,
            Some(&json!({"code":handoff}).to_string()),
        )
        .await;
    assert_eq!(exchange.status(), StatusCode::OK);
    assert_eq!(dialer.calls(), before_handoff);
    let bearer: Value = serde_json::from_slice(
        &axum::body::to_bytes(exchange.into_body(), 2048)
            .await
            .unwrap(),
    )
    .unwrap();
    let bearer = format!("Bearer {}", bearer["token"].as_str().unwrap());
    let refresh = f
        .server
        .mock_async(|when, then| {
            when.method(POST)
                .path("/token")
                .form_urlencoded_tuple("grant_type", "refresh_token")
                .form_urlencoded_tuple("refresh_token", "native-refresh-1");
            then.status(200).header("content-type", "application/json").json_body(json!({
            "access_token":"new-access", "token_type":"Bearer", "refresh_token":"native-refresh-2"
        }));
        })
        .await;
    sqlx::query("UPDATE passport_sessions SET next_refresh_at = now() - interval '1 second'")
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(
        f.request("GET", "/api", None, Some(&bearer), None)
            .await
            .status(),
        StatusCode::OK
    );
    refresh.assert_calls_async(1).await;
    assert_eq!(dialer.calls().len(), before_handoff.len() + 1);
    f.close().await;
}

const MCP_CALLBACK: &str = "https://claude.ai/api/mcp/auth_callback";
const MCP_VERIFIER: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-._~";

async fn mcp_login_and_approve(f: &Fixture) -> (String, String) {
    let register = f.request("POST", "/oauth/register", None, None,
        Some(&json!({"redirect_uris":[MCP_CALLBACK],"client_name":"Claude","token_endpoint_auth_method":"none"}).to_string())).await;
    assert_eq!(register.status(), StatusCode::CREATED);
    let registered: Value = serde_json::from_slice(
        &axum::body::to_bytes(register.into_body(), 8192)
            .await
            .unwrap(),
    )
    .unwrap();
    let client_id = registered["client_id"].as_str().unwrap();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(MCP_VERIFIER.as_bytes()));
    let mut authorize = url::Url::parse("https://app.example.test/oauth/authorize").unwrap();
    authorize
        .query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", MCP_CALLBACK)
        .append_pair("scope", "mcp offline_access")
        .append_pair("state", "client-state")
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("resource", "https://app.example.test/mcp");
    let (_, cookie, state) = f
        .start(&format!(
            "{}?{}",
            authorize.path(),
            authorize.query().unwrap()
        ))
        .await;
    let nonce = f.nonce().await;
    let code_mock = f
        .token_mock(jwt(&f.issuer, &nonce, "userid:123"), "mcp-upstream-1")
        .await;
    let callback = f.callback(&cookie, &state).await;
    assert_eq!(callback.status(), StatusCode::OK);
    code_mock.assert_calls_async(1).await;
    let html = String::from_utf8(
        axum::body::to_bytes(callback.into_body(), 65536)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    let field = |name: &str| -> String {
        html.split(&format!("name=\"{name}\" value=\""))
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap()
            .to_owned()
    };
    let pending = field("pending_id");
    let csrf = field("csrf_token");
    let approved = f
        .form(
            "/oauth/consent",
            Some(&cookie),
            &[
                ("pending_id", &pending),
                ("csrf_token", &csrf),
                ("action", "allow"),
            ],
        )
        .await;
    assert_eq!(approved.status(), StatusCode::SEE_OTHER);
    let location = url::Url::parse(approved.headers()["location"].to_str().unwrap()).unwrap();
    let code = location
        .query_pairs()
        .find(|(name, _)| name == "code")
        .unwrap()
        .1
        .to_string();
    (client_id.to_owned(), code)
}

#[tokio::test]
async fn mcp_grant_login_and_upstream_refresh_use_same_dialer() {
    let (f, dialer, _listener) = fixture().await;
    let (client_id, code) = mcp_login_and_approve(&f).await;
    let client_id = client_id.as_str();
    let upstream_before_local = dialer.calls().len();
    let token = f
        .form(
            "/oauth/token",
            None,
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("client_id", client_id),
                ("redirect_uri", MCP_CALLBACK),
                ("code_verifier", MCP_VERIFIER),
            ],
        )
        .await;
    assert_eq!(token.status(), StatusCode::OK);
    assert_eq!(dialer.calls().len(), upstream_before_local);
    let token: Value =
        serde_json::from_slice(&axum::body::to_bytes(token.into_body(), 8192).await.unwrap())
            .unwrap();
    let local_refresh = token["refresh_token"].as_str().unwrap();
    let refresh = f
        .server
        .mock_async(|when, then| {
            when.method(POST)
                .path("/token")
                .form_urlencoded_tuple("grant_type", "refresh_token")
                .form_urlencoded_tuple("refresh_token", "mcp-upstream-1");
            then.status(200).header("content-type", "application/json").json_body(json!({
            "access_token":"new-access", "token_type":"Bearer", "refresh_token":"mcp-upstream-2"
        }));
        })
        .await;
    let refreshed = f
        .form(
            "/oauth/token",
            None,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", local_refresh),
                ("client_id", client_id),
            ],
        )
        .await;
    assert_eq!(refreshed.status(), StatusCode::OK);
    refresh.assert_calls_async(1).await;
    assert_eq!(dialer.calls().len(), upstream_before_local + 1);
    let refreshed: Value = serde_json::from_slice(
        &axum::body::to_bytes(refreshed.into_body(), 8192)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_mcp_outage(&f, &dialer, client_id, refreshed).await;
    f.close().await;
}

async fn assert_mcp_outage(
    f: &Fixture,
    dialer: &RecordingDialer,
    client_id: &str,
    refreshed: Value,
) {
    dialer.fail();
    let outage = f
        .form(
            "/oauth/token",
            None,
            &[
                ("grant_type", "refresh_token"),
                (
                    "refresh_token",
                    refreshed["refresh_token"].as_str().unwrap(),
                ),
                ("client_id", client_id),
            ],
        )
        .await;
    assert_eq!(outage.status(), StatusCode::OK);
    let outage: Value = serde_json::from_slice(
        &axum::body::to_bytes(outage.into_body(), 8192)
            .await
            .unwrap(),
    )
    .unwrap();
    let upstream: String =
        sqlx::query_scalar("SELECT upstream_refresh_token FROM passport_oauth_grants")
            .fetch_one(&f.state.pool)
            .await
            .unwrap();
    assert_eq!(upstream, "mcp-upstream-2");
    sqlx::query("UPDATE passport_oauth_grants SET last_upstream_check_at=now()-interval '9 hours'")
        .execute(&f.state.pool)
        .await
        .unwrap();
    let expired = f
        .form(
            "/oauth/token",
            None,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", outage["refresh_token"].as_str().unwrap()),
                ("client_id", client_id),
            ],
        )
        .await;
    assert_eq!(expired.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM passport_oauth_grants")
            .fetch_one(&f.state.pool)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn unknown_kid_forces_discovery_and_jwks_over_dialer() {
    let (f, dialer, _listener) = fixture().await;
    let (_, cookie, state) = f.start("/auth/login").await;
    let nonce = f.nonce().await;
    let mut parts: Vec<String> = jwt(&f.issuer, &nonce, "userid:123")
        .split('.')
        .map(str::to_owned)
        .collect();
    parts[0] = URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&json!({"alg":"RS256","kid":"unknown"})).unwrap());
    let code = f.token_mock(parts.join("."), "refresh-1").await;
    assert_eq!(
        f.callback(&cookie, &state).await.status(),
        StatusCode::UNAUTHORIZED
    );
    code.assert_calls_async(1).await;
    assert_eq!(dialer.calls().len(), 5);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM passport_sessions")
            .fetch_one(&f.state.pool)
            .await
            .unwrap(),
        0
    );
    f.close().await;
}

#[tokio::test]
async fn failed_session_refresh_retries_then_obeys_original_hard_cap() {
    let (f, dialer, _listener) = fixture().await;
    let (_, old_cookie, state) = f.start("/auth/login").await;
    let nonce = f.nonce().await;
    f.token_mock(jwt(&f.issuer, &nonce, "userid:123"), "refresh-1")
        .await;
    let cookie = cookie(&f.callback(&old_cookie, &state).await);
    sqlx::query("UPDATE passport_sessions SET next_refresh_at=now()-interval '1 second'")
        .execute(&f.state.pool)
        .await
        .unwrap();
    dialer.fail();
    assert_eq!(
        f.request("GET", "/private", Some(&cookie), None, None)
            .await
            .status(),
        StatusCode::OK
    );
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT next_refresh_at > now() FROM passport_sessions")
            .fetch_one(&f.state.pool)
            .await
            .unwrap()
    );
    let calls = dialer.calls().len();
    assert_eq!(
        f.request("GET", "/private", Some(&cookie), None, None)
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(dialer.calls().len(), calls);
    sqlx::query("UPDATE passport_sessions SET authenticated_at=now()-interval '9 hours'")
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(
        f.request("GET", "/private", Some(&cookie), None, None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    f.close().await;
}
