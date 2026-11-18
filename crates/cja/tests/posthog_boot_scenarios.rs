// Boot scenarios through the real `cja::setup::setup_tracing`. Every scenario
// re-executes this binary as a child (see `posthog_support::boot`), so each
// one gets its own process and its own global subscriber. Nothing here ever
// contacts the real PostHog: every host is loopback.
mod posthog_support;
use posthog_support::{Captures, boot, mock, payload};
use std::time::Duration;

const CONFIGURED_BANNER: &str = "PostHog OTLP layer configured";
const SETUP_FAILED_BANNER: &str = "Skipping PostHog layer: exporter setup failed:";
// The stdout line the OTel SDK's own log-export failure produces.
const SDK_EXPORT_ERROR: &str = "BatchLogProcessor.ExportError";
const SDK_MARKER: &str = "synthetic-sdk-marker";
const AFTER_NOISE_MARKER: &str = "after-sdk-noise-marker";

// An event on an SDK target, as the OTel SDK's `internal-logs` emit them.
fn emit_sdk_event() {
    tracing::info!(target: "opentelemetry_sdk", "{SDK_MARKER}");
}

// The `event_data.target` of everything the Eyes mock received.
fn eyes_targets(eyes: &Captures) -> Vec<String> {
    eyes.lock()
        .unwrap()
        .iter()
        .filter_map(|c| serde_json::from_slice::<serde_json::Value>(&c.body).ok())
        .filter_map(|event| event["event_data"]["target"].as_str().map(str::to_owned))
        .collect()
}

// A1: `reqwest::blocking::Client::build()` fails, so the exporters cannot be
// constructed. Boot must still succeed with the sink skipped and Eyes intact.
//
// How the failure is forced: reqwest 0.13's rustls backend builds its
// verifier with `rustls-platform-verifier`, which on Linux loads the roots
// through `rustls-native-certs` and returns an error when the resulting root
// store is empty (rustls-platform-verifier 0.7.1
// `src/verification/others.rs`). `rustls-native-certs` honors
// `SSL_CERT_FILE` / `SSL_CERT_DIR`, so pointing them at an empty file and an
// empty directory yields zero roots and a deterministic `build()` error.
#[test]
fn exporter_setup_failure_skips_posthog_and_keeps_eyes() {
    const MARKER: &str = "setup-failure-eyes-marker";
    if boot::is_child() {
        return boot::child(&[&|| tracing::info!(target: "fixture", "{MARKER}")], |_| {});
    }
    boot::block_on(async {
        let (eyes, events) = mock(Duration::ZERO).await;
        let certs = tempfile::tempdir().unwrap();
        let cert_file = certs.path().join("empty.pem");
        std::fs::write(&cert_file, "").unwrap();
        let cert_dir = certs.path().join("empty-dir");
        std::fs::create_dir(&cert_dir).unwrap();
        let mut envs = boot::EYES_ENV.to_vec();
        envs.extend([
            ("EYES_URL", eyes.as_str()),
            ("POSTHOG_PROJECT_TOKEN", "phc_test"),
            // Never dialed: construction fails first. Loopback regardless.
            ("POSTHOG_HOST", "https://127.0.0.1:1"),
            ("SSL_CERT_FILE", cert_file.to_str().unwrap()),
            ("SSL_CERT_DIR", cert_dir.to_str().unwrap()),
        ]);
        let child = boot::spawn("exporter_setup_failure_skips_posthog_and_keeps_eyes", &envs);
        let delivered = boot::eventually(|| boot::eyes_payload(&events).contains(MARKER)).await;
        let exit = child.finish();
        assert!(exit.success, "boot must survive: {}", exit.output());
        assert!(exit.stdout.contains(boot::BOOTED), "{}", exit.output());
        assert!(
            exit.stdout.contains(SETUP_FAILED_BANNER),
            "missing the setup-failed skip line: {}",
            exit.output()
        );
        assert!(
            !exit.stdout.contains(CONFIGURED_BANNER),
            "the configured banner must not print for a skipped sink: {}",
            exit.output()
        );
        assert!(delivered, "Eyes never received {MARKER}: {}", exit.output());
    });
}

