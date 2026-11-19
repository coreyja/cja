// Boot scenarios through the real `cja::setup::setup_tracing`. Every scenario
// re-executes this binary as a child (see `posthog_support::boot`), so each
// one gets its own process and its own global subscriber. Nothing here ever
// contacts the real PostHog: every host is loopback.
mod posthog_support;
use posthog_support::{Captures, boot, mock, payload};
use std::time::Duration;
use tower::ServiceExt as _;

#[cfg(target_os = "linux")]
struct TlsMock {
    host: String,
    captures: Captures,
    handshakes: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    cert_file: std::path::PathBuf,
    cert_dir: std::path::PathBuf,
    server: tokio::task::JoinHandle<()>,
    _temp: tempfile::TempDir,
}

#[cfg(target_os = "linux")]
async fn tls_mock(wrong_ca: bool) -> TlsMock {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let make_ca = || {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::DigitalSignature,
        ];
        let cert = params.self_signed(&key).unwrap();
        (params, key, cert)
    };
    let (trusted_params, trusted_key, trusted_cert) = make_ca();
    let (wrong_params, wrong_key, wrong_cert) = make_ca();
    let (signer_params, signer_key, signer_cert) = if wrong_ca {
        (&wrong_params, &wrong_key, &wrong_cert)
    } else {
        (&trusted_params, &trusted_key, &trusted_cert)
    };
    let issuer = rcgen::Issuer::from_params(signer_params, signer_key);
    let leaf_key = rcgen::KeyPair::generate().unwrap();
    let leaf = rcgen::CertificateParams::new(vec!["localhost".into(), "127.0.0.1".into()])
        .unwrap()
        .signed_by(&leaf_key, &issuer)
        .unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![leaf.der().clone(), signer_cert.der().clone()],
        rustls::pki_types::PrivateKeyDer::Pkcs8(leaf_key.serialize_der().into()),
    )
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host = format!("https://{}", listener.local_addr().unwrap());
    let captures = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let router =
        posthog_support::mock_router(captures.clone(), Duration::ZERO, axum::http::StatusCode::OK);
    let handshakes = Arc::new(AtomicUsize::new(0));
    let attempts = handshakes.clone();
    let server = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let acceptor = acceptor.clone();
            let router = router.clone();
            let attempts = attempts.clone();
            connections.spawn(async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                let Ok(stream) = acceptor.accept(stream).await else {
                    return;
                };
                let service = hyper_util::service::TowerToHyperService::new(router);
                let builder = hyper_util::server::conn::auto::Builder::new(
                    hyper_util::rt::TokioExecutor::new(),
                );
                let _ = builder
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    let temp = tempfile::tempdir().unwrap();
    let cert_file = temp.path().join("ca.pem");
    let cert_dir = temp.path().join("empty-certs");
    std::fs::write(&cert_file, trusted_cert.pem()).unwrap();
    std::fs::create_dir(&cert_dir).unwrap();
    TlsMock {
        host,
        captures,
        handshakes,
        cert_file,
        cert_dir,
        server,
        _temp: temp,
    }
}

#[cfg(target_os = "linux")]
#[test]
fn local_tls_accepts_our_ca_and_rejects_a_different_ca() {
    const MARKER: &str = "tls-eyes-marker";
    if boot::is_child() {
        return boot::child(
            &[&|| {
                emit_private_request();
                tracing::info!(target: "fixture", "{MARKER}");
            }],
            |_| {},
        );
    }
    boot::block_on(async {
        for wrong_ca in [false, true] {
            let tls = tls_mock(wrong_ca).await;
            let (eyes_host, eyes) = mock(Duration::ZERO).await;
            let mut envs = boot::EYES_ENV.to_vec();
            envs.extend([
                ("EYES_URL", eyes_host.as_str()),
                ("POSTHOG_HOST", tls.host.as_str()),
                ("POSTHOG_PROJECT_TOKEN", "phc_test"),
                ("SSL_CERT_FILE", tls.cert_file.to_str().unwrap()),
                ("SSL_CERT_DIR", tls.cert_dir.to_str().unwrap()),
            ]);
            let exit =
                boot::spawn("local_tls_accepts_our_ca_and_rejects_a_different_ca", &envs).finish();
            assert!(
                exit.success && exit.stdout.contains(CONFIGURED_BANNER),
                "{}",
                exit.output()
            );
            assert!(
                boot::eyes_payload(&eyes).contains(MARKER),
                "Eyes missing: {}",
                exit.output()
            );
            let attempts = tls.handshakes.load(std::sync::atomic::Ordering::SeqCst);
            assert!(attempts > 0, "TLS handshake was never attempted");
            let captured = tls.captures.lock().unwrap();
            if wrong_ca {
                assert!(captured.is_empty(), "untrusted cert reached HTTP");
            } else {
                for path in ["/i/v1/logs", "/i/v1/traces"] {
                    assert!(
                        captured.iter().any(|c| c.uri.path() == path
                            && c.headers["authorization"] == "Bearer phc_test"),
                        "{path}: {}",
                        exit.output()
                    );
                }
            }
            drop(captured);
            tls.server.abort();
        }
    });
}

