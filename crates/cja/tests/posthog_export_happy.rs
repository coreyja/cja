mod posthog_support;
use cja::setup::posthog_test_support::{PostHogConfig, PostHogSink, filter_stack};
use opentelemetry_proto::tonic::collector::{
    logs::v1::ExportLogsServiceRequest, trace::v1::ExportTraceServiceRequest,
};
use prost::Message as _;
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
    // The exporter's client carries its own ring config. Building it must not
    // install a process-wide rustls provider over the app's own choice.
    assert!(rustls::crypto::CryptoProvider::get_default().is_none());
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
        let _span = tracing::info_span!(target:"fixture","fixture_span",tenant="shared-field",uri="/callback?code=synthetic-code&page=2",url.query="code=synthetic-code&page=2",user_agent.original="synthetic-agent").entered();
        tracing::info!(target:"fixture","happy-record");
        tracing::info!(target:"fixture",uri="/event?token=synthetic-event",http.request.header.authorization="Bearer synthetic-authorization","private-event");
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
    assert_redacted_payloads(captures.as_slice());
}

fn assert_redacted_payloads(captures: &[posthog_support::Capture]) {
    let traces = captures
        .iter()
        .filter(|c| c.uri.path() == "/i/v1/traces")
        .map(|c| ExportTraceServiceRequest::decode(c.body.as_ref()).unwrap())
        .collect::<Vec<_>>();
    let spans = traces
        .iter()
        .flat_map(|r| &r.resource_spans)
        .flat_map(|r| &r.scope_spans)
        .flat_map(|s| &s.spans)
        .collect::<Vec<_>>();
    let span = spans.iter().find(|s| s.name == "fixture_span").unwrap();
    let find =
        |attrs: &[opentelemetry_proto::tonic::common::v1::KeyValue], key: &str| -> Option<String> {
            attrs.iter().find(|a| a.key == key).and_then(|a| {
                match a.value.as_ref()?.value.as_ref()? {
                    opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(v) => {
                        Some(v.clone())
                    }
                    _ => None,
                }
            })
        };
    assert_eq!(
        find(&span.attributes, "uri").as_deref(),
        Some("/callback?code=[REDACTED]&page=2")
    );
    assert_eq!(
        find(&span.attributes, "url.query").as_deref(),
        Some("code=[REDACTED]&page=2")
    );
    assert_eq!(
        find(&span.attributes, "user_agent.original").as_deref(),
        Some("synthetic-agent")
    );
    let event = span
        .events
        .iter()
        .find(|e| e.name == "private-event")
        .unwrap();
    assert_eq!(
        find(&event.attributes, "uri").as_deref(),
        Some("/event?token=[REDACTED]")
    );
    assert!(find(&event.attributes, "http.request.header.authorization").is_none());
    let logs = captures
        .iter()
        .filter(|c| c.uri.path() == "/i/v1/logs")
        .map(|c| ExportLogsServiceRequest::decode(c.body.as_ref()).unwrap())
        .collect::<Vec<_>>();
    let records = logs
        .iter()
        .flat_map(|r| &r.resource_logs)
        .flat_map(|r| &r.scope_logs)
        .flat_map(|s| &s.log_records)
        .collect::<Vec<_>>();
    let record = records
        .iter()
        .find(|r| r.attributes.iter().any(|a| a.key == "uri" && matches!(a.value.as_ref().and_then(|v| v.value.as_ref()), Some(opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(v)) if v == "/event?token=[REDACTED]")))
        .unwrap();
    assert!(find(&record.attributes, "http.request.header.authorization").is_none());
    assert_eq!(
        find(&record.attributes, "user_agent.original").as_deref(),
        Some("synthetic-agent")
    );
    for c in captures
        .iter()
        .filter(|c| matches!(c.uri.path(), "/i/v1/logs" | "/i/v1/traces"))
    {
        let payload = String::from_utf8_lossy(&c.body);
        for secret in [
            "synthetic-code",
            "synthetic-event",
            "synthetic-authorization",
        ] {
            assert!(
                !payload.contains(secret),
                "{} leaked {secret}",
                c.uri.path()
            );
        }
    }
}
