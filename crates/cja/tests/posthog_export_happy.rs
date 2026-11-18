mod posthog_support;
use cja::setup::posthog_test_support::{PostHogConfig, PostHogSink, filter_stack};
use std::time::Duration;
use tracing_subscriber::{filter::LevelFilter, prelude::*};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exports() {
    let (host, captures) = posthog_support::mock(Duration::ZERO).await;
    let sink = PostHogSink::build(
        &PostHogConfig::new_for_tests(host, "phc_test", LevelFilter::TRACE, 1.0),
        "fixture-service",
        None,
        None,
        8192,
    )
    .unwrap();
    let (env, cap) = filter_stack("trace").unwrap();
    let sub = tracing_subscriber::registry()
        .with(
            tracing_opentelemetry::layer()
                .with_tracer(sink.tracer())
                .with_filter(sink.filter()),
        )
        .with(sink.log_bridge().with_filter(sink.filter()))
        .with(env)
        .with(cap);
    tracing::subscriber::with_default(sub, || {
        let _span =
            tracing::info_span!(target:"fixture","fixture_span",tenant="shared-field").entered();
        tracing::info!(target:"fixture","happy-record");
        tracing::trace!(target:"fixture","allowed-trace");
        tracing::trace!(target:"rig::completions","denied-wire");
        tracing::trace!(target: "rig::streaming", "denied-stream");
        tracing::warn!(target: "rig_agent::agent::typed", "denied-typed");
        macro_rules! excluded { ($($t:literal),*) => {$(tracing::info!(target:$t,concat!("denied-", $t));)*}; }
        excluded!(
            "opentelemetry_sdk",
            "hyper::proto::h1",
            "reqwest::connect",
            "rustls::client",
            "h2::codec",
            "posthog"
        );
    });
    sink.shutdown_handle().shutdown().await;
    let captures = captures.lock().unwrap();
    for (path, markers) in [
        (
            "/i/v1/logs",
            vec!["happy-record", "allowed-trace", "tenant", "shared-field"],
        ),
        ("/i/v1/traces", vec!["fixture_span", "shared-field"]),
    ] {
        let mut payload = String::new();
        for c in captures.iter().filter(|c| c.uri == path) {
            assert_eq!(c.headers["authorization"], "Bearer phc_test");
            payload.push_str(&String::from_utf8_lossy(&c.body));
        }
        for marker in markers
            .into_iter()
            .chain(["service.name", "fixture-service"])
        {
            assert!(payload.contains(marker), "missing {marker} from {path}");
        }
        assert!(!payload.contains("denied-"));
    }
    assert!(
        captures
            .iter()
            .all(|c| matches!(c.uri.path(), "/i/v1/logs" | "/i/v1/traces"))
    );
}