fn emit_private_request() {
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
        let app = cja::server::App::new(
            axum::Router::new().route("/callback", axum::routing::get(|| async {
                tracing::info!(target: "fixture", uri = "/event?code=synthetic-code&page=2", http.request.header.authorization = "Bearer synthetic-authorization", "privacy-event");
                "ok"
            }))
        ).into_router();
        let request = axum::http::Request::builder()
            .uri("/callback?code=synthetic-code&page=2")
            .header("host", "example.test")
            .header("user-agent", "synthetic-agent")
            .header("x-forwarded-for", "127.0.0.1")
            .header("x-forwarded-proto", "https")
            .header("x-forwarded-ssl", "on")
            .header("fly-region", "ewr")
            .header("via", "test-proxy")
            .header("referer", "https://example.test/start?token=synthetic-referer&page=2")
            .body(axum::body::Body::empty()).unwrap();
        assert_eq!(app.oneshot(request).await.unwrap().status(), axum::http::StatusCode::OK);
    });
    });
}

#[test]
fn dual_sink_privacy_keeps_eyes_raw_and_redacts_posthog() {
    if boot::is_child() {
        return boot::child(&[&emit_private_request], |_| {});
    }
    boot::block_on(async {
        let (eyes_host, eyes) = mock(Duration::ZERO).await;
        let (posthog_host, posthog) = mock(Duration::ZERO).await;
        let mut envs = boot::EYES_ENV.to_vec();
        envs.extend([
            ("EYES_URL", eyes_host.as_str()),
            ("POSTHOG_PROJECT_TOKEN", "phc_test"),
            ("POSTHOG_HOST", posthog_host.as_str()),
        ]);
        let exit = boot::spawn(
            "dual_sink_privacy_keeps_eyes_raw_and_redacts_posthog",
            &envs,
        )
        .finish();
        assert!(exit.success, "{}", exit.output());
        let eyes_raw = boot::eyes_payload(&eyes);
        assert!(
            eyes_raw.contains("synthetic-code")
                && eyes_raw.contains("synthetic-referer")
                && eyes_raw.contains("synthetic-agent"),
            "Eyes raw span missing: {eyes_raw}"
        );
        assert!(
            eyes_raw.contains("synthetic-authorization"),
            "Eyes raw event missing: {eyes_raw}"
        );
        let captures = posthog.lock().unwrap();
        let logs = captures
            .iter()
            .filter(|c| c.uri.path() == "/i/v1/logs")
            .map(|c| String::from_utf8_lossy(&c.body).into_owned())
            .collect::<String>();
        let traces = captures
            .iter()
            .filter(|c| c.uri.path() == "/i/v1/traces")
            .map(|c| String::from_utf8_lossy(&c.body).into_owned())
            .collect::<String>();
        assert!(
            logs.contains("privacy-event"),
            "privacy event absent from logs"
        );
        assert!(
            traces.contains("privacy-event"),
            "privacy event absent from span events"
        );
        for path in ["/i/v1/traces", "/i/v1/logs"] {
            let bodies = captures
                .iter()
                .filter(|c| c.uri.path() == path)
                .map(|c| String::from_utf8_lossy(&c.body).into_owned())
                .collect::<String>();
            assert!(
                bodies.contains("[REDACTED]")
                    && bodies.contains("synthetic-agent")
                    && bodies.contains("/callback"),
                "{path}: sanitized payload absent"
            );
            for secret in [
                "synthetic-code",
                "synthetic-referer",
                "synthetic-authorization",
            ] {
                assert!(!bodies.contains(secret), "{path}: leaked {secret}");
            }
        }
    });
}

const CONFIGURED_BANNER: &str = "PostHog OTLP layer configured";
const SETUP_FAILED_BANNER: &str = "Skipping PostHog layer: exporter setup failed:";
// The stdout line the OTel SDK's own log-export failure produces.
const SDK_EXPORT_ERROR: &str = "BatchLogProcessor.ExportError";
const SDK_MARKER: &str = "synthetic-sdk-marker";
const AFTER_NOISE_MARKER: &str = "after-sdk-noise-marker";

#[test]
fn boot_child_scrubs_inherited_credentials() {
    if boot::is_child() {
        return boot::child(
            &[&|| tracing::info!(target: "fixture", "scrub-marker")],
            |_| {},
        );
    }
    if std::env::var_os("CJA_SCRUB_INTERMEDIATE").is_some() {
        let exit = boot::spawn("boot_child_scrubs_inherited_credentials", &[]).finish();
        assert!(exit.success, "{}", exit.output());
        assert!(
            exit.stdout.contains("Skipping PostHog layer")
                && !exit.stdout.contains(CONFIGURED_BANNER),
            "{}",
            exit.output()
        );
        return;
    }
    boot::block_on(async {
        let (host, captures) = mock(Duration::ZERO).await;
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "boot_child_scrubs_inherited_credentials",
                "--nocapture",
            ])
            .env("CJA_SCRUB_INTERMEDIATE", "1")
            .env("POSTHOG_PROJECT_TOKEN", "phc_synthetic_inherited")
            .env("POSTHOG_HOST", &host)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            captures.lock().unwrap().is_empty(),
            "inherited token exported telemetry"
        );
        let exit = boot::spawn(
            "boot_child_scrubs_inherited_credentials",
            &[
                ("POSTHOG_PROJECT_TOKEN", "phc_test"),
                ("POSTHOG_HOST", &host),
            ],
        )
        .finish();
        assert!(
            exit.success && exit.stdout.contains(CONFIGURED_BANNER),
            "{}",
            exit.output()
        );
        assert!(
            !captures.lock().unwrap().is_empty(),
            "explicit credentials did not export"
        );
    });
}

