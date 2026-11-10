use cja::server::session::{
    ACTIVITY_REFRESH_MIN_INTERVAL, AppSession, CJASession, SESSION_COOKIE_NAME, Session,
};
use sqlx::{PgPool, Row};
use uuid::Uuid;

/// Mint a private `__Host-session_id` cookie exactly as the server writes
/// it, mirroring cja-passkey tests' `make_session_cookie`.
fn mint_session_cookie(session_id: Uuid, key: &cja::server::cookies::CookieKey) -> String {
    let key = key.0.clone();
    let mut jar = tower_cookies::cookie::CookieJar::new();
    jar.private_mut(&key)
        .add(tower_cookies::cookie::Cookie::new(
            SESSION_COOKIE_NAME,
            session_id.to_string(),
        ));
    jar.get(SESSION_COOKIE_NAME).unwrap().to_string()
}

/// Minimal `AppState` for probe routers: pool + generated cookie key.
#[derive(Clone)]
struct CookieProbeState {
    db: sqlx::PgPool,
    cookie_key: cja::server::cookies::CookieKey,
}
impl cja::app_state::AppState for CookieProbeState {
    fn version(&self) -> &'static str {
        "test"
    }
    fn db(&self) -> &sqlx::PgPool {
        &self.db
    }
    fn cookie_key(&self) -> &cja::server::cookies::CookieKey {
        &self.cookie_key
    }
}

async fn probe(Session(_session): Session<TestSession>) -> &'static str {
    "ok"
}

fn probe_router(state: CookieProbeState) -> axum::Router {
    axum::Router::new()
        .route("/probe", axum::routing::get(probe))
        .layer(tower_cookies::CookieManagerLayer::new())
        .with_state(state)
}

#[derive(Clone, Debug)]
struct TestSession {
    inner: CJASession,
    #[allow(dead_code)]
    user_id: Option<i32>,
}

#[async_trait::async_trait]
impl AppSession for TestSession {
    async fn from_db(pool: &PgPool, session_id: Uuid) -> cja::Result<Option<Self>> {
        let row = sqlx::query(
            "SELECT session_id, created_at, updated_at FROM sessions WHERE session_id = $1",
        )
        .bind(session_id)
        .fetch_optional(pool)
        .await?;

        Ok(row.map(|row| Self {
            inner: CJASession {
                session_id: row.get("session_id"),
                created_at: row.get("created_at"),
                updated_at: row.get("updated_at"),
            },
            user_id: None,
        }))
    }

    async fn create(pool: &PgPool) -> cja::Result<Self> {
        let row = sqlx::query(
            "INSERT INTO sessions DEFAULT VALUES RETURNING session_id, created_at, updated_at",
        )
        .fetch_one(pool)
        .await?;

        Ok(Self {
            inner: CJASession {
                session_id: row.get("session_id"),
                created_at: row.get("created_at"),
                updated_at: row.get("updated_at"),
            },
            user_id: None,
        })
    }

    fn from_inner(inner: CJASession) -> Self {
        Self {
            inner,
            user_id: None,
        }
    }

    fn inner(&self) -> &CJASession {
        &self.inner
    }
}

#[tokio::test]
async fn test_session_creation() {
    let (pool, _guard) = crate::common::db::setup_test_db().await.unwrap();

    let session = TestSession::create(&pool).await.unwrap();
    assert_ne!(session.inner.session_id, Uuid::nil());

    // Verify session was persisted
    let loaded = TestSession::from_db(&pool, session.inner.session_id)
        .await
        .unwrap()
        .expect("session row exists");
    assert_eq!(loaded.inner.session_id, session.inner.session_id);
}

#[tokio::test]
async fn test_session_lifecycle() {
    let (pool, _guard) = crate::common::db::setup_test_db().await.unwrap();

    // Create session
    let session = TestSession::create(&pool).await.unwrap();
    let session_id = session.inner.session_id;

    // Load it back
    let loaded = TestSession::from_db(&pool, session_id)
        .await
        .unwrap()
        .expect("session row exists");
    assert_eq!(loaded.inner.session_id, session_id);
    assert_eq!(loaded.inner.created_at, session.inner.created_at);

    // Update timestamp
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    sqlx::query("UPDATE sessions SET updated_at = NOW() WHERE session_id = $1")
        .bind(session_id)
        .execute(&pool)
        .await
        .unwrap();

    // Load again and verify update
    let updated = TestSession::from_db(&pool, session_id)
        .await
        .unwrap()
        .expect("session row exists");
    assert!(updated.inner.updated_at > loaded.inner.updated_at);
}

