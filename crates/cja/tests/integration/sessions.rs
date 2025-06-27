use cja::server::session::{AppSession, CJASession};
use sqlx::{PgPool, Row};
use uuid::Uuid;

#[derive(Clone, Debug)]
struct TestSession {
    inner: CJASession,
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
