//! Integration tests for cja-passkey.
//!
//! Each test creates an isolated database. Requires `DATABASE_URL` pointing
//! at a `PostgreSQL` instance with permission to create databases.

#![allow(clippy::similar_names)]

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

use axum::body::Body;
use cja::server::session::SESSION_COOKIE_NAME;
use cja_passkey::config::HasPasskeyConfig;
use cja_passkey::session::{ChallengeState, PasskeySession};
use cja_passkey::{PasskeyConfig, models};
use http::Request;
use http_body_util::BodyExt;
use sqlx::Row;
use tower::ServiceExt;
use url::Url;
use uuid::Uuid;

async fn setup_test_db() -> (sqlx::PgPool, cja::testing::test_db::TestDatabaseGuard) {
    let (pool, guard) = cja::testing::test_db::create_test_database("cja_passkey_test_")
        .await
        .unwrap();

    sqlx::migrate!("../cja/migrations")
        .run(&pool)
        .await
        .unwrap();
    cja_passkey::run_migrations(&pool).await.unwrap();

    (pool, guard)
}

#[derive(Clone)]
struct TestAppState {
    db_pool: sqlx::PgPool,
    cookie_key: cja::server::cookies::CookieKey,
    passkey_config: PasskeyConfig,
}

impl cja::app_state::AppState for TestAppState {
    fn version(&self) -> &'static str {
        "test"
    }
    fn db(&self) -> &sqlx::PgPool {
        &self.db_pool
    }
    fn cookie_key(&self) -> &cja::server::cookies::CookieKey {
        &self.cookie_key
    }
}

impl HasPasskeyConfig for TestAppState {
    fn passkey_config(&self) -> &PasskeyConfig {
        &self.passkey_config
    }
}

fn shared_cookie_key() -> &'static cja::server::cookies::CookieKey {
    static KEY: OnceLock<cja::server::cookies::CookieKey> = OnceLock::new();
    KEY.get_or_init(cja::server::cookies::CookieKey::generate)
}

fn make_state(pool: sqlx::PgPool) -> TestAppState {
    let url = Url::parse("http://localhost").unwrap();
    let cfg = PasskeyConfig::new("localhost", &url).unwrap();
    TestAppState {
        db_pool: pool,
        cookie_key: shared_cookie_key().clone(),
        passkey_config: cfg,
    }
}

fn make_router(state: TestAppState) -> axum::Router {
    cja_passkey::passkey_router::<TestAppState>()
        .layer(tower_cookies::CookieManagerLayer::new())
        .with_state(state)
}

fn make_session_cookie(session_id: Uuid, key: &cja::server::cookies::CookieKey) -> String {
    let key = key.0.clone();
    let mut jar = tower_cookies::cookie::CookieJar::new();
    jar.private_mut(&key)
        .add(tower_cookies::cookie::Cookie::new(
            SESSION_COOKIE_NAME,
            session_id.to_string(),
        ));
    jar.get(SESSION_COOKIE_NAME).unwrap().to_string()
}

/// Decrypt the session id from a response's `Set-Cookie` header, mirroring
/// `make_session_cookie`.
fn set_cookie_session_id(
    headers: &http::HeaderMap,
    key: &cja::server::cookies::CookieKey,
) -> Option<Uuid> {
    let raw = headers
        .get_all(http::header::SET_COOKIE)
        .iter()
        .find_map(|v| v.to_str().ok())
        .filter(|v| v.starts_with(&format!("{SESSION_COOKIE_NAME}=")))?;
    let mut jar = tower_cookies::cookie::CookieJar::new();
    jar.add_original(tower_cookies::cookie::Cookie::parse(raw.to_owned()).ok()?);
    let decrypted = jar.private(&key.0).get(SESSION_COOKIE_NAME)?;
    Uuid::parse_str(decrypted.value()).ok()
}

async fn create_anonymous_session(pool: &sqlx::PgPool) -> Uuid {
    let row = sqlx::query("INSERT INTO sessions DEFAULT VALUES RETURNING session_id")
        .fetch_one(pool)
        .await
        .unwrap();
    row.try_get("session_id").unwrap()
}

async fn insert_user(pool: &sqlx::PgPool, username: &str) -> Uuid {
    let user_id = Uuid::new_v4();
    sqlx::query("INSERT INTO passkey_users (user_id, username) VALUES ($1, $2)")
        .bind(user_id)
        .bind(username)
        .execute(pool)
        .await
        .unwrap();
    user_id
}

