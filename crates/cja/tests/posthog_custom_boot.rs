mod posthog_support;

use posthog_support::{boot, mock, payload};
use std::time::Duration;
use tracing_subscriber::{Layer as _, layer::SubscriberExt as _, util::SubscriberInitExt as _};

const ORG: &str = "11111111-1111-4111-8111-111111111111";
const APP: &str = "22222222-2222-4222-8222-222222222222";
const INFO: &str = "custom-info-marker";
const WARN: &str = "custom-warn-marker";
const SPAN: &str = "custom-span-marker";
const PRODUCT: &str = "custom-product-event";

fn ids() -> (uuid::Uuid, uuid::Uuid) {
    (ORG.parse().unwrap(), APP.parse().unwrap())
}

fn stdout_layer()
-> impl tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync + 'static {
    tracing_subscriber::fmt::layer()
        .json()
        .with_ansi(false)
        .with_writer(std::io::stdout)
        .with_filter(tracing_subscriber::EnvFilter::new("warn"))
}

fn emit() {
    let _span = tracing::info_span!(target: "fixture", "custom_closed_span").entered();
    tracing::info!(target: "fixture", "{INFO}");
    tracing::info!(target: "fixture", "{SPAN}");
    tracing::warn!(target: "fixture", "{WARN}");
    cja::product_event!(name: PRODUCT, user_id: None, message: "custom product marker", properties: {});
    tracing::info!(target: "fixture", event_type = "ops_only", "ops marker");
    tracing::trace!(target: "rig::completions", "rig-wire-marker");
    let _request = tracing::info_span!(target: "fixture", "request").entered();
    tracing::trace!(target: "rig::completions", "rig-request-wire-marker");
    tracing::warn!(target: "posthog", "exporter-target-marker");
}

fn scenario_env<'a>(eyes: &'a str, posthog: &'a str) -> Vec<(&'a str, &'a str)> {
    vec![
        ("EYES_URL", eyes),
        ("EYES_TRANSPORT", "http"),
        ("EYES_TOKEN", "eyes_wrong"),
        ("EYES_ORG_ID", "malformed"),
        ("POSTHOG_HOST", posthog),
        ("POSTHOG_PROJECT_TOKEN", "phc_test"),
        ("POSTHOG_LOG_LEVEL", "trace"),
        ("RUST_LOG", "info,rig=trace,[request]=trace"),
    ]
}