// A1, second half: a malformed `POSTHOG_*` value with a token set is a
// configuration error and still fails boot (only construction failures are
// isolated).
#[test]
fn malformed_posthog_env_fails_boot() {
    if boot::is_child() {
        return boot::child(&[], |_| {});
    }
    for (key, value) in [
        ("POSTHOG_LOG_LEVEL", "verbose"),
        ("POSTHOG_TRACES_SAMPLE_RATIO", "1.5"),
        ("POSTHOG_HOST", "not a url"),
    ] {
        let exit = boot::spawn(
            "malformed_posthog_env_fails_boot",
            &[("POSTHOG_PROJECT_TOKEN", "phc_test"), (key, value)],
        )
        .finish();
        assert!(
            !exit.success,
            "{key}={value} must fail boot: {}",
            exit.output()
        );
        assert!(
            !exit.stdout.contains(boot::BOOTED),
            "{key}={value}: setup_tracing returned Ok: {}",
            exit.output()
        );
        assert!(
            exit.stderr.contains(&format!("invalid {key}")),
            "{key}={value}: the error must name the variable: {}",
            exit.output()
        );
        assert!(
            !exit.stdout.contains(CONFIGURED_BANNER),
            "{}",
            exit.output()
        );
    }
    // Control: the same value without a token is never read, so boot succeeds.
    let exit = boot::spawn(
        "malformed_posthog_env_fails_boot",
        &[("POSTHOG_LOG_LEVEL", "verbose")],
    )
    .finish();
    assert!(exit.success, "{}", exit.output());
    assert!(
        exit.stdout.contains("Skipping PostHog layer\n"),
        "{}",
        exit.output()
    );
}

// Item 3: the production stack (not a hand-composed one) keeps both global
// filters in front of the PostHog sink. `RUST_LOG=info,rig=trace` admits
// rig's TRACE wire dumps past the `EnvFilter` and `POSTHOG_LOG_LEVEL=trace`
// admits everything past the PostHog filter, so only the rig content cap
// stands between the wire dump and PostHog; the DEBUG event is stopped by
// `RUST_LOG` alone.
#[test]
fn production_stack_filters_posthog_logs() {
    const CONTROL: &str = "filter-control-marker";
    const RIG: &str = "filter-rig-wire-marker";
    const DEBUG: &str = "filter-below-rust-log-marker";
    if boot::is_child() {
        return boot::child(
            &[&|| {
                tracing::trace!(target: "rig::completions", "{RIG}");
                tracing::debug!(target: "fixture", "{DEBUG}");
                tracing::info!(target: "fixture", "{CONTROL}");
            }],
            |_| {},
        );
    }
    boot::block_on(async {
        let (host, captures) = mock(Duration::ZERO).await;
        let child = boot::spawn(
            "production_stack_filters_posthog_logs",
            &[
                ("RUST_LOG", "info,rig=trace"),
                ("POSTHOG_LOG_LEVEL", "trace"),
                ("POSTHOG_PROJECT_TOKEN", "phc_test"),
                ("POSTHOG_HOST", host.as_str()),
            ],
        );
        let logs = || payload(&captures, |path| path == "/i/v1/logs");
        let exported = boot::eventually(|| logs().contains(CONTROL)).await;
        // Shutdown flushes whatever is still queued before the child exits.
        let exit = child.finish();
        assert!(exit.success, "{}", exit.output());
        assert!(exit.stdout.contains(CONFIGURED_BANNER), "{}", exit.output());
        let logs = logs();
        assert!(
            exported && logs.contains(CONTROL),
            "control marker never exported"
        );
        assert!(
            !logs.contains(RIG),
            "a rig::completions TRACE wire dump reached PostHog"
        );
        assert!(
            !logs.contains(DEBUG),
            "an event below RUST_LOG reached PostHog"
        );
        let everything = payload(&captures, |_| true);
        assert!(!everything.contains(RIG) && !everything.contains(DEBUG));
    });
}