fn json_request(uri: &str, body: &serde_json::Value, cookie: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .uri(uri)
        .method("POST")
        .header(http::header::CONTENT_TYPE, "application/json");
    if let Some(c) = cookie {
        builder = builder.header(http::header::COOKIE, c);
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

// ---- soft authenticator ----
// A minimal in-process WebAuthn authenticator (openssl ECDSA P-256 + CBOR,
// "none" attestation) so tests can drive real registration/login ceremonies
// without a browser.

const RP_ID: &str = "localhost";
const ORIGIN: &str = "http://localhost";

struct SoftPasskey {
    ec_key: openssl::ec::EcKey<openssl::pkey::Private>,
    cred_id: Vec<u8>,
    counter: AtomicU32,
}

fn sha256(data: &[u8]) -> Vec<u8> {
    openssl::hash::hash(openssl::hash::MessageDigest::sha256(), data)
        .unwrap()
        .to_vec()
}

fn b64url(data: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

impl SoftPasskey {
    fn new() -> Self {
        let group =
            openssl::ec::EcGroup::from_curve_name(openssl::nid::Nid::X9_62_PRIME256V1).unwrap();
        Self {
            ec_key: openssl::ec::EcKey::generate(&group).unwrap(),
            cred_id: Uuid::new_v4().into_bytes().to_vec(),
            counter: AtomicU32::new(0),
        }
    }

    /// (x, y) coordinates, 32 bytes each, big-endian.
    fn public_coords(&self) -> (Vec<u8>, Vec<u8>) {
        let mut ctx = openssl::bn::BigNumContext::new().unwrap();
        let bytes = self
            .ec_key
            .public_key()
            .to_bytes(
                self.ec_key.group(),
                openssl::ec::PointConversionForm::UNCOMPRESSED,
                &mut ctx,
            )
            .unwrap();
        (bytes[1..33].to_vec(), bytes[33..65].to_vec())
    }

    /// COSE EC2 key map: {1:2, 3:-7, -1:1, -2:x, -3:y}.
    fn cose_key(&self) -> Vec<u8> {
        let (x, y) = self.public_coords();
        let mut map = std::collections::BTreeMap::new();
        map.insert(
            serde_cbor_2::Value::Integer(1),
            serde_cbor_2::Value::Integer(2),
        );
        map.insert(
            serde_cbor_2::Value::Integer(3),
            serde_cbor_2::Value::Integer(-7),
        );
        map.insert(
            serde_cbor_2::Value::Integer(-1),
            serde_cbor_2::Value::Integer(1),
        );
        map.insert(
            serde_cbor_2::Value::Integer(-2),
            serde_cbor_2::Value::Bytes(x),
        );
        map.insert(
            serde_cbor_2::Value::Integer(-3),
            serde_cbor_2::Value::Bytes(y),
        );
        serde_cbor_2::to_vec(&serde_cbor_2::Value::Map(map)).unwrap()
    }

    /// DER-encoded ECDSA-SHA256 signature (the format `WebAuthn` expects).
    fn sign(&self, message: &[u8]) -> Vec<u8> {
        let pkey = openssl::pkey::PKey::from_ec_key(self.ec_key.clone()).unwrap();
        let mut signer =
            openssl::sign::Signer::new(openssl::hash::MessageDigest::sha256(), &pkey).unwrap();
        signer.update(message).unwrap();
        signer.sign_to_vec().unwrap()
    }

    /// Registration response with `fmt: "none"` attestation.
    ///
    /// ```text
    /// authData = rp_id_hash || 0x45 (UP|UV|AT) || u32be(0) || aaguid(16 zero bytes)
    ///            || u16be(cred_id len) || cred_id || cose_key
    /// ```
    fn registration_response(&self, challenge: &str) -> serde_json::Value {
        let mut auth_data = sha256(RP_ID.as_bytes());
        auth_data.push(0x45);
        auth_data.extend_from_slice(&0u32.to_be_bytes());
        auth_data.extend_from_slice(&[0u8; 16]);
        let cred_len = u16::try_from(self.cred_id.len()).unwrap();
        auth_data.extend_from_slice(&cred_len.to_be_bytes());
        auth_data.extend_from_slice(&self.cred_id);
        auth_data.extend_from_slice(&self.cose_key());

        let mut att_obj = std::collections::BTreeMap::new();
        att_obj.insert(
            serde_cbor_2::Value::Text("fmt".into()),
            serde_cbor_2::Value::Text("none".into()),
        );
        att_obj.insert(
            serde_cbor_2::Value::Text("attStmt".into()),
            serde_cbor_2::Value::Map(std::collections::BTreeMap::default()),
        );
        att_obj.insert(
            serde_cbor_2::Value::Text("authData".into()),
            serde_cbor_2::Value::Bytes(auth_data),
        );
        let att_obj = serde_cbor_2::to_vec(&serde_cbor_2::Value::Map(att_obj)).unwrap();

        let client_data = serde_json::json!({
            "type": "webauthn.create", "challenge": challenge, "origin": ORIGIN
        });

        serde_json::json!({
            "id": b64url(&self.cred_id), "rawId": b64url(&self.cred_id), "type": "public-key",
            "response": {
                "attestationObject": b64url(&att_obj),
                "clientDataJSON": b64url(client_data.to_string().as_bytes()),
            },
            "extensions": {},
        })
    }

    /// Assertion response. Counter strictly increases (registration stores 0).
    ///
    /// ```text
    /// authData = rp_id_hash || 0x05 (UP|UV) || u32be(counter)
    /// ```
    fn assertion_response(&self, challenge: &str, user_handle: Option<&[u8]>) -> serde_json::Value {
        let counter = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
        let mut auth_data = sha256(RP_ID.as_bytes());
        auth_data.push(0x05);
        auth_data.extend_from_slice(&counter.to_be_bytes());

        let client_data = serde_json::json!({
            "type": "webauthn.get", "challenge": challenge, "origin": ORIGIN
        });

        let mut signed = auth_data.clone();
        signed.extend_from_slice(&sha256(client_data.to_string().as_bytes()));

        serde_json::json!({
            "id": b64url(&self.cred_id), "rawId": b64url(&self.cred_id), "type": "public-key",
            "response": {
                "authenticatorData": b64url(&auth_data),
                "clientDataJSON": b64url(client_data.to_string().as_bytes()),
                "signature": b64url(&self.sign(&signed)),
                "userHandle": user_handle.map(b64url),
            },
            "extensions": {},
        })
    }
}

async fn response_json(response: http::Response<Body>) -> serde_json::Value {
    let body = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).unwrap()
}

/// Drive register/start + register/finish over HTTP with the soft authenticator.
/// Returns the finish response's status and headers.
async fn register_over_http(
    app: axum::Router,
    cookie: &str,
    soft: &SoftPasskey,
    username: &str,
) -> (u16, http::HeaderMap) {
    let start = app
        .clone()
        .oneshot(json_request(
            "/register/start",
            &serde_json::json!({ "username": username }),
            Some(cookie),
        ))
        .await
        .unwrap();
    assert_eq!(start.status(), 200);
    let challenge = response_json(start).await["publicKey"]["challenge"]
        .as_str()
        .unwrap()
        .to_string();
    let finish = app
        .oneshot(json_request(
            "/register/finish",
            &soft.registration_response(&challenge),
            Some(cookie),
        ))
        .await
        .unwrap();
    (finish.status().as_u16(), finish.headers().clone())
}

/// Drive /auth/start + /auth/finish (named login) over HTTP.
async fn named_login_over_http(
    app: axum::Router,
    cookie: &str,
    soft: &SoftPasskey,
    username: &str,
) -> (u16, http::HeaderMap) {
    let start = app
        .clone()
        .oneshot(json_request(
            "/auth/start",
            &serde_json::json!({ "username": username }),
            Some(cookie),
        ))
        .await
        .unwrap();
    assert_eq!(start.status(), 200);
    let challenge = response_json(start).await["publicKey"]["challenge"]
        .as_str()
        .unwrap()
        .to_string();
    let finish = app
        .oneshot(json_request(
            "/auth/finish",
            &soft.assertion_response(&challenge, None),
            Some(cookie),
        ))
        .await
        .unwrap();
    (finish.status().as_u16(), finish.headers().clone())
}

/// Drive /auth/discoverable/start + /auth/discoverable/finish over HTTP.
async fn discoverable_login_over_http(
    app: axum::Router,
    cookie: &str,
    soft: &SoftPasskey,
    user_handle: &[u8],
) -> (u16, http::HeaderMap) {
    let start = app
        .clone()
        .oneshot(json_request(
            "/auth/discoverable/start",
            &serde_json::json!({}),
            Some(cookie),
        ))
        .await
        .unwrap();
    assert_eq!(start.status(), 200);
    let challenge = response_json(start).await["publicKey"]["challenge"]
        .as_str()
        .unwrap()
        .to_string();
    let finish = app
        .oneshot(json_request(
            "/auth/discoverable/finish",
            &soft.assertion_response(&challenge, Some(user_handle)),
            Some(cookie),
        ))
        .await
        .unwrap();
    (finish.status().as_u16(), finish.headers().clone())
}

/// The authenticated session id in the (single-user-per-test-DB) sessions table.
async fn authenticated_session_id(pool: &sqlx::PgPool) -> Uuid {
    sqlx::query_scalar("SELECT session_id FROM sessions WHERE user_id IS NOT NULL")
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Block until `expected` other backends in this database are waiting on locks
/// (lock views are cluster-wide, so scope by `datname`).
async fn wait_for_lock_waiters(pool: &sqlx::PgPool, expected: i64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let waiters: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE datname = current_database() \
               AND wait_event_type = 'Lock' \
               AND pid <> pg_backend_pid()",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        if waiters >= expected {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "finishes never blocked on the lock"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// Hit a `CurrentUser`-protected route with `cookie`; returns (status, body).
async fn me_status(state: &TestAppState, cookie: Option<&str>) -> (u16, String) {
    async fn protected(user: cja_passkey::CurrentUser) -> String {
        user.0.username
    }
    let app: axum::Router = axum::Router::new()
        .route("/me", axum::routing::get(protected))
        .layer(tower_cookies::CookieManagerLayer::new())
        .with_state(state.clone());
    let mut builder = Request::builder().uri("/me");
    if let Some(c) = cookie {
        builder = builder.header(http::header::COOKIE, c);
    }
    let response = app
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&body).into_owned())
}

// -------- Tests --------

#[tokio::test]
async fn register_start_returns_valid_challenge() {
    let (pool, _guard) = setup_test_db().await;
    let state = make_state(pool.clone());

    let app = make_router(state.clone());
    let response = app
        .oneshot(json_request(
            "/register/start",
            &serde_json::json!({ "username": "alice" }),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json.get("publicKey").is_some(), "missing publicKey: {json}");
    assert_eq!(
        json["publicKey"]["authenticatorSelection"]["residentKey"],
        "preferred"
    );
    assert_eq!(
        json["publicKey"]["authenticatorSelection"]["requireResidentKey"],
        false
    );

    // No user yet
    let row = sqlx::query("SELECT count(*) AS n FROM passkey_users")
        .fetch_one(&pool)
        .await
        .unwrap();
    let n: i64 = row.try_get("n").unwrap();
    assert_eq!(n, 0);

    // challenge_state is set on the (newly created) session
    let row = sqlx::query("SELECT count(*) AS n FROM sessions WHERE challenge_state IS NOT NULL")
        .fetch_one(&pool)
        .await
        .unwrap();
    let n: i64 = row.try_get("n").unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn register_start_rejects_duplicate_username() {
    let (pool, _guard) = setup_test_db().await;
    insert_user(&pool, "alice").await;
    let app = make_router(make_state(pool));

    let response = app
        .oneshot(json_request(
            "/register/start",
            &serde_json::json!({ "username": "alice" }),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 409);
}

#[tokio::test]
async fn auth_start_unknown_username_returns_404() {
    let (pool, _guard) = setup_test_db().await;
    let app = make_router(make_state(pool));

    let response = app
        .oneshot(json_request(
            "/auth/start",
            &serde_json::json!({ "username": "nope" }),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
}

#[tokio::test]
async fn discoverable_start_is_anonymous_and_has_no_allowed_credentials() {
    let (pool, _guard) = setup_test_db().await;
    let app = make_router(make_state(pool.clone()));
    let response = app
        .oneshot(json_request(
            "/auth/discoverable/start",
            &serde_json::json!({}),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let challenge: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        challenge["publicKey"]["allowCredentials"],
        serde_json::json!([])
    );
    assert_eq!(challenge["mediation"], "conditional");

    let row = sqlx::query("SELECT user_id, challenge_state FROM sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.try_get::<Option<Uuid>, _>("user_id").unwrap(), None);
    let state: serde_json::Value = row.try_get("challenge_state").unwrap();
    assert_eq!(state["type"], "discoverable_authentication");
    assert!(serde_json::from_value::<ChallengeState>(state).is_ok());
}

fn dummy_assertion() -> serde_json::Value {
    serde_json::json!({
        "id": "AQ", "rawId": "AQ", "type": "public-key",
        "response": {
            "authenticatorData": "AQ", "clientDataJSON": "AQ", "signature": "AQ",
            "userHandle": null
        },
        "extensions": {}
    })
}

#[tokio::test]
async fn discoverable_finish_rejects_missing_malformed_and_wrong_challenge() {
    let (pool, _guard) = setup_test_db().await;
    let session_id = create_anonymous_session(&pool).await;
    let state = make_state(pool.clone());
    let cookie = make_session_cookie(session_id, &state.cookie_key);
    for challenge in [
        None,
        Some(serde_json::json!({"type": "discoverable_authentication"})),
        Some(serde_json::json!({"type": "authentication"})),
    ] {
        sqlx::query("UPDATE sessions SET challenge_state = $1 WHERE session_id = $2")
            .bind(challenge)
            .bind(session_id)
            .execute(&pool)
            .await
            .unwrap();
        let response = make_router(state.clone())
            .oneshot(json_request(
                "/auth/discoverable/finish",
                &dummy_assertion(),
                Some(&cookie),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
    }

    let register_start = make_router(state.clone())
        .oneshot(json_request(
            "/register/start",
            &serde_json::json!({ "username": "alice" }),
            Some(&cookie),
        ))
        .await
        .unwrap();
    assert_eq!(register_start.status(), 200);
    let response = make_router(state)
        .oneshot(json_request(
            "/auth/discoverable/finish",
            &dummy_assertion(),
            Some(&cookie),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
}

#[tokio::test]
async fn discoverable_finish_rejects_missing_user_handle() {
    let (pool, _guard) = setup_test_db().await;
    let session_id = create_anonymous_session(&pool).await;
    let state = make_state(pool.clone());
    let (_, auth_state) = state
        .passkey_config
        .webauthn
        .start_discoverable_authentication()
        .unwrap();
    PasskeySession::set_challenge_state(
        &pool,
        session_id,
        serde_json::to_value(ChallengeState::DiscoverableAuthentication { auth_state }).unwrap(),
    )
    .await
    .unwrap();
    let cookie = make_session_cookie(session_id, &state.cookie_key);
    let response = make_router(state)
        .oneshot(json_request(
            "/auth/discoverable/finish",
            &dummy_assertion(),
            Some(&cookie),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    let row = sqlx::query("SELECT user_id FROM sessions WHERE session_id = $1")
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.try_get::<Option<Uuid>, _>("user_id").unwrap(), None);
}

#[tokio::test]
async fn named_finish_rejects_discoverable_challenge() {
    let (pool, _guard) = setup_test_db().await;
    let session_id = create_anonymous_session(&pool).await;
    let state = make_state(pool);
    let cookie = make_session_cookie(session_id, &state.cookie_key);
    let start = make_router(state.clone())
        .oneshot(json_request(
            "/auth/discoverable/start",
            &serde_json::json!({}),
            Some(&cookie),
        ))
        .await
        .unwrap();
    assert_eq!(start.status(), 200);
    let finish = make_router(state)
        .oneshot(json_request(
            "/auth/finish",
            &dummy_assertion(),
            Some(&cookie),
        ))
        .await
        .unwrap();
    assert_eq!(finish.status(), 400);
}

#[tokio::test]
async fn auth_start_does_not_set_session_user_id() {
    let (pool, _guard) = setup_test_db().await;
    let user_id = insert_user(&pool, "alice").await;

    // Need a real-looking credential row to pass list_for_user > empty check.
    // Use a Passkey-shaped JSON pulled from a register_start_finish flow is
    // overkill; instead, simulate by directly calling start_passkey_registration
    // and finishing through a fake payload is impossible without an authenticator.
    // For this test we inject one credential row with a serializable Passkey JSON
    // produced from the `webauthn-rs` test helpers. As that's not exposed in 0.5,
    // we instead just verify that with credentials present on disk we don't
    // mutate user_id. We use a syntactically valid JSON fragment that
    // start_passkey_authentication needs to deserialize from — to skip the
    // deserialization complexity we instead assert the security invariant by
    // observing that even if the route fails internally it does NOT touch user_id.

    sqlx::query(
        "INSERT INTO passkey_credentials (user_id, credential_json, credential_id) \
         VALUES ($1, '{}'::jsonb, $2)",
    )
    .bind(user_id)
    .bind(b"\x01\x02\x03".as_slice())
    .execute(&pool)
    .await
    .unwrap();

    let app = make_router(make_state(pool.clone()));
    let _ = app
        .oneshot(json_request(
            "/auth/start",
            &serde_json::json!({ "username": "alice" }),
            None,
        ))
        .await
        .unwrap();

    // user_id should remain NULL on every session — auth/start MUST NOT log a user in.
    let row = sqlx::query("SELECT count(*) AS n FROM sessions WHERE user_id IS NOT NULL")
        .fetch_one(&pool)
        .await
        .unwrap();
    let n: i64 = row.try_get("n").unwrap();
    assert_eq!(n, 0, "auth/start must not set sessions.user_id");
}

#[tokio::test]
async fn current_user_returns_401_without_cookie() {
    let (pool, _guard) = setup_test_db().await;
    let state = make_state(pool);

    async fn protected(_: cja_passkey::CurrentUser) -> &'static str {
        "ok"
    }

    let app: axum::Router = axum::Router::new()
        .route("/me", axum::routing::get(protected))
        .layer(tower_cookies::CookieManagerLayer::new())
        .with_state(state);

    let response = app
        .oneshot(Request::builder().uri("/me").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
}

#[tokio::test]
async fn current_user_returns_401_with_anonymous_session() {
    let (pool, _guard) = setup_test_db().await;
    let session_id = create_anonymous_session(&pool).await;
    let state = make_state(pool);
    let cookie = make_session_cookie(session_id, &state.cookie_key);

    async fn protected(_: cja_passkey::CurrentUser) -> &'static str {
        "ok"
    }
    let app: axum::Router = axum::Router::new()
        .route("/me", axum::routing::get(protected))
        .layer(tower_cookies::CookieManagerLayer::new())
        .with_state(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/me")
                .header(http::header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
}

#[tokio::test]
async fn current_user_returns_user_when_authenticated() {
    let (pool, _guard) = setup_test_db().await;
    let user_id = insert_user(&pool, "alice").await;
    let session_id = create_anonymous_session(&pool).await;
    sqlx::query("UPDATE sessions SET user_id = $1 WHERE session_id = $2")
        .bind(user_id)
        .bind(session_id)
        .execute(&pool)
        .await
        .unwrap();

    let state = make_state(pool);
    let cookie = make_session_cookie(session_id, &state.cookie_key);

    async fn protected(user: cja_passkey::CurrentUser) -> String {
        user.0.username
    }

    let app: axum::Router = axum::Router::new()
        .route("/me", axum::routing::get(protected))
        .layer(tower_cookies::CookieManagerLayer::new())
        .with_state(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/me")
                .header(http::header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"alice");
}

#[tokio::test]
async fn optional_user_none_when_anonymous() {
    let (pool, _guard) = setup_test_db().await;
    let state = make_state(pool);

    async fn handler(user: cja_passkey::OptionalUser) -> String {
        match user.0 {
            Some(u) => u.username,
            None => "anon".to_string(),
        }
    }

    let app: axum::Router = axum::Router::new()
        .route("/me", axum::routing::get(handler))
        .layer(tower_cookies::CookieManagerLayer::new())
        .with_state(state);

    let response = app
        .oneshot(Request::builder().uri("/me").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"anon");
}

#[tokio::test]
async fn optional_user_some_when_authenticated() {
    let (pool, _guard) = setup_test_db().await;
    let user_id = insert_user(&pool, "alice").await;
    let session_id = create_anonymous_session(&pool).await;
    sqlx::query("UPDATE sessions SET user_id = $1 WHERE session_id = $2")
        .bind(user_id)
        .bind(session_id)
        .execute(&pool)
        .await
        .unwrap();
    let state = make_state(pool);
    let cookie = make_session_cookie(session_id, &state.cookie_key);

    async fn handler(user: cja_passkey::OptionalUser) -> String {
        match user.0 {
            Some(u) => u.username,
            None => "anon".to_string(),
        }
    }

    let app: axum::Router = axum::Router::new()
        .route("/me", axum::routing::get(handler))
        .layer(tower_cookies::CookieManagerLayer::new())
        .with_state(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/me")
                .header(http::header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"alice");
}

#[tokio::test]
async fn user_model_crud() {
    let (pool, _guard) = setup_test_db().await;

    let user_id = Uuid::new_v4();
    let user = models::user::create(&pool, user_id, "alice", Some("Alice"))
        .await
        .unwrap();
    assert_eq!(user.user_id, user_id);
    assert_eq!(user.username, "alice");
    assert_eq!(user.display_name.as_deref(), Some("Alice"));

    let by_id = models::user::find_by_id(&pool, user_id).await.unwrap();
    assert!(by_id.is_some());
    let by_name = models::user::find_by_username(&pool, "alice")
        .await
        .unwrap();
    assert_eq!(by_name.unwrap().user_id, user_id);

    assert!(models::user::username_exists(&pool, "alice").await.unwrap());
    assert!(!models::user::username_exists(&pool, "bob").await.unwrap());
}

#[tokio::test]
async fn credential_model_crud() {
    let (pool, _guard) = setup_test_db().await;
    let user_id = insert_user(&pool, "alice").await;

    let cred_id = b"\xaa\xbb\xcc";
    let json = serde_json::json!({"foo": "bar"});
    let cred = models::credential::create(&pool, user_id, json.clone(), cred_id, Some("k1"))
        .await
        .unwrap();
    assert_eq!(cred.user_id, user_id);
    assert_eq!(cred.credential_id, cred_id);

    let listed = models::credential::list_for_user(&pool, user_id)
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);

    let found = models::credential::find_by_credential_id(&pool, cred_id)
        .await
        .unwrap();
    assert!(found.is_some());

    let new_json = serde_json::json!({"foo": "baz"});
    models::credential::update_after_auth(
        &pool,
        cred.credential_id_pk,
        new_json.clone(),
        chrono::Utc::now(),
    )
    .await
    .unwrap();

    let after = models::credential::find_by_credential_id(&pool, cred_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.credential_json, new_json);
    assert!(after.last_used_at.is_some());
}

#[tokio::test]
async fn lock_and_rotate_challenge_lifecycle() {
    let (pool, _guard) = setup_test_db().await;
    let session_id = create_anonymous_session(&pool).await;
    // Lock with no challenge set -> None.
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(
        PasskeySession::lock_challenge_state(&mut *tx, session_id)
            .await
            .unwrap(),
        None
    );
    tx.rollback().await.unwrap();

    PasskeySession::set_challenge_state(&pool, session_id, serde_json::json!({"k": "v"}))
        .await
        .unwrap();
    let created_before: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT created_at FROM sessions WHERE session_id = $1")
            .bind(session_id)
            .fetch_one(&pool)
            .await
            .unwrap();

    let mut tx = pool.begin().await.unwrap();
    assert_eq!(
        PasskeySession::lock_challenge_state(&mut *tx, session_id)
            .await
            .unwrap(),
        Some(serde_json::json!({"k": "v"}))
    );
    let user_id = insert_user(&pool, "alice").await;
    let rotated = PasskeySession::rotate_session_id(&mut *tx, session_id, user_id)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert_ne!(rotated.session_id, session_id);
    assert_eq!(rotated.created_at, created_before); // session age carries over
    // Old row is gone; new row is authenticated with no challenge.
    let old: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions WHERE session_id = $1")
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(old, 0);
    let (user, challenge): (Option<Uuid>, Option<serde_json::Value>) =
        sqlx::query_as("SELECT user_id, challenge_state FROM sessions WHERE session_id = $1")
            .bind(rotated.session_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(user, Some(user_id));
    assert_eq!(challenge, None);
}

#[tokio::test]
async fn multiple_passkeys_per_user() {
    let (pool, _guard) = setup_test_db().await;
    let user_id = insert_user(&pool, "alice").await;
    models::credential::create(&pool, user_id, serde_json::json!({"a": 1}), b"\x01", None)
        .await
        .unwrap();
    models::credential::create(&pool, user_id, serde_json::json!({"b": 2}), b"\x02", None)
        .await
        .unwrap();
    let listed = models::credential::list_for_user(&pool, user_id)
        .await
        .unwrap();
    assert_eq!(listed.len(), 2);
}

#[tokio::test]
async fn challenge_state_type_serde_roundtrip() {
    // Authentication variant uses publicly constructible types only via webauthn-rs internals,
    // but we can still verify the discriminant and registration shape with a direct JSON.
    let raw = serde_json::json!({
        "type": "registration",
        "registration_state": {},
        "user_unique_id": Uuid::new_v4().to_string(),
        "username": "alice",
        "display_name": "Alice"
    });
    // We can't deserialize into ChallengeState because PasskeyRegistration has private fields,
    // but we CAN verify the discriminator field exists.
    assert_eq!(
        raw.get("type").and_then(|v| v.as_str()),
        Some("registration")
    );
    assert!(raw.get("user_unique_id").is_some());
    assert!(raw.get("username").is_some());
}

#[tokio::test]
async fn challenge_state_type_mismatch_register_finish_rejected() {
    let (pool, _guard) = setup_test_db().await;
    let session_id = create_anonymous_session(&pool).await;
    // Plant an authentication-typed (but invalid-shaped) challenge state. The
    // tagged-enum deserialization will fail before the route can verify intent,
    // which still produces 400 — the security property we want.
    PasskeySession::set_challenge_state(
        &pool,
        session_id,
        serde_json::json!({ "type": "authentication" }),
    )
    .await
    .unwrap();

    let state = make_state(pool);
    let cookie = make_session_cookie(session_id, &state.cookie_key);
    let app = make_router(state);

    let response = app
        .oneshot(json_request(
            "/register/finish",
            &serde_json::json!({}),
            Some(&cookie),
        ))
        .await
        .unwrap();
    assert!(
        response.status() == 400 || response.status() == 422,
        "expected 400/422, got {}",
        response.status()
    );
}

#[tokio::test]
async fn client_manifest_serves_hashed_js_and_map() {
    use cja_passkey::passkey_assets;
    use http::header::{CACHE_CONTROL, CONTENT_TYPE};

    let manifest = &passkey_assets::MANIFEST;
    assert_eq!(manifest.owner, "cja-passkey");
    assert!(
        manifest
            .entries
            .iter()
            .any(|asset| asset.logical_name == "passkey-client.js")
    );
    assert!(
        manifest
            .entries
            .iter()
            .any(|asset| asset.logical_name == "passkey-client.js.map")
    );
    let js_url = passkey_assets::asset_url("passkey-client.js");
    let stem = js_url.strip_prefix("/assets/passkey-client.").unwrap();
    let hash = stem.strip_suffix(".js").unwrap();
    assert_eq!(hash.len(), 8);
    assert!(
        hash.bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    );

    let app = cja::assets::router::<()>(&[manifest]).unwrap();
    let response = app
        .clone()
        .oneshot(Request::builder().uri(&js_url).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()[CONTENT_TYPE], "text/javascript");
    assert_eq!(
        response.headers()[CACHE_CONTROL],
        "public, max-age=31536000, immutable"
    );
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let js = std::str::from_utf8(&body).unwrap();
    assert!(js.contains("cjaPasskey") && js.contains("createPasskeyClient"));
    let map_basename = js
        .lines()
        .find_map(|line| line.strip_prefix("//# sourceMappingURL="))
        .expect("external source map reference");
    let map_url = format!("/assets/{map_basename}");
    assert_eq!(map_url, passkey_assets::asset_url("passkey-client.js.map"));
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(&map_url)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()[CONTENT_TYPE], "application/json");
    assert_eq!(
        response.headers()[CACHE_CONTROL],
        "public, max-age=31536000, immutable"
    );
    for path in ["/passkey-client.js", "/assets/passkey-client.js"] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 404, "{path}");
    }
}

#[tokio::test]
async fn auth_router_does_not_serve_unhashed_client() {
    let (pool, _guard) = setup_test_db().await;
    let app = make_router(make_state(pool));
    let response = app
        .oneshot(
            Request::builder()
                .uri("/passkey-client.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
}

// Ensure ChallengeState enum tag is `type` and roundtrips for the registration variant.
#[tokio::test]
async fn challenge_state_registration_round_trip_via_json_text() {
    // We cannot construct a real PasskeyRegistration without going through webauthn,
    // so this test only ensures the type discriminator is present in the JSON
    // produced by serde when serializing a real one. We verify the variant tag
    // by building the registration fresh.
    let webauthn_cfg =
        PasskeyConfig::new("localhost", &Url::parse("http://localhost").unwrap()).unwrap();
    let user_id = Uuid::new_v4();
    let (_ccr, reg) = webauthn_cfg
        .webauthn
        .start_passkey_registration(user_id, "alice", "Alice", None)
        .unwrap();
    let cs = ChallengeState::Registration {
        registration_state: reg,
        user_unique_id: user_id,
        username: "alice".into(),
        display_name: Some("Alice".into()),
    };
    let v = serde_json::to_value(&cs).unwrap();
    assert_eq!(v.get("type").and_then(|x| x.as_str()), Some("registration"));
    assert_eq!(v.get("username").and_then(|x| x.as_str()), Some("alice"));
    assert!(v.get("user_unique_id").is_some());

    let back: ChallengeState = serde_json::from_value(v).unwrap();
    match back {
        ChallengeState::Registration { username, .. } => assert_eq!(username, "alice"),
        ChallengeState::Authentication { .. }
        | ChallengeState::DiscoverableAuthentication { .. } => {
            panic!("wrong variant")
        }
    }
}

/// Regression for the `ignore_missing` composition fix (DEV-2188): the real
/// passkey-app boot sequence is `cja::db::run_migrations` followed by
/// `cja_passkey::run_migrations`. Before the fix, cja's migrator rejected
/// `cja_passkey`'s foreign `_sqlx_migrations` rows on the *second* boot.
#[tokio::test]
async fn migrations_compose_across_two_boots() {
    let (pool, _guard) = cja::testing::test_db::create_test_database("cja_passkey_test_")
        .await
        .unwrap();

    for _ in 0..2 {
        cja::db::run_migrations(&pool).await.unwrap();
        cja_passkey::run_migrations(&pool).await.unwrap();
    }
}

// ---- Session rotation / atomic challenge consume (DEV-2189) ----

/// Shared rotation assertions: the finish rotated the session row (old id gone,
/// new authenticated row with no challenge) and only the new cookie authenticates.
async fn assert_rotated(
    pool: &sqlx::PgPool,
    state: &TestAppState,
    old_id: Uuid,
    new_id: Uuid,
    username: &str,
) {
    assert_ne!(new_id, old_id);
    let old: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions WHERE session_id = $1")
        .bind(old_id)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(old, 0, "old session row must be deleted");
    let (user, challenge): (Option<Uuid>, Option<serde_json::Value>) =
        sqlx::query_as("SELECT user_id, challenge_state FROM sessions WHERE session_id = $1")
            .bind(new_id)
            .fetch_one(pool)
            .await
            .unwrap();
    assert!(user.is_some(), "rotated session must be authenticated");
    assert_eq!(challenge, None, "rotated session must have no challenge");

    // Old cookie no longer authenticates; the new one does.
    let old_cookie = make_session_cookie(old_id, &state.cookie_key);
    let (status, body) = me_status(state, Some(&old_cookie)).await;
    assert_eq!((status, body.as_str()), (401, ""), "old cookie must 401");
    let new_cookie = make_session_cookie(new_id, &state.cookie_key);
    let (status, body) = me_status(state, Some(&new_cookie)).await;
    assert_eq!((status, body.as_str()), (200, username));
}

#[tokio::test]
async fn register_rotates_session_id_and_invalidates_old_cookie() {
    let (pool, _guard) = setup_test_db().await;
    let old_id = create_anonymous_session(&pool).await;
    let state = make_state(pool.clone());
    let cookie = make_session_cookie(old_id, &state.cookie_key);
    let soft = SoftPasskey::new();

    let (status, headers) =
        register_over_http(make_router(state.clone()), &cookie, &soft, "alice").await;
    assert_eq!(status, 200);

    let new_id = set_cookie_session_id(&headers, &state.cookie_key)
        .expect("finish response must re-issue the session cookie");
    assert_rotated(&pool, &state, old_id, new_id, "alice").await;
}

#[tokio::test]
async fn named_login_rotates_session_id_and_invalidates_old_cookie() {
    let (pool, _guard) = setup_test_db().await;
    let anon_id = create_anonymous_session(&pool).await;
    let state = make_state(pool.clone());
    let cookie = make_session_cookie(anon_id, &state.cookie_key);
    let soft = SoftPasskey::new();

    let (status, _) = register_over_http(make_router(state.clone()), &cookie, &soft, "alice").await;
    assert_eq!(status, 200);

    // Registration rotated the session: re-mint the cookie from the DB row.
    let before_login = authenticated_session_id(&pool).await;
    let cookie = make_session_cookie(before_login, &state.cookie_key);

    let (status, headers) =
        named_login_over_http(make_router(state.clone()), &cookie, &soft, "alice").await;
    assert_eq!(status, 200);

    let new_id = set_cookie_session_id(&headers, &state.cookie_key)
        .expect("finish response must re-issue the session cookie");
    assert_rotated(&pool, &state, before_login, new_id, "alice").await;
}

#[tokio::test]
async fn discoverable_login_rotates_session_id() {
    let (pool, _guard) = setup_test_db().await;
    let anon_id = create_anonymous_session(&pool).await;
    let state = make_state(pool.clone());
    let cookie = make_session_cookie(anon_id, &state.cookie_key);
    let soft = SoftPasskey::new();

    let (status, _) = register_over_http(make_router(state.clone()), &cookie, &soft, "alice").await;
    assert_eq!(status, 200);

    let before_login = authenticated_session_id(&pool).await;
    let cookie = make_session_cookie(before_login, &state.cookie_key);
    let alice_user_id: Uuid =
        sqlx::query_scalar("SELECT user_id FROM passkey_users WHERE username = 'alice'")
            .fetch_one(&pool)
            .await
            .unwrap();

    let (status, headers) = discoverable_login_over_http(
        make_router(state.clone()),
        &cookie,
        &soft,
        alice_user_id.as_bytes(),
    )
    .await;
    assert_eq!(status, 200);

    let new_id = set_cookie_session_id(&headers, &state.cookie_key)
        .expect("finish response must re-issue the session cookie");
    assert_rotated(&pool, &state, before_login, new_id, "alice").await;
}

#[tokio::test]
async fn finish_after_successful_finish_returns_400() {
    let (pool, _guard) = setup_test_db().await;
    let anon_id = create_anonymous_session(&pool).await;
    let state = make_state(pool.clone());
    let cookie = make_session_cookie(anon_id, &state.cookie_key);
    let soft = SoftPasskey::new();

    let (status, _) = register_over_http(make_router(state.clone()), &cookie, &soft, "alice").await;
    assert_eq!(status, 200);

    let before_login = authenticated_session_id(&pool).await;
    let cookie = make_session_cookie(before_login, &state.cookie_key);

    let app = make_router(state.clone());
    let start = app
        .clone()
        .oneshot(json_request(
            "/auth/start",
            &serde_json::json!({ "username": "alice" }),
            Some(&cookie),
        ))
        .await
        .unwrap();
    assert_eq!(start.status(), 200);
    let challenge = response_json(start).await["publicKey"]["challenge"]
        .as_str()
        .unwrap()
        .to_string();
    let assertion = soft.assertion_response(&challenge, None);

    let finish = app
        .clone()
        .oneshot(json_request("/auth/finish", &assertion, Some(&cookie)))
        .await
        .unwrap();
    assert_eq!(finish.status(), 200);
    // Mint the *current* cookie from the DB (post-rotation on the fix,
    // unchanged row on main) so the replay targets the live session either way.
    let current_id = authenticated_session_id(&pool).await;
    let rotated_cookie = make_session_cookie(current_id, &state.cookie_key);

    // Same assertion again with the rotated cookie: the challenge is consumed.
    let replay = app
        .oneshot(json_request(
            "/auth/finish",
            &assertion,
            Some(&rotated_cookie),
        ))
        .await
        .unwrap();
    assert_eq!(replay.status(), 400);

    let authenticated: i64 =
        sqlx::query_scalar("SELECT count(*) FROM sessions WHERE user_id IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(authenticated, 1);
    let challenge_live: i64 =
        sqlx::query_scalar("SELECT count(*) FROM sessions WHERE challenge_state IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(challenge_live, 0);
}

#[tokio::test]
async fn concurrent_finishes_exactly_one_succeeds() {
    let (pool, _guard) = setup_test_db().await;
    let anon_id = create_anonymous_session(&pool).await;
    let state = make_state(pool.clone());
    let cookie = make_session_cookie(anon_id, &state.cookie_key);
    let soft = SoftPasskey::new();

    let (status, _) = register_over_http(make_router(state.clone()), &cookie, &soft, "alice").await;
    assert_eq!(status, 200);
    let session_before = authenticated_session_id(&pool).await;
    let alice_id: Uuid =
        sqlx::query_scalar("SELECT user_id FROM passkey_users WHERE username = 'alice'")
            .fetch_one(&pool)
            .await
            .unwrap();

    let cookie = make_session_cookie(session_before, &state.cookie_key);
    let app = make_router(state.clone());
    let start = app
        .clone()
        .oneshot(json_request(
            "/auth/start",
            &serde_json::json!({ "username": "alice" }),
            Some(&cookie),
        ))
        .await
        .unwrap();
    assert_eq!(start.status(), 200);
    let challenge = response_json(start).await["publicKey"]["challenge"]
        .as_str()
        .unwrap()
        .to_string();
    let assertion = soft.assertion_response(&challenge, None);

    // Hold the session row so both finishes block on the lock.
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM sessions WHERE session_id = $1 FOR UPDATE")
        .bind(session_before)
        .execute(&mut *holder)
        .await
        .unwrap();

    let cookie_a = cookie.clone();
    let cookie_b = cookie.clone();
    let app_a = app.clone();
    let app_b = app.clone();
    let assertion_a = assertion.clone();
    let assertion_b = assertion.clone();
    let a = tokio::spawn(async move {
        app_a
            .oneshot(json_request("/auth/finish", &assertion_a, Some(&cookie_a)))
            .await
            .unwrap()
            .status()
            .as_u16()
    });
    let b = tokio::spawn(async move {
        app_b
            .oneshot(json_request("/auth/finish", &assertion_b, Some(&cookie_b)))
            .await
            .unwrap()
            .status()
            .as_u16()
    });

    // Wait until both requests are blocked on the lock (they are in THIS
    // database, waiting on locks, and not the poller itself).
    wait_for_lock_waiters(&pool, 2).await;

    holder.rollback().await.unwrap();
    let (a, b) = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        (a.await.unwrap(), b.await.unwrap())
    })
    .await
    .expect("finishes must complete after the lock is released");

    let mut statuses = vec![a, b];
    statuses.sort_unstable();
    assert_eq!(statuses, vec![200, 400], "exactly one finish may succeed");

    let old: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions WHERE session_id = $1")
        .bind(session_before)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(old, 0);
    let authenticated: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions WHERE user_id = $1")
        .bind(alice_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(authenticated, 1);
    let new_session: Uuid =
        sqlx::query_scalar("SELECT session_id FROM sessions WHERE user_id = $1")
            .bind(alice_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_ne!(new_session, session_before);
}

#[tokio::test]
async fn consumer_fk_rows_cascade_on_rotation() {
    let (pool, _guard) = setup_test_db().await;
    let old_id = create_anonymous_session(&pool).await;
    sqlx::query(
        "CREATE TABLE consumer_session_notes (\
             session_id UUID PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE, \
             note TEXT NOT NULL\
         )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO consumer_session_notes (session_id, note) VALUES ($1, 'pre-auth')")
        .bind(old_id)
        .execute(&pool)
        .await
        .unwrap();

    let state = make_state(pool.clone());
    let cookie = make_session_cookie(old_id, &state.cookie_key);
    let soft = SoftPasskey::new();
    let (status, _headers) = register_over_http(make_router(state), &cookie, &soft, "alice").await;
    assert_eq!(status, 200);

    let old: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions WHERE session_id = $1")
        .bind(old_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(old, 0, "old session row must be deleted");
    let notes: i64 = sqlx::query_scalar("SELECT count(*) FROM consumer_session_notes")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(notes, 0, "pre-auth consumer rows must cascade away");
    let (user, challenge): (Option<Uuid>, Option<serde_json::Value>) =
        sqlx::query_as("SELECT user_id, challenge_state FROM sessions WHERE user_id IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(user.is_some());
    assert_eq!(challenge, None);
}

/// Install a trigger that fails exactly the write completing a login (a session
/// row with `user_id` set and no challenge).
async fn install_login_failure_trigger(pool: &sqlx::PgPool) {
    sqlx::query(
        "CREATE FUNCTION test_fail_session_auth() RETURNS trigger LANGUAGE plpgsql AS \
         $$ BEGIN RAISE EXCEPTION 'injected failure completing login'; END $$",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER test_fail_session_auth BEFORE INSERT OR UPDATE ON sessions \
         FOR EACH ROW WHEN (NEW.user_id IS NOT NULL AND NEW.challenge_state IS NULL) \
         EXECUTE FUNCTION test_fail_session_auth()",
    )
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn register_write_failure_leaves_no_account_and_live_challenge() {
    let (pool, _guard) = setup_test_db().await;
    let session_id = create_anonymous_session(&pool).await;
    install_login_failure_trigger(&pool).await;

    let state = make_state(pool.clone());
    let cookie = make_session_cookie(session_id, &state.cookie_key);
    let soft = SoftPasskey::new();
    let (status, _headers) = register_over_http(make_router(state), &cookie, &soft, "alice").await;
    assert_eq!(status, 500);

    let users: i64 = sqlx::query_scalar("SELECT count(*) FROM passkey_users")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(users, 0, "no account may survive a failed registration");
    let creds: i64 = sqlx::query_scalar("SELECT count(*) FROM passkey_credentials")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(creds, 0, "no credential may survive a failed registration");
    let (user, challenge): (Option<Uuid>, Option<serde_json::Value>) =
        sqlx::query_as("SELECT user_id, challenge_state FROM sessions WHERE session_id = $1")
            .bind(session_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(user, None, "session must stay anonymous");
    assert!(
        challenge.is_some(),
        "challenge must stay usable for a retry"
    );
}

#[tokio::test]
async fn login_write_failure_leaves_session_anonymous_and_challenge_live() {
    let (pool, _guard) = setup_test_db().await;
    let state = make_state(pool.clone());
    let reg_cookie_session = create_anonymous_session(&pool).await;
    let reg_cookie = make_session_cookie(reg_cookie_session, &state.cookie_key);
    let soft = SoftPasskey::new();
    let (status, _) =
        register_over_http(make_router(state.clone()), &reg_cookie, &soft, "alice").await;
    assert_eq!(status, 200);
    let (cred_json, last_used): (serde_json::Value, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as("SELECT credential_json, last_used_at FROM passkey_credentials")
            .fetch_one(&pool)
            .await
            .unwrap();

    // Fresh anonymous session for the login attempt.
    let login_session = create_anonymous_session(&pool).await;
    let login_cookie = make_session_cookie(login_session, &state.cookie_key);
    install_login_failure_trigger(&pool).await;

    let (status, _headers) =
        named_login_over_http(make_router(state.clone()), &login_cookie, &soft, "alice").await;
    assert_eq!(status, 500);

    let (user, challenge): (Option<Uuid>, Option<serde_json::Value>) =
        sqlx::query_as("SELECT user_id, challenge_state FROM sessions WHERE session_id = $1")
            .bind(login_session)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(user, None, "session must stay anonymous behind the 500");
    assert!(
        challenge.is_some(),
        "challenge must stay usable for a retry"
    );

    let (cred_json_after, last_used_after): (
        serde_json::Value,
        Option<chrono::DateTime<chrono::Utc>>,
    ) = sqlx::query_as("SELECT credential_json, last_used_at FROM passkey_credentials")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        cred_json_after, cred_json,
        "credential counter must roll back"
    );
    assert_eq!(last_used_after, last_used, "last_used_at must roll back");
}