#[tokio::test]
async fn test_session_not_found() {
    let (pool, _guard) = crate::common::db::setup_test_db().await.unwrap();

    let random_id = Uuid::new_v4();
    let result = TestSession::from_db(&pool, random_id).await;

    // A missing row is `Ok(None)`, never an `Err`: the extractor creates a
    // fresh session on `None` and 500s only on genuine load failures.
    assert!(result.unwrap().is_none());
}

#[tokio::test]
async fn test_concurrent_session_creation() {
    let (pool, _guard) = crate::common::db::setup_test_db().await.unwrap();

    let mut handles = vec![];

    for _ in 0..10 {
        let pool_clone = pool.clone();
        let handle = tokio::spawn(async move { TestSession::create(&pool_clone).await });
        handles.push(handle);
    }

    let mut session_ids = std::collections::HashSet::new();
    for handle in handles {
        let session = handle.await.unwrap().unwrap();
        session_ids.insert(session.inner.session_id);
    }

    // All sessions should have unique IDs
    assert_eq!(session_ids.len(), 10);
}

#[tokio::test]
async fn session_cookie_is_host_prefixed_lax_and_private() {
    let (pool, _guard) = crate::common::db::setup_test_db().await.unwrap();

    let state = CookieProbeState {
        db: pool,
        cookie_key: cja::server::cookies::CookieKey::generate(),
    };
    let app = probe_router(state.clone());

    use tower::ServiceExt as _;
    let response = app
        .oneshot(
            http::Request::builder()
                .uri("/probe")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let set_cookie = response.headers()[http::header::SET_COOKIE]
        .to_str()
        .unwrap();
    assert!(
        set_cookie.starts_with("__Host-session_id="),
        "got: {set_cookie}"
    );
    assert!(set_cookie.contains("HttpOnly"));
    assert!(set_cookie.contains("Secure"));
    assert!(set_cookie.contains("Path=/"));
    assert!(set_cookie.contains("SameSite=Lax"));

    // The cookie value must actually be private: the raw (ciphertext) value
    // must not parse as a UUID, must decrypt with the state key to a UUID,
    // and that UUID must exist in `sessions`.
    let raw_value = set_cookie
        .strip_prefix("__Host-session_id=")
        .and_then(|rest| rest.split(';').next())
        .expect("cookie value");
    assert!(
        Uuid::parse_str(raw_value).is_err(),
        "cookie value is plaintext: {raw_value}"
    );
    let mut jar = tower_cookies::cookie::CookieJar::new();
    jar.add_original(tower_cookies::cookie::Cookie::parse(set_cookie.to_owned()).unwrap());
    let decrypted = jar
        .private(&state.cookie_key.0)
        .get(SESSION_COOKIE_NAME)
        .expect("cookie must decrypt with the state key");
    let session_id =
        Uuid::parse_str(decrypted.value()).expect("decrypted cookie value must be a UUID");
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions WHERE session_id = $1")
        .bind(session_id)
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(rows, 1, "decrypted session id must exist in sessions");
}

// -------- DEV-2192: from_db error semantics --------

/// A session whose DB load always fails (pool outage / decode error shape).
#[derive(Clone, Debug)]
struct ErroringSession {
    inner: CJASession,
}

#[async_trait::async_trait]
impl AppSession for ErroringSession {
    async fn from_db(_pool: &PgPool, _session_id: Uuid) -> cja::Result<Option<Self>> {
        Err(color_eyre::eyre::eyre!("injected from_db failure"))
    }

    async fn create(pool: &PgPool) -> cja::Result<Self> {
        let row = sqlx::query(
            "INSERT INTO sessions DEFAULT VALUES RETURNING session_id, created_at, updated_at",
        )
        .fetch_one(pool)
        .await?;
        Ok(Self {
            inner: CJASession {
                session_id: row.get("session_id"),
                created_at: row.get("created_at"),
                updated_at: row.get("updated_at"),
            },
        })
    }

    fn from_inner(inner: CJASession) -> Self {
        Self { inner }
    }

    fn inner(&self) -> &CJASession {
        &self.inner
    }
}

#[tokio::test]
async fn from_db_error_returns_500_and_leaves_cookie_unchanged() {
    let (pool, _guard) = crate::common::db::setup_test_db().await.unwrap();

    let state = CookieProbeState {
        db: pool,
        cookie_key: cja::server::cookies::CookieKey::generate(),
    };
    // `CookieManagerLayer` is load-bearing: without it the extractor
    // rejects (empty 500) before the handler runs, which would false-green
    // this test.
    let app = axum::Router::new()
        .route(
            "/probe",
            axum::routing::get(|Session(_session): Session<ErroringSession>| async { "ok" }),
        )
        .layer(tower_cookies::CookieManagerLayer::new())
        .with_state(state.clone());

    // Any well-formed cookie UUID: a real outage fails every session load,
    // so always-Err is the faithful injection.
    let cookie = mint_session_cookie(Uuid::new_v4(), &state.cookie_key);

    use tower::ServiceExt as _;
    let response = app
        .oneshot(
            http::Request::builder()
                .uri("/probe")
                .header(http::header::COOKIE, cookie)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 500);
    assert!(
        !response.headers().contains_key(http::header::SET_COOKIE),
        "a failed load must not mint an anonymous session over the cookie: {:?}",
        response.headers().get(http::header::SET_COOKIE)
    );
}

#[tokio::test]
async fn missing_session_row_creates_new_session() {
    let (pool, _guard) = crate::common::db::setup_test_db().await.unwrap();

    let state = CookieProbeState {
        db: pool.clone(),
        cookie_key: cja::server::cookies::CookieKey::generate(),
    };
    let app = probe_router(state.clone());

    let sent_id = Uuid::new_v4();
    let cookie = mint_session_cookie(sent_id, &state.cookie_key);

    use tower::ServiceExt as _;
    let response = app
        .oneshot(
            http::Request::builder()
                .uri("/probe")
                .header(http::header::COOKIE, cookie)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);

    // Control for from_db_error_returns_500: the Ok(None) path succeeds and
    // mints a NEW session cookie (different id), and that id exists.
    let raw = response
        .headers()
        .get(http::header::SET_COOKIE)
        .expect("Ok(None) must mint a new session cookie")
        .to_str()
        .unwrap();
    let mut jar = tower_cookies::cookie::CookieJar::new();
    jar.add_original(tower_cookies::cookie::Cookie::parse(raw.to_owned()).unwrap());
    let decrypted = jar
        .private(&state.cookie_key.0)
        .get(SESSION_COOKIE_NAME)
        .expect("new cookie must decrypt");
    let new_id = Uuid::parse_str(decrypted.value()).expect("must be a UUID");
    assert_ne!(new_id, sent_id);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions WHERE session_id = $1")
        .bind(new_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 1);
}

// -------- DEV-2192: prune --------

/// Backdate `session_id` by `interval`, disabling the `update_sessions_updated_at`
/// trigger around the UPDATE (the trigger re-stamps every UPDATE).
async fn backdate_session(pool: &PgPool, session_id: Uuid, interval: &str) {
    sqlx::query("ALTER TABLE sessions DISABLE TRIGGER update_sessions_updated_at")
        .execute(pool)
        .await
        .unwrap();
    let result = sqlx::query(&format!(
        "UPDATE sessions SET updated_at = now() - interval '{interval}' WHERE session_id = $1"
    ))
    .bind(session_id)
    .execute(pool)
    .await
    .unwrap();
    assert_eq!(result.rows_affected(), 1, "backdate must hit its row");
    sqlx::query("ALTER TABLE sessions ENABLE TRIGGER update_sessions_updated_at")
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn prune_deletes_only_idle_sessions() {
    let (pool, _guard) = crate::common::db::setup_test_db().await.unwrap();

    let fresh = TestSession::create(&pool).await.unwrap();
    let idle = TestSession::create(&pool).await.unwrap();
    backdate_session(&pool, idle.inner.session_id, "40 days").await;

    let deleted = cja::server::session::prune(&pool, std::time::Duration::from_hours(30 * 24))
        .await
        .unwrap();
    assert_eq!(deleted, 1);

    let idle_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions WHERE session_id = $1")
        .bind(idle.inner.session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(idle_rows, 0, "idle session must be pruned");
    let fresh_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions WHERE session_id = $1")
        .bind(fresh.inner.session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(fresh_rows, 1, "fresh session must survive");
}

#[tokio::test]
async fn prune_rejects_idle_at_or_below_refresh_interval() {
    let (pool, _guard) = crate::common::db::setup_test_db().await.unwrap();

    let at = cja::server::session::prune(&pool, ACTIVITY_REFRESH_MIN_INTERVAL).await;
    assert!(at.is_err(), "prune(idle == interval) must refuse");
    let below = cja::server::session::prune(&pool, std::time::Duration::from_secs(60)).await;
    assert!(below.is_err(), "prune(idle < interval) must refuse");
}

// -------- DEV-2192: throttled activity refresh --------

#[tokio::test]
async fn stale_session_updated_at_is_refreshed_after_request() {
    let (pool, _guard) = crate::common::db::setup_test_db().await.unwrap();

    let session = TestSession::create(&pool).await.unwrap();
    backdate_session(&pool, session.inner.session_id, "2 hours").await;

    let state = CookieProbeState {
        db: pool.clone(),
        cookie_key: cja::server::cookies::CookieKey::generate(),
    };
    let app = probe_router(state.clone());
    let cookie = mint_session_cookie(session.inner.session_id, &state.cookie_key);

    use tower::ServiceExt as _;
    let response = app
        .oneshot(
            http::Request::builder()
                .uri("/probe")
                .header(http::header::COOKIE, cookie)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);

    let updated_at: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT updated_at FROM sessions WHERE session_id = $1")
            .bind(session.inner.session_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        updated_at > chrono::Utc::now() - chrono::Duration::minutes(1),
        "stale session must have been refreshed, got {updated_at}"
    );
}

/// Pass-by-design (mutation evidence): a fresh session's `updated_at` is
/// byte-identical after a request — the throttle means no UPDATE ran at all.
#[tokio::test]
async fn fresh_session_updated_at_is_not_touched_by_request() {
    let (pool, _guard) = crate::common::db::setup_test_db().await.unwrap();

    let session = TestSession::create(&pool).await.unwrap();
    backdate_session(&pool, session.inner.session_id, "10 minutes").await;
    let before: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT updated_at FROM sessions WHERE session_id = $1")
            .bind(session.inner.session_id)
            .fetch_one(&pool)
            .await
            .unwrap();

    let state = CookieProbeState {
        db: pool.clone(),
        cookie_key: cja::server::cookies::CookieKey::generate(),
    };
    let app = probe_router(state.clone());
    let cookie = mint_session_cookie(session.inner.session_id, &state.cookie_key);

    use tower::ServiceExt as _;
    let response = app
        .oneshot(
            http::Request::builder()
                .uri("/probe")
                .header(http::header::COOKIE, cookie)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);

    let after: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT updated_at FROM sessions WHERE session_id = $1")
            .bind(session.inner.session_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        before, after,
        "no UPDATE may run for a session refreshed within the interval"
    );
}

/// Pass-by-design (mutation evidence): a failing activity refresh is logged
/// and swallowed — the request still succeeds.
#[tokio::test]
async fn failed_activity_refresh_does_not_fail_the_request() {
    let (pool, _guard) = crate::common::db::setup_test_db().await.unwrap();

    let session = TestSession::create(&pool).await.unwrap();
    backdate_session(&pool, session.inner.session_id, "2 hours").await;

    sqlx::query(
        "CREATE FUNCTION cja_2192_block_updates() RETURNS trigger AS \
         $$ BEGIN RAISE EXCEPTION 'injected update failure'; END $$ LANGUAGE plpgsql",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER cja_2192_block_updates_trigger \
         BEFORE UPDATE ON sessions FOR EACH ROW \
         EXECUTE FUNCTION cja_2192_block_updates()",
    )
    .execute(&pool)
    .await
    .unwrap();

    let state = CookieProbeState {
        db: pool.clone(),
        cookie_key: cja::server::cookies::CookieKey::generate(),
    };
    let app = probe_router(state.clone());
    let cookie = mint_session_cookie(session.inner.session_id, &state.cookie_key);

    use tower::ServiceExt as _;
    let response = app
        .oneshot(
            http::Request::builder()
                .uri("/probe")
                .header(http::header::COOKIE, cookie)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "failed refresh must not 500");

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions WHERE session_id = $1")
        .bind(session.inner.session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 1, "session row must survive");

    sqlx::query("DROP TRIGGER cja_2192_block_updates_trigger ON sessions")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DROP FUNCTION cja_2192_block_updates")
        .execute(&pool)
        .await
        .unwrap();
}
