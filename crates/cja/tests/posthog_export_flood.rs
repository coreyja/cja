//! The loopback server stalls responses for longer than the export timeout. A 64-record
//! queue holds about 1 MiB versus 1.53 GiB for all 100,000 records. Boot tests cover refusal.
use cja::setup::posthog_test_support::{PostHogConfig, PostHogSink};
use std::time::{Duration, Instant};
use tracing_subscriber::{filter::LevelFilter, prelude::*};

#[cfg(target_os = "linux")]
fn memory_kib(field: &str) -> usize {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix(field))
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flood() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host = format!("http://{}", listener.local_addr().unwrap());
    let received = std::sync::Arc::new(tokio::sync::Notify::new());
    let arrival = received.clone();
    let app = axum::Router::new().route(
        "/i/v1/logs",
        axum::routing::post(move || async move {
            arrival.notify_one();
            tokio::time::sleep(Duration::from_secs(30)).await;
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let cfg = PostHogConfig::new_for_tests(host, "synthetic", LevelFilter::INFO, 1.0);
    let sink = PostHogSink::build(&cfg, "app", None, None, 64).unwrap();
    let sub = tracing_subscriber::registry().with(sink.log_bridge().with_filter(sink.filter()));
    let guard = tracing::subscriber::set_default(sub);
    tracing::info!(target:"test_app","warmup");
    #[cfg(target_os = "linux")]
    let baseline = memory_kib("VmRSS:");
    let start = Instant::now();
    for i in 0..100_000 {
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "emit loop blocked"
        );
        tracing::info!(target:"test_app", i, payload = "x".repeat(16*1024));
    }
    assert!(start.elapsed() < Duration::from_secs(30));
    drop(guard);
    tokio::time::timeout(Duration::from_secs(5), received.notified())
        .await
        .unwrap();
    let start = Instant::now();
    sink.shutdown_handle().shutdown().await;
    assert!(start.elapsed() < Duration::from_millis(3500));
    #[cfg(target_os = "linux")]
    assert!(
        memory_kib("VmHWM:").saturating_sub(baseline) < 256 * 1024,
        "queue retained over 256 MiB"
    );
}