// Item 5: while PostHog is enabled and refusing connections, Eyes receives
// every app event and nothing from an `opentelemetry*` target: neither the
// SDK's own export-failure diagnostics nor an event emitted on an SDK target.
#[test]
fn eyes_drops_sdk_targets_while_posthog_enabled() {
    if boot::is_child() {
        return boot::child(
            &[
                &|| {
                    boot::emit_markers();
                    emit_sdk_event();
                },
                &|| tracing::info!(target: "fixture", "{AFTER_NOISE_MARKER}"),
            ],
            |_| {},
        );
    }
    boot::block_on(async {
        let (eyes, events) = mock(Duration::ZERO).await;
        let mut envs = boot::EYES_ENV.to_vec();
        envs.extend([
            ("EYES_URL", eyes.as_str()),
            ("POSTHOG_PROJECT_TOKEN", "phc_test"),
            ("POSTHOG_HOST", "http://127.0.0.1:1"),
        ]);
        let mut child = boot::spawn("eyes_drops_sdk_targets_while_posthog_enabled", &envs);
        // Non-vacuity: wait until the SDK has really logged an export failure
        // (stdout is unfiltered), then have the child emit one more app event.
        // Eyes delivers in order, so once that event arrives, any SDK event
        // the Eyes layer accepted before it has arrived too.
        let noise = boot::eventually(|| child.stdout().contains(SDK_EXPORT_ERROR)).await;
        child.release();
        let delivered = boot::eventually(|| {
            let payload = boot::eyes_payload(&events);
            boot::has_markers(&payload) && payload.contains(AFTER_NOISE_MARKER)
        })
        .await;
        let exit = child.finish();
        assert!(exit.success, "{}", exit.output());
        assert!(
            noise,
            "the SDK never logged {SDK_EXPORT_ERROR}: {}",
            exit.output()
        );
        assert!(
            exit.stdout.contains(SDK_MARKER),
            "stdout keeps SDK targets: {}",
            exit.output()
        );
        assert!(delivered, "Eyes is missing app markers: {}", exit.output());
        let leaked: Vec<String> = eyes_targets(&events)
            .into_iter()
            .filter(|target| target.starts_with("opentelemetry"))
            .collect();
        assert!(
            leaked.is_empty(),
            "Eyes received events from OTel SDK targets while PostHog is enabled: {leaked:?}"
        );
        assert!(!boot::eyes_payload(&events).contains(SDK_MARKER));
    });
}

// Item 5, no-token half: without a token the Eyes layer is unfiltered, so an
// event on an `opentelemetry*` target still reaches Eyes as it did before.
// No SDK runs without a token, so the event is emitted on the SDK's target.
#[test]
fn eyes_keeps_sdk_targets_without_token() {
    if boot::is_child() {
        return boot::child(
            &[&|| {
                boot::emit_markers();
                emit_sdk_event();
            }],
            |_| {},
        );
    }
    boot::block_on(async {
        let (eyes, events) = mock(Duration::ZERO).await;
        let mut envs = boot::EYES_ENV.to_vec();
        envs.push(("EYES_URL", eyes.as_str()));
        let child = boot::spawn("eyes_keeps_sdk_targets_without_token", &envs);
        let delivered = boot::eventually(|| {
            let payload = boot::eyes_payload(&events);
            boot::has_markers(&payload) && payload.contains(SDK_MARKER)
        })
        .await;
        let exit = child.finish();
        assert!(exit.success, "{}", exit.output());
        assert!(
            exit.stdout.contains("Skipping PostHog layer\n"),
            "{}",
            exit.output()
        );
        assert!(delivered, "Eyes is missing events: {}", exit.output());
        assert!(
            eyes_targets(&events)
                .iter()
                .any(|target| target == "opentelemetry_sdk"),
            "without a token Eyes must still receive opentelemetry* targets"
        );
    });
}
