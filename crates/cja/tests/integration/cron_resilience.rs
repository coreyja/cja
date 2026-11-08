use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use cja::cron::{CronRegistry, Worker};
use tokio_util::sync::CancellationToken;

/// A callback error must be logged and absorbed by `Worker::tick`: the worker
/// keeps running and sibling crons keep firing on their intervals. On the old
/// propagating behaviour the first failing fire made `Worker::run` return
/// `Err`, which a Supervisor turned into process death — a crash loop at cron
/// cadence, since the failing cron's claim commits before its callback runs.
#[sqlx::test]
async fn callback_error_does_not_end_the_worker_or_stop_sibling_crons(pool: sqlx::PgPool) {
    let state = crate::common::app::TestAppState::new(pool.clone());
    let failing_calls = Arc::new(AtomicUsize::new(0));
    let healthy_calls = Arc::new(AtomicUsize::new(0));
    let mut registry = CronRegistry::new();
    let failing_seen = failing_calls.clone();
    registry.register("failing", None, Duration::from_millis(1), move |_, _| {
        let failing_seen = failing_seen.clone();
        Box::pin(async move {
            failing_seen.fetch_add(1, Ordering::SeqCst);
            Err::<(), std::io::Error>(std::io::Error::other("boom"))
        })
    });
    let healthy_seen = healthy_calls.clone();
    registry.register("healthy", None, Duration::from_millis(1), move |_, _| {
        let healthy_seen = healthy_seen.clone();
        Box::pin(async move {
            healthy_seen.fetch_add(1, Ordering::SeqCst);
            Ok::<(), std::io::Error>(())
        })
    });
    let shutdown = CancellationToken::new();
    let worker_task = tokio::spawn(
        Worker::new_with_timezone(state, registry, chrono_tz::UTC, Duration::from_millis(25))
            .run(shutdown.clone()),
    );

    // Fixture control: the failing cron fires within one tick regardless of
    // HashMap iteration order. This passes on the old behaviour too.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while failing_calls.load(Ordering::SeqCst) < 1 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "failing cron never fired; fixture control failed"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    // Regression gate: on the old behaviour the worker task finished with
    // `Err` after the first failing fire, so the sibling stalls at 0-1 calls.
    while healthy_calls.load(Ordering::SeqCst) < 3 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "worker stopped running sibling crons after a callback error: healthy fired only {} times",
            healthy_calls.load(Ordering::SeqCst)
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(!worker_task.is_finished(), "worker still running");

    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(2), worker_task)
        .await
        .unwrap()
        .unwrap();
}

/// An unreadable `crons` table skips the tick (logged, never propagated) and
/// the worker recovers on the next poll once the table is readable again. On
/// the old behaviour the first 42P01 from the map read ended the worker.
#[sqlx::test]
async fn unreadable_crons_table_skips_ticks_and_recovers(pool: sqlx::PgPool) {
    sqlx::query("DROP TABLE crons")
        .execute(&pool)
        .await
        .unwrap();
    let state = crate::common::app::TestAppState::new(pool.clone());
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = CronRegistry::new();
    let seen = calls.clone();
    registry.register(
        "recovery_probe",
        None,
        Duration::from_mins(1),
        move |_, _| {
            let seen = seen.clone();
            Box::pin(async move {
                seen.fetch_add(1, Ordering::SeqCst);
                Ok::<(), std::io::Error>(())
            })
        },
    );
    let shutdown = CancellationToken::new();
    let worker_task = tokio::spawn(
        Worker::new_with_timezone(state, registry, chrono_tz::UTC, Duration::from_millis(25))
            .run(shutdown.clone()),
    );
    // Negative assertion: a fixed settle over ~10 poll periods proves no fire
    // happened while the table was gone. It cannot be polled — waiting for a
    // non-event would pass vacuously before the first due fire.
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        !worker_task.is_finished(),
        "worker ended on an unreadable crons table"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "probe must not fire while the crons table is gone"
    );

    // Recreate the table exactly as migration 20240228040146_AddCrons defines
    // it, unique index included — the claim path relies on ON CONFLICT (name).
    sqlx::query(
        "CREATE TABLE crons (cron_id UUID PRIMARY KEY, name TEXT NOT NULL, last_run_at TIMESTAMPTZ NOT NULL, created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ NOT NULL)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("CREATE UNIQUE INDEX idx_crons_name ON crons (name)")
        .execute(&pool)
        .await
        .unwrap();

    // The next tick reads an empty map, the probe is due, and it fires.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while calls.load(Ordering::SeqCst) < 1 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "worker did not recover after the crons table returned"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(2), worker_task)
        .await
        .unwrap()
        .unwrap();
}
