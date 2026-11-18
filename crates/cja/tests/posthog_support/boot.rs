use super::mock;
use std::{
    process::Command,
    time::{Duration, Instant},
};

pub fn boot(slow: bool) {
    const CHILD: &str = "CJA_POSTHOG_BOOT_CHILD";
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            if std::env::var_os(CHILD).is_some() {
                let handle = cja::setup::setup_tracing("fixture").unwrap();
                let start = Instant::now();
                for i in 0..25 {
                    tracing::info!(target:"fixture",message=%format!("boot-marker-{i:02}"));
                }
                {
                    let _span = tracing::info_span!(target:"fixture","boot-span").entered();
                    tracing::info!(target:"fixture","span-marker");
                }
                assert!(
                    start.elapsed() < Duration::from_secs(1),
                    "emission exceeded one second"
                );
                // Parent closes stdin once it has observed delivery to the running app's sinks.
                let mut release = String::new();
                std::io::stdin().read_line(&mut release).unwrap();
                let start = Instant::now();
                handle.shutdown().await.unwrap();
                assert!(start.elapsed() < Duration::from_secs(15));
                return;
            }
            let (eyes, events) = mock(Duration::ZERO).await;
            let (host, arrivals) = mock(Duration::from_secs(30)).await;
            let mut child = Command::new(std::env::current_exe().unwrap())
                .stdin(std::process::Stdio::piped())
                .env(CHILD, "1")
                .env("EYES_URL", eyes)
                .env("EYES_TRANSPORT", "http")
                .env("EYES_ORG_ID", "11111111-1111-4111-8111-111111111111")
                .env("EYES_APP_ID", "22222222-2222-4222-8222-222222222222")
                .env("POSTHOG_PROJECT_TOKEN", "phc_test")
                .env(
                    "POSTHOG_HOST",
                    if slow { &host } else { "http://127.0.0.1:1" },
                )
                .env("RUST_LOG", "info")
                .spawn()
                .unwrap();
            let start = Instant::now();
            let delivered = loop {
                let payload: String = events
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|c| String::from_utf8_lossy(&c.body).into_owned())
                    .collect();
                if (0..25).all(|i| payload.contains(&format!("boot-marker-{i:02}")))
                    && payload.contains("span-marker")
                    && (!slow
                        || arrivals.lock().unwrap().iter().any(|c| {
                            c.uri.path() == "/i/v1/logs" && c.headers.contains_key("authorization")
                        }))
                {
                    break true;
                }
                if start.elapsed() > Duration::from_secs(10) {
                    break false;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            };
            drop(child.stdin.take());
            assert!(child.wait().unwrap().success());
            assert!(delivered, "missing telemetry within ten seconds");
        });
}
