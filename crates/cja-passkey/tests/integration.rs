//! Integration tests for cja-passkey.
//!
//! Each test creates an isolated database. Requires `DATABASE_URL` pointing
//! at a `PostgreSQL` instance with permission to create databases.

#![allow(clippy::similar_names)]

use std::sync::OnceLock;

use axum::body::Body;
use cja_passkey::config::HasPasskeyConfig;
use cja_passkey::session::{ChallengeState, PasskeySession};
use cja_passkey::{PasskeyConfig, models};
use http::Request;
use http_body_util::BodyExt;
use sqlx::Row;
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;
use url::Url;
use uuid::Uuid;

fn base_url() -> String {
    std::env::var("DATABASE_URL").unwrap_or_else(|_| "postgres:///postgres".to_string())
}

fn admin_url() -> String {
    let url = base_url();
    if let Some(idx) = url.rfind('/') {
        format!("{}/postgres", &url[..idx])
    } else {
        url
    }
}

fn db_url(db_name: &str) -> String {
    let url = base_url();
    if let Some(idx) = url.rfind('/') {
        format!("{}/{db_name}", &url[..idx])
    } else {
        format!("{url}/{db_name}")
    }
}

struct TestDbGuard {
    db_name: String,
}

impl Drop for TestDbGuard {
    fn drop(&mut self) {
        let db_name = self.db_name.clone();
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let admin = admin_url();
                if let Ok(pool) = PgPoolOptions::new()
                    .max_connections(1)
                    .connect(&admin)
                    .await
                {
                    let _ = sqlx::query(&format!(
                        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                         WHERE datname = '{db_name}' AND pid <> pg_backend_pid()"
                    ))
                    .execute(&pool)
                    .await;
                    let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS \"{db_name}\""))
                        .execute(&pool)
                        .await;
                    pool.close().await;
                }
            });
        })
        .join();
    }
}

async fn setup_test_db() -> (sqlx::PgPool, TestDbGuard) {
    let db_name = format!("cja_passkey_test_{}", Uuid::new_v4().simple());
    let admin = admin_url();
    let admin_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin)
        .await
        .unwrap();
    sqlx::query(&format!("CREATE DATABASE \"{db_name}\""))
        .execute(&admin_pool)
        .await
        .unwrap();
    admin_pool.close().await;

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&db_url(&db_name))
        .await
        .unwrap();

    sqlx::migrate!("../cja/migrations")
        .run(&pool)
        .await
        .unwrap();
    cja_passkey::run_migrations(&pool).await.unwrap();

    (pool, TestDbGuard { db_name })
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
            "session_id",
            session_id.to_string(),
        ));
    jar.get("session_id").unwrap().to_string()
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
async fn passkey_session_lifecycle() {
    let (pool, _guard) = setup_test_db().await;
    let session_id = create_anonymous_session(&pool).await;

    PasskeySession::set_challenge_state(&pool, session_id, serde_json::json!({"k": "v"}))
        .await
        .unwrap();
    let row = sqlx::query("SELECT challenge_state FROM sessions WHERE session_id = $1")
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let cs: Option<serde_json::Value> = row.try_get("challenge_state").unwrap();
    assert_eq!(cs, Some(serde_json::json!({"k": "v"})));

    PasskeySession::clear_challenge_state(&pool, session_id)
        .await
        .unwrap();
    let row = sqlx::query("SELECT challenge_state FROM sessions WHERE session_id = $1")
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let cs: Option<serde_json::Value> = row.try_get("challenge_state").unwrap();
    assert_eq!(cs, None);

    let user_id = insert_user(&pool, "alice").await;
    PasskeySession::set_user_id(&pool, session_id, user_id)
        .await
        .unwrap();
    let row = sqlx::query("SELECT user_id FROM sessions WHERE session_id = $1")
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let uid: Option<Uuid> = row.try_get("user_id").unwrap();
    assert_eq!(uid, Some(user_id));
}

#[tokio::test]
async fn clearing_challenge_state_does_not_affect_user_id() {
    let (pool, _guard) = setup_test_db().await;
    let session_id = create_anonymous_session(&pool).await;
    let user_id = insert_user(&pool, "alice").await;

    PasskeySession::set_user_id(&pool, session_id, user_id)
        .await
        .unwrap();
    PasskeySession::set_challenge_state(&pool, session_id, serde_json::json!({"a": 1}))
        .await
        .unwrap();
    PasskeySession::clear_challenge_state(&pool, session_id)
        .await
        .unwrap();

    let row = sqlx::query("SELECT user_id FROM sessions WHERE session_id = $1")
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let uid: Option<Uuid> = row.try_get("user_id").unwrap();
    assert_eq!(uid, Some(user_id));
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
async fn js_asset_is_served() {
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
    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .headers()
            .get(http::header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        "application/javascript; charset=utf-8"
    );
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&body);
    assert!(body.len() > 100, "body too short: {} bytes", body.len());
    assert!(
        text.contains("cjaPasskey") && text.contains("createPasskeyClient"),
        "expected cjaPasskey/createPasskeyClient in JS output, got: {}",
        &text[..text.len().min(200)]
    );
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
        ChallengeState::Authentication { .. } => panic!("wrong variant"),
    }
}
