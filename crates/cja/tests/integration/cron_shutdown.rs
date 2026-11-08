use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use cja::cron::{CronRegistry, Worker};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

#[sqlx::test]
async fn shutdown_finishes_the_active_cron_and_records_its_completion(pool: sqlx::PgPool) {
    check_active_cron_shutdown(pool, false).await;
}

#[sqlx::test]
async fn shutdown_finishes_the_active_cron_even_when_it_fails(pool: sqlx::PgPool) {
    check_active_cron_shutdown(pool, true).await;
}

async fn check_active_cron_shutdown(pool: sqlx::PgPool, fail: bool) {
    let state = crate::common::app::TestAppState::new(pool.clone());
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let mut registry = CronRegistry::new();
    let task_started = started.clone();
    let task_release = release.clone();
    registry.register("active_email", None, Duration::from_mins(1), move |_, _| {
        let started = task_started.clone();
        let release = task_release.clone();
        Box::pin(async move {
            started.notify_one();
            release.notified().await;
            if fail {
                Err(std::io::Error::other("SMTP unavailable"))
            } else {
                Ok(())
            }
        })
    });
    let shutdown = CancellationToken::new();
    let mut worker = tokio::spawn(Worker::new(state, registry).run(shutdown.clone()));
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    shutdown.cancel();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut worker)
            .await
            .is_err(),
        "cancellation must wait for the active callback"
    );
    release.notify_one();
    // Whether the callback returned Ok or Err, its error is logged inside
    // `tick` and swallowed: the worker still completes promptly once released
    // and shut down.
    tokio::time::timeout(Duration::from_secs(2), worker)
        .await
        .unwrap()
        .unwrap();
    let recorded: i64 =
        sqlx::query_scalar("SELECT count(*) FROM crons WHERE name = 'active_email'")
            .fetch_one(&pool)
            .await
            .unwrap();
    // The claim commits before the callback starts, including a failing callback.
    assert_eq!(recorded, 1);
}

#[sqlx::test]
async fn already_cancelled_worker_does_not_start_a_tick(pool: sqlx::PgPool) {
    let state = crate::common::app::TestAppState::new(pool.clone());
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = CronRegistry::new();
    let seen = calls.clone();
    registry.register("never_runs", None, Duration::from_millis(1), move |_, _| {
        let seen = seen.clone();
        Box::pin(async move {
            seen.fetch_add(1, Ordering::SeqCst);
            Ok::<(), std::io::Error>(())
        })
    });
    let shutdown = CancellationToken::new();
    shutdown.cancel();
    Worker::new(state, registry).run(shutdown).await;
    // A pre-cancelled worker must not attempt a claim or run any callback.
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let recorded: i64 = sqlx::query_scalar("SELECT count(*) FROM crons")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(recorded, 0);
}