#[test]
fn custom_stack_flushes_all_sinks() {
    if boot::is_child() {
        boot::child_with_setup(
            || {
                let (org, app) = ids();
                cja::setup::TracingConfig::new("fixture")
                    .stdout_layer(stdout_layer())
                    .eyes_ids(org, app)
                    .eyes_token("eyes_explicit")
                    .init()
                    .unwrap()
            },
            &[&emit],
            |handle| async move { handle.shutdown().await.unwrap() },
            |elapsed| {
                assert!(
                    elapsed < Duration::from_secs(1),
                    "healthy flush took {elapsed:?}"
                );
            },
        );
        return;
    }
    boot::block_on(async {
        let (eyes_host, eyes) = mock(Duration::ZERO).await;
        let (posthog_host, posthog) = mock(Duration::ZERO).await;
        let mut child = boot::spawn(
            "custom_stack_flushes_all_sinks",
            &scenario_env(&eyes_host, &posthog_host),
        );
        assert!(boot::eventually(|| child.stdout().contains(boot::BOOTED)).await);
        child.release();
        let exit = child.finish();
        assert!(exit.success, "{}", exit.output());
        assert!(!exit.stdout.contains("capture shutdown budget exceeded"));
        let stdout_json: serde_json::Value = serde_json::from_str(
            exit.stdout
                .lines()
                .find(|line| line.contains(WARN))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(stdout_json["level"], "WARN");
        assert_eq!(stdout_json["fields"]["message"], WARN);
        assert!(!exit.stdout.contains(INFO));
        assert!(
            exit.stdout
                .contains("Logging to STDOUT with app-supplied layer")
        );

        let eyes_requests = eyes.lock().unwrap();
        assert!(!eyes_requests.is_empty(), "Eyes received no requests");
        assert!(eyes_requests.iter().all(|r| {
            r.uri.path() == format!("/api/orgs/{ORG}/apps/{APP}/events")
                && r.headers["authorization"] == "Bearer eyes_explicit"
        }));
        drop(eyes_requests);
        let eyes_body = boot::eyes_payload(&eyes);
        assert!(eyes_body.contains(INFO), "Eyes missed INFO marker");
        assert!(eyes_body.contains(WARN), "Eyes missed WARN marker");
        assert!(eyes_body.contains("custom_closed_span"), "Eyes missed span");

        let logs = payload(&posthog, |path| path == "/i/v1/logs");
        let traces = payload(&posthog, |path| path == "/i/v1/traces");
        let capture = payload(&posthog, |path| path == "/i/v1/analytics/events");
        assert!(logs.contains(INFO), "PostHog missed INFO log");
        assert!(logs.contains("service.name") && logs.contains("fixture"));
        assert!(traces.contains("custom_closed_span"), "PostHog missed span");
        assert!(traces.contains("service.name") && traces.contains("fixture"));
        assert!(capture.contains(PRODUCT), "PostHog missed product event");
        assert!(!capture.contains("ops_only"));
        for forbidden in [
            "rig-wire-marker",
            "rig-request-wire-marker",
            "exporter-target-marker",
        ] {
            assert!(
                !logs.contains(forbidden),
                "filtered marker reached PostHog logs"
            );
            assert!(
                !traces.contains(forbidden),
                "filtered marker reached PostHog traces"
            );
        }
        let requests = posthog.lock().unwrap();
        for request in requests.iter() {
            assert_eq!(request.headers["authorization"], "Bearer phc_test");
            assert!(!request.uri.to_string().contains("phc_test"));
            assert!(!request.body.windows(8).any(|bytes| bytes == b"phc_test"));
        }
    });
}

#[test]
fn app_owned_registry_does_not_install_posthog() {
    if boot::is_child() {
        boot::child_with_setup(
            || {
                let (org, app) = ids();
                let (builder, transport) =
                    eyes_subscriber::EyesSubscriberBuilder::from_env_with_transport(org, app)
                        .unwrap();
                let (eyes, handle) = builder
                    .with_auth_token("eyes_explicit")
                    .build_with_transport(transport);
                tracing_subscriber::registry()
                    .with(stdout_layer())
                    .with(eyes)
                    .with(tracing_subscriber::EnvFilter::new(
                        "info,rig=trace,[request]=trace",
                    ))
                    .try_init()
                    .unwrap();
                handle
            },
            &[&emit],
            |handle| async move { handle.shutdown().await.unwrap() },
            |_| {},
        );
        return;
    }
    boot::block_on(async {
        let (eyes_host, eyes) = mock(Duration::ZERO).await;
        let (posthog_host, posthog) = mock(Duration::ZERO).await;
        let mut child = boot::spawn(
            "app_owned_registry_does_not_install_posthog",
            &scenario_env(&eyes_host, &posthog_host),
        );
        assert!(boot::eventually(|| child.stdout().contains(boot::BOOTED)).await);
        child.release();
        let exit = child.finish();
        assert!(exit.success, "{}", exit.output());
        assert!(exit.stdout.contains(WARN));
        assert!(boot::eyes_payload(&eyes).contains(INFO));
        assert!(boot::eyes_payload(&eyes).contains("custom product marker"));
        assert!(
            posthog.lock().unwrap().is_empty(),
            "app-owned registry reached PostHog"
        );
    });
}

#[test]
fn env_eyes_token_fallback() {
    if boot::is_child() {
        boot::child(
            &[&|| tracing::info!(target: "fixture", "env-token-marker")],
            |_| {},
        );
        return;
    }
    boot::block_on(async {
        let (eyes_host, eyes) = mock(Duration::ZERO).await;
        let mut envs = boot::EYES_ENV.to_vec();
        envs.extend([("EYES_URL", eyes_host.as_str()), ("EYES_TOKEN", "eyes_env")]);
        let mut child = boot::spawn("env_eyes_token_fallback", &envs);
        assert!(boot::eventually(|| child.stdout().contains(boot::BOOTED)).await);
        child.release();
        let exit = child.finish();
        assert!(exit.success, "{}", exit.output());
        let requests = eyes.lock().unwrap();
        assert!(
            requests
                .iter()
                .any(|r| r.headers["authorization"] == "Bearer eyes_env")
        );
    });
}

#[test]
fn invalid_explicit_token_is_redacted() {
    if boot::is_child() {
        boot::block_on(async {
            let (org, app) = ids();
            for token in [" ", "eyes_secret\ninvalid"] {
                let error = cja::setup::TracingConfig::new("fixture")
                    .eyes_ids(org, app)
                    .eyes_token(token)
                    .init()
                    .expect_err("invalid token must fail");
                assert_eq!(error.to_string(), "invalid Eyes token");
            }
            println!("invalid-token-redacted");
        });
        return;
    }
    let exit = boot::spawn("invalid_explicit_token_is_redacted", &[]).finish();
    assert!(exit.success, "{}", exit.output());
    assert!(exit.stdout.contains("invalid-token-redacted"));
    assert!(!exit.stdout.contains("eyes_secret"));
    assert!(!exit.stderr.contains("eyes_secret"));
}
