//! The `finished processing request` event carries `user_id` and
//! `auth_type = "passkey"` for passkey-authenticated requests (DEV-2192).
//!
//! This is its own test binary so it can install one global subscriber, as
//! production does (mirroring `crates/cja/tests/request_user_event.rs`): a
//! per-test scoped subscriber races other test threads for the event
//! callsite's cached interest and silently records nothing.

use std::sync::{Arc, Mutex, OnceLock};

use axum::body::Body;
use cja::server::session::SESSION_COOKIE_NAME;
use cja::server::trace::{RequestUserLayer, Tracer};
use cja_passkey::config::HasPasskeyConfig;
use cja_passkey::{CurrentUser, PasskeyConfig};
use http::Request;
use sqlx::Row;
use tower::ServiceExt;
use tracing_subscriber::prelude::*;
use url::Url;
use uuid::Uuid;

// -------- shared DB/state/cookie helpers (local copies from integration.rs) --------

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

fn make_state(pool: sqlx::PgPool) -> TestAppState {
    let url = Url::parse("http://localhost").unwrap();
    let cfg = PasskeyConfig::new("localhost", &url).unwrap();
    TestAppState {
        db_pool: pool,
        cookie_key: cja::server::cookies::CookieKey::generate(),
        passkey_config: cfg,
    }
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

// -------- capture (trimmed copy of request_user_event.rs) --------

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<Vec<(String, String)>>>>);

/// Serializes the per-scenario tests so windowing the shared buffer is safe.
static SERIALIZE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn capture() -> &'static Capture {
    static CAPTURE: OnceLock<Capture> = OnceLock::new();
    CAPTURE.get_or_init(|| {
        let capture = Capture::default();
        tracing::subscriber::set_global_default(
            tracing_subscriber::registry().with(capture.clone()),
        )
        .expect("install one test tracing subscriber");
        capture
    })
}

impl Capture {
    /// A handler-side assertion panic must not poison the capture for the
    /// remaining scenarios.
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Vec<(String, String)>>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn field<'a>(event: &'a [(String, String)], name: &str) -> Option<&'a str> {
    event
        .iter()
        .find(|(f, _)| f == name)
        .map(|(_, v)| v.as_str())
}

#[derive(Default)]
struct EventFields {
    message: Option<String>,
    fields: Vec<(String, String)>,
}

impl tracing::field::Visit for EventFields {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_owned());
        } else {
            self.fields
                .push((field.name().to_owned(), value.to_owned()));
        }
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.fields
            .push((field.name().to_owned(), value.to_string()));
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = Some(format!("{value:?}"));
        } else {
            self.fields
                .push((field.name().to_owned(), format!("{value:?}")));
        }
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visit = EventFields::default();
        event.record(&mut visit);
        let is_target = visit
            .message
            .as_deref()
            .is_some_and(|m| m.trim_matches('"') == "finished processing request");
        if is_target {
            self.lock().push(visit.fields);
        }
    }
}

// -------- tests --------

/// A `/me` router layered exactly as `cja_passkey::server` does:
/// routes -> (RequestUserLayer, TraceLayer) -> CookieManagerLayer -> state.
fn me_router(state: TestAppState) -> axum::Router {
    async fn protected(user: CurrentUser) -> String {
        user.0.username
    }
    let tracer = Tracer;
    axum::Router::new()
        .route("/me", axum::routing::get(protected))
        .layer((
            RequestUserLayer,
            tower_http::trace::TraceLayer::new_for_http()
                .make_span_with(tracer)
                .on_response(tracer),
        ))
        .layer(tower_cookies::CookieManagerLayer::new())
        .with_state(state)
}

#[tokio::test]
async fn passkey_request_event_carries_user_and_auth_type() {
    let _guard = SERIALIZE.lock().await;
    let capture = capture();
    let start = capture.lock().len();

    let (pool, _db_guard) = setup_test_db().await;
    let state = make_state(pool.clone());

    // Plant a user and an authenticated session (direct writes; the
    // ceremony is exercised in integration.rs).
    let user_id = Uuid::new_v4();
    sqlx::query("INSERT INTO passkey_users (user_id, username) VALUES ($1, 'alice')")
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();
    let row = sqlx::query("INSERT INTO sessions DEFAULT VALUES RETURNING session_id")
        .fetch_one(&pool)
        .await
        .unwrap();
    let session_id: Uuid = row.try_get("session_id").unwrap();
    sqlx::query("UPDATE sessions SET user_id = $1 WHERE session_id = $2")
        .bind(user_id)
        .bind(session_id)
        .execute(&pool)
        .await
        .unwrap();

    let cookie = make_session_cookie(session_id, &state.cookie_key);
    let app = me_router(state);
    let response = app
        .oneshot(
            Request::get("/me")
                .header(http::header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);

    // Window the shared buffer and find this test's event by the planted
    // session's user id, not by counting.
    let events = capture.lock();
    let ours = events[start..]
        .iter()
        .find(|ev| field(ev, "user_id") == Some(&user_id.to_string()))
        .unwrap_or_else(|| panic!("no event carries user {user_id}: {events:?}"));
    assert_eq!(
        field(ours, "auth_type"),
        Some(cja::server::trace::AUTH_TYPE_PASSKEY),
        "event: {ours:?}"
    );
    assert_eq!(field(ours, "status"), Some("200"), "event: {ours:?}");
}

#[tokio::test]
async fn anonymous_passkey_request_has_no_user_fields() {
    let _guard = SERIALIZE.lock().await;
    let capture = capture();
    let start = capture.lock().len();

    let (pool, _db_guard) = setup_test_db().await;
    let state = make_state(pool);

    let app = me_router(state);
    // No cookie: the extractor mints an anonymous session and must NOT
    // record a user.
    let response = app
        .oneshot(Request::get("/me").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), 401);

    let events = capture.lock();
    let ours = events[start..]
        .iter()
        .find(|ev| field(ev, "status") == Some("401"))
        .unwrap_or_else(|| panic!("no 401 event: {events:?}"));
    assert!(
        ours.iter().all(|(f, _)| f != "user_id" && f != "auth_type"),
        "anonymous event carries user fields: {ours:?}"
    );
}
