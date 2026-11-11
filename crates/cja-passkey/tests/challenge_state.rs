//! `set_challenge_state` rows-affected contract, in its own binary so
//! `integration.rs` stays compilable against main for the behavior-red runs
//! (the `== 0` assertion cannot compile against the old `()`-returning
//! signature).
//!
//! Each test creates an isolated database. Requires `DATABASE_URL` pointing
//! at a `PostgreSQL` instance with permission to create databases.

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

async fn create_anonymous_session(pool: &sqlx::PgPool) -> uuid::Uuid {
    let row = sqlx::query("INSERT INTO sessions DEFAULT VALUES RETURNING session_id")
        .fetch_one(pool)
        .await
        .unwrap();
    sqlx::Row::try_get(&row, "session_id").unwrap()
}

#[tokio::test]
async fn set_challenge_state_reports_missing_row() {
    let (pool, _guard) = setup_test_db().await;

    // A live session: exactly one row affected, and the value lands.
    let live = create_anonymous_session(&pool).await;
    let affected = cja_passkey::session::PasskeySession::set_challenge_state(
        &pool,
        live,
        serde_json::json!({ "type": "registration" }),
    )
    .await
    .unwrap();
    assert_eq!(affected, 1, "live row must report exactly one row affected");

    // A random UUID (row rotated/deleted concurrently): zero rows, not an
    // error — and not a silent success either.
    let ghost = uuid::Uuid::new_v4();
    let affected = cja_passkey::session::PasskeySession::set_challenge_state(
        &pool,
        ghost,
        serde_json::json!({ "type": "registration" }),
    )
    .await
    .unwrap();
    assert_eq!(
        affected, 0,
        "vanished row must report zero rows, not success"
    );
}
