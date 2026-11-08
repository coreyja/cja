use cja::server::session::{AppSession, CJASession, Session};
use sqlx::{PgPool, Row};
use uuid::Uuid;

#[derive(Clone, Debug)]
struct TestSession {
    inner: CJASession,
    #[allow(dead_code)]
    user_id: Option<i32>,
}

#[async_trait::async_trait]
impl AppSession for TestSession {
    async fn from_db(pool: &PgPool, session_id: Uuid) -> cja::Result<Self> {
        let row = sqlx::query(
            "SELECT session_id, created_at, updated_at FROM sessions WHERE session_id = $1",
        )
        .bind(session_id)
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
        .unwrap();
    assert_eq!(loaded.inner.session_id, session.inner.session_id);
}

#[tokio::test]
async fn test_session_lifecycle() {
    let (pool, _guard) = crate::common::db::setup_test_db().await.unwrap();

    // Create session
    let session = TestSession::create(&pool).await.unwrap();
    let session_id = session.inner.session_id;

    // Load it back
    let loaded = TestSession::from_db(&pool, session_id).await.unwrap();
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
    let updated = TestSession::from_db(&pool, session_id).await.unwrap();
    assert!(updated.inner.updated_at > loaded.inner.updated_at);
}

#[tokio::test]
async fn test_session_not_found() {
    let (pool, _guard) = crate::common::db::setup_test_db().await.unwrap();

    let random_id = Uuid::new_v4();
    let result = TestSession::from_db(&pool, random_id).await;

    assert!(result.is_err());
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

    let state = CookieProbeState {
        db: pool,
        cookie_key: cja::server::cookies::CookieKey::generate(),
    };
    let app = axum::Router::new()
        .route("/probe", axum::routing::get(probe))
        .layer(tower_cookies::CookieManagerLayer::new())
        .with_state(state);

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
    assert!(set_cookie.starts_with("__Host-session_id="), "got: {set_cookie}");
    assert!(set_cookie.contains("HttpOnly"));
    assert!(set_cookie.contains("Secure"));
    assert!(set_cookie.contains("Path=/"));
    assert!(set_cookie.contains("SameSite=Lax"));
}