#[test]
fn hostile_otel_exporter_settings_skip_posthog_but_keep_eyes() {
    const MARKER: &str = "hostile-env-eyes-marker";
    if boot::is_child() {
        return boot::child(
            &[&|| {
                boot::emit_markers();
                tracing::info!(target: "fixture", "{MARKER}");
            }],
            |_| {},
        );
    }
    boot::block_on(async {
        for key in [
            "OTEL_EXPORTER_OTLP_HEADERS",
            "OTEL_EXPORTER_OTLP_LOGS_HEADERS",
            "OTEL_EXPORTER_OTLP_TRACES_HEADERS",
            "OTEL_EXPORTER_OTLP_COMPRESSION",
            "OTEL_EXPORTER_OTLP_LOGS_COMPRESSION",
            "OTEL_EXPORTER_OTLP_TRACES_COMPRESSION",
        ] {
            let (eyes_host, eyes) = mock(Duration::ZERO).await;
            let (posthog_host, posthog) = mock(Duration::ZERO).await;
            let mut envs = boot::EYES_ENV.to_vec();
            envs.extend([
                ("EYES_URL", eyes_host.as_str()),
                ("POSTHOG_PROJECT_TOKEN", "phc_test"),
                ("POSTHOG_HOST", posthog_host.as_str()),
                (
                    key,
                    if key.ends_with("HEADERS") {
                        "Authorization=Bearer foreign"
                    } else {
                        "gzip"
                    },
                ),
            ]);
            let child = boot::spawn(
                "hostile_otel_exporter_settings_skip_posthog_but_keep_eyes",
                &envs,
            );
            let delivered = boot::eventually(|| boot::eyes_payload(&eyes).contains(MARKER)).await;
            let exit = child.finish();
            assert!(exit.success && delivered, "{key}: {}", exit.output());
            assert!(
                exit.stdout.contains(SETUP_FAILED_BANNER) && exit.stdout.contains(key),
                "{key}: {}",
                exit.output()
            );
            assert!(
                !exit.stdout.contains(CONFIGURED_BANNER),
                "{key}: {}",
                exit.output()
            );
            assert!(
                posthog.lock().unwrap().is_empty(),
                "{key}: hostile setting exported"
            );
        }
        for setting in [
            None,
            Some(("OTEL_EXPORTER_OTLP_HEADERS", "   ")),
            Some(("OTEL_EXPORTER_OTLP_COMPRESSION", "   ")),
        ] {
            let (posthog_host, posthog) = mock(Duration::ZERO).await;
            let mut envs = vec![
                ("POSTHOG_PROJECT_TOKEN", "phc_test"),
                ("POSTHOG_HOST", posthog_host.as_str()),
            ];
            if let Some(setting) = setting {
                envs.push(setting);
            }
            let exit = boot::spawn(
                "hostile_otel_exporter_settings_skip_posthog_but_keep_eyes",
                &envs,
            )
            .finish();
            assert!(
                exit.success && exit.stdout.contains(CONFIGURED_BANNER),
                "{}",
                exit.output()
            );
            let captures = posthog.lock().unwrap();
            for path in ["/i/v1/logs", "/i/v1/traces"] {
                assert!(
                    captures.iter().any(|capture| capture.uri.path() == path
                        && capture.headers["authorization"] == "Bearer phc_test"),
                    "{path}: {}",
                    exit.output()
                );
            }
        }
    });
}

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

// A1: the exporters' HTTP client cannot be built, so the exporters cannot be
// constructed. Boot must still succeed with the sink skipped and Eyes intact.
//
// How the failure is forced: `PostHogHttpClient::new` builds its rustls
// verifier with `rustls-platform-verifier`, which on Linux loads the roots
// through `rustls-native-certs` and returns an error when the resulting root
// store is empty (rustls-platform-verifier 0.7.1
// `src/verification/others.rs`). `rustls-native-certs` honors
// `SSL_CERT_FILE` / `SSL_CERT_DIR`, so pointing them at an empty file and an
// empty directory yields zero roots and a deterministic construction error.
#[cfg(target_os = "linux")]
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
                let _span = tracing::info_span!(target: "fixture", "filter-span").entered();
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
        let traces = payload(&captures, |path| path == "/i/v1/traces");
        assert!(
            traces.contains("filter-span") || traces.contains(CONTROL),
            "filter trace never exported"
        );
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
