mod posthog_support;
use posthog_support::{boot, mock, mock_with};
use std::time::Duration;

#[test]
fn capture_payload() {
    if boot::is_child() {
        boot::child(
            &[&|| {
                let user = uuid::Uuid::parse_str("11111111-1111-4111-8111-111111111111").unwrap();
                cja::product_event!(name: "achievement_earned", user_id: Some(&user), message: "Achievement earned", properties: { first = true, slug = "winner", display_value = %42, debug_value = ?Some(7) });
                tracing::info!(event_type = "game_phase", phase = "ready");
            }],
            |_| {},
        );
        return;
    }
    boot::block_on(async {
        let (host, captures) = mock(Duration::ZERO).await;
        let mut child = boot::spawn(
            "capture_payload",
            &[
                ("POSTHOG_HOST", &host),
                ("POSTHOG_PROJECT_TOKEN", "phc_fixture"),
            ],
        );
        assert!(boot::eventually(|| child.stdout().contains(boot::BOOTED)).await);
        child.release();
        let exit = child.finish();
        assert!(exit.success, "{}", exit.output());
        assert!(exit.stdout.contains("Achievement earned"));
        assert!(
            !exit.stdout.contains("capture rejected events"),
            "{}",
            exit.output()
        );
        assert!(
            !exit.stdout.contains("capture request failed"),
            "{}",
            exit.output()
        );
        assert!(!exit.stdout.contains("capture response unreadable"), "{}", exit.output());
        assert!(exit.stdout.contains("game_phase"));
        let requests = captures.lock().unwrap();
        let request = requests
            .iter()
            .find(|r| r.uri.path() == "/i/v1/analytics/events")
            .expect("capture request");
        assert!(request.uri.query().is_none());
        assert_eq!(request.headers["authorization"], "Bearer phc_fixture");
        assert!(request.headers.contains_key("user-agent"));
        assert!(request.headers.contains_key("posthog-sdk-info"));
        assert!(request.headers.contains_key("posthog-request-id"));
        assert_eq!(request.headers["posthog-attempt"], "1");
        assert!(request.headers.contains_key("posthog-request-timestamp"));
        assert!(
            request.headers["content-type"]
                .to_str()
                .unwrap()
                .starts_with("application/json")
        );
        let raw = String::from_utf8_lossy(&request.body);
        assert!(!raw.contains("phc_fixture"));
        assert!(!raw.contains("game_phase"));
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        let event = &body["batch"][0];
        assert_eq!(event["event"], "achievement_earned");
        assert_eq!(event["distinct_id"], "11111111-1111-4111-8111-111111111111");
        assert!(event.get("options").is_none());
        assert!(body["created_at"].as_str().is_some());
        assert!(event["timestamp"].as_str().is_some());
        assert_eq!(event["properties"]["first"], true);
        assert_eq!(event["properties"]["slug"], "winner");
        assert_eq!(event["properties"]["display_value"], "42");
        assert_eq!(event["properties"]["debug_value"], "Some(7)");
        assert_eq!(event["properties"]["service.name"], "fixture");
        assert!(event["properties"].get("user_id").is_none());
        assert_eq!(
            uuid::Uuid::parse_str(event["uuid"].as_str().unwrap())
                .unwrap()
                .get_version_num(),
            7
        );
        assert_eq!(
            uuid::Uuid::parse_str(event["distinct_id"].as_str().unwrap())
                .unwrap()
                .get_version_num(),
            4
        );
    });
}

#[allow(dead_code)]
fn downstream_modes_compile() {
    struct DisplayOnly;
    impl std::fmt::Display for DisplayOnly {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("display")
        }
    }
    struct DebugOnly;
    impl std::fmt::Debug for DebugOnly {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("debug")
        }
    }
    let id = uuid::Uuid::new_v4();
    cja::product_event!(name: "modes", user_id: Some(&id), message: "Modes", properties: { display_value = %DisplayOnly, debug_value = ?DebugOnly, count = 3 });
    cja::product_event!(name: "empty", user_id: None, message: "Empty", properties: {});
}

#[test]
fn stalled_capture_respects_shutdown_budget() {
    if boot::is_child() {
        boot::child(
            &[&|| {
                let start = std::time::Instant::now();
                for marker in 0..200 {
                    cja::product_event!(name: "prompt_event", user_id: None, message: "Prompt event", properties: { marker = marker });
                }
                assert!(start.elapsed() < Duration::from_millis(150));
            }],
            |_| {},
        );
        return;
    }
    boot::block_on(async {
        let (host, captures) = mock(Duration::from_secs(30)).await;
        let mut child = boot::spawn(
            "stalled_capture_respects_shutdown_budget",
            &[
                ("POSTHOG_HOST", &host),
                ("POSTHOG_PROJECT_TOKEN", "phc_fixture"),
            ],
        );
        assert!(boot::eventually(|| child.stdout().contains(boot::BOOTED)).await);
        child.release();
        let exit = child.finish();
        assert!(exit.success, "{}", exit.output());
        assert!(!captures.lock().unwrap().is_empty());
    });
}

#[test]
fn anonymous_and_ops_isolation() {
    if boot::is_child() {
        boot::child(
            &[&|| {
                cja::product_event!(name: "anonymous_view", user_id: None, message: "Anonymous view", properties: {});
                cja::product_event!(name: "anonymous_view", user_id: None, message: "Anonymous view", properties: {});
                tracing::info!(event_type = "job_enqueued", job_id = 42);
            }],
            |_| {},
        );
        return;
    }
    boot::block_on(async {
        let (host, captures) = mock(Duration::ZERO).await;
        let mut child = boot::spawn(
            "anonymous_and_ops_isolation",
            &[
                ("POSTHOG_HOST", &host),
                ("POSTHOG_PROJECT_TOKEN", "phc_fixture"),
            ],
        );
        assert!(boot::eventually(|| child.stdout().contains(boot::BOOTED)).await);
        child.release();
        let exit = child.finish();
        assert!(exit.success, "{}", exit.output());
        assert!(exit.stdout.contains("job_enqueued"));
        let requests = captures.lock().unwrap();
        let body: serde_json::Value = serde_json::from_slice(
            &requests
                .iter()
                .find(|r| r.uri.path() == "/i/v1/analytics/events")
                .unwrap()
                .body,
        )
        .unwrap();
        let event = &body["batch"][0];
        assert_eq!(body["batch"].as_array().unwrap().len(), 2);
        assert_eq!(event["event"], "anonymous_view");
        assert_eq!(event["options"]["process_person_profile"], false);
        assert!(event["properties"].get("$process_person_profile").is_none());
        assert_eq!(event["properties"]["service.name"], "fixture");
        assert_ne!(event["distinct_id"], event["uuid"]);
        let other = &body["batch"][1];
        for item in [event, other] {
            assert_eq!(uuid::Uuid::parse_str(item["distinct_id"].as_str().unwrap()).unwrap().get_version_num(), 4);
            assert_ne!(item["distinct_id"], item["uuid"]);
        }
        assert_ne!(event["distinct_id"], other["distinct_id"]);
        let line = exit.stdout.lines().find(|line| line.contains("Anonymous view")).unwrap();
        assert!(!line.contains("user_id"), "{line}");
    });
}

#[test]
fn blank_token_disables_capture() {
    if boot::is_child() {
        boot::child(
            &[&|| {
                cja::product_event!(name: "no_capture", user_id: None, message: "No capture", properties: {});
            }],
            |_| {},
        );
        return;
    }
    boot::block_on(async {
        let (host, captures) = mock(Duration::ZERO).await;
        let mut child = boot::spawn(
            "blank_token_disables_capture",
            &[("POSTHOG_HOST", &host), ("POSTHOG_PROJECT_TOKEN", " ")],
        );
        assert!(boot::eventually(|| child.stdout().contains(boot::BOOTED)).await);
        child.release();
        let exit = child.finish();
        assert!(exit.success, "{}", exit.output());
        assert!(exit.stdout.contains("No capture"));
        assert!(captures.lock().unwrap().is_empty());
    });
}

#[test]
fn unreachable_capture_does_not_delay_emission() {
    if boot::is_child() {
        boot::child(
            &[&|| {
                let start = std::time::Instant::now();
                for marker in 0..2000 {
                    cja::product_event!(name: "prompt_event", user_id: None, message: "Prompt event", properties: { marker = marker });
                }
                assert!(start.elapsed() < Duration::from_millis(200));
            }],
            |_| {},
        );
        return;
    }
    let mut child = boot::spawn(
        "unreachable_capture_does_not_delay_emission",
        &[
            ("POSTHOG_HOST", "http://127.0.0.1:1"),
            ("POSTHOG_PROJECT_TOKEN", "phc_fixture"),
        ],
    );
    boot::block_on(async {
        assert!(boot::eventually(|| child.stdout().contains(boot::BOOTED)).await);
    });
    child.release();
    let exit = child.finish();
    assert!(exit.success, "{}", exit.output());
}

#[test]
fn delayed_500_does_not_delay_emission() {
    if boot::is_child() {
        boot::child(
            &[&|| {
                let start = std::time::Instant::now();
                for marker in 0..50 {
                    cja::product_event!(name: "prompt_event", user_id: None, message: "Prompt event", properties: { marker = marker });
                }
                assert!(start.elapsed() < Duration::from_millis(50));
            }],
            |_| {},
        );
        return;
    }
    boot::block_on(async {
        let (host, captures) = mock_with(
            Duration::from_millis(250),
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        )
        .await;
        let mut child = boot::spawn(
            "delayed_500_does_not_delay_emission",
            &[
                ("POSTHOG_HOST", &host),
                ("POSTHOG_PROJECT_TOKEN", "phc_fixture"),
            ],
        );
        assert!(boot::eventually(|| child.stdout().contains(boot::BOOTED)).await);
        child.release();
        let exit = child.finish();
        assert!(exit.success, "{}", exit.output());
        assert!(!captures.lock().unwrap().is_empty());
    });
}

#[test]
fn capture_excludes_span_and_wire_dump() {
    if boot::is_child() {
        boot::child(
            &[&|| {
                let span = tracing::info_span!(
                    "req",
                    uri = "/x?token=SPANSECRET",
                    "url.query" = "token=SPANSECRET",
                    user_agent = "UA-SPANSECRET"
                );
                let _entered = span.enter();
                cja::product_event!(name: "safe_view", user_id: None, message: "Safe view", properties: { page = "/safe" });
                tracing::debug!(target: "rig::completions", body = "RIGSECRET");
                tracing::info!(event_type = "job_enqueued", job_id = 42);
            }],
            |_| {},
        );
        return;
    }
    boot::block_on(async {
        let (host, captures) = mock(Duration::ZERO).await;
        let mut child = boot::spawn(
            "capture_excludes_span_and_wire_dump",
            &[
                ("POSTHOG_HOST", &host),
                ("POSTHOG_PROJECT_TOKEN", "phc_fixture"),
                ("RUST_LOG", "debug"),
            ],
        );
        assert!(boot::eventually(|| child.stdout().contains(boot::BOOTED)).await);
        child.release();
        let exit = child.finish();
        assert!(exit.success, "{}", exit.output());
        let requests = captures.lock().unwrap();
        let request = requests
            .iter()
            .find(|r| r.uri.path() == "/i/v1/analytics/events")
            .unwrap();
        let raw = String::from_utf8_lossy(&request.body);
        for secret in ["SPANSECRET", "UA-SPANSECRET", "RIGSECRET", "job_enqueued"] {
            assert!(!raw.contains(secret), "{secret} leaked");
        }
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        let keys: std::collections::BTreeSet<_> = body["batch"][0]["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["page", "service.name"].into_iter().collect());
    });
}

#[test]
fn filtered_tracing_still_captures() {
    if boot::is_child() {
        boot::child(
            &[&|| {
                cja::product_event!(name: "filtered_view", user_id: None, message: "Filtered view", properties: {});
            }],
            |_| {},
        );
        return;
    }
    boot::block_on(async {
        let (host, captures) = mock(Duration::ZERO).await;
        let mut child = boot::spawn(
            "filtered_tracing_still_captures",
            &[
                ("POSTHOG_HOST", &host),
                ("POSTHOG_PROJECT_TOKEN", "phc_fixture"),
                ("RUST_LOG", "off"),
            ],
        );
        assert!(boot::eventually(|| child.stdout().contains(boot::BOOTED)).await);
        child.release();
        let exit = child.finish();
        assert!(exit.success, "{}", exit.output());
        assert!(!exit.stdout.contains("Filtered view"));
        assert!(
            posthog_support::payload(&captures, |p| p == "/i/v1/analytics/events")
                .contains("filtered_view")
        );
    });
}

#[test]
fn large_event_drops_without_request() {
    if boot::is_child() {
        boot::child(
            &[&|| {
                let text = "x".repeat(1024 * 1024);
                cja::product_event!(name: "large_view", user_id: None, message: "Large view", properties: { text = text });
            }],
            |_| {},
        );
        return;
    }
    boot::block_on(async {
        let (host, captures) = mock(Duration::ZERO).await;
        let mut child = boot::spawn(
            "large_event_drops_without_request",
            &[
                ("POSTHOG_HOST", &host),
                ("POSTHOG_PROJECT_TOKEN", "phc_fixture"),
            ],
        );
        assert!(boot::eventually(|| child.stdout().contains(boot::BOOTED)).await);
        child.release();
        let exit = child.finish();
        assert!(exit.success, "{}", exit.output());
        assert_eq!(
            posthog_support::payload(&captures, |p| p == "/i/v1/analytics/events"),
            ""
        );
    });
}

#[test]
fn eyes_retains_events_across_capture_outages() {
    if boot::is_child() {
        boot::child(
            &[&|| {
                for i in 0..20 {
                    cja::product_event!(name: "both_sink_event", user_id: None, message: "Both sink event", properties: { marker = format!("both-marker-{i:02}") });
                }
            }],
            |_| {},
        );
        return;
    }
    boot::block_on(async {
        for case in 0..3 {
            let (eyes_host, eyes) = mock(Duration::ZERO).await;
            let (capture_host, capture) = mock(if case == 1 {
                Duration::from_secs(30)
            } else {
                Duration::ZERO
            })
            .await;
            let host = if case == 0 {
                "http://127.0.0.1:1"
            } else {
                &capture_host
            };
            let mut envs = vec![
                ("POSTHOG_HOST", host),
                ("POSTHOG_PROJECT_TOKEN", "phc_fixture"),
                ("EYES_URL", eyes_host.as_str()),
            ];
            envs.extend_from_slice(&boot::EYES_ENV);
            let mut child = boot::spawn("eyes_retains_events_across_capture_outages", &envs);
            assert!(boot::eventually(|| child.stdout().contains(boot::BOOTED)).await);
            child.release();
            let exit = child.finish();
            assert!(exit.success, "case {case}: {}", exit.output());
            let eyes_body = boot::eyes_payload(&eyes);
            for i in 0..20 {
                assert!(
                    eyes_body.contains(&format!("both-marker-{i:02}")),
                    "case {case} missing marker {i}"
                );
            }
            if case == 2 {
                let capture_body =
                    posthog_support::payload(&capture, |p| p == "/i/v1/analytics/events");
                for i in 0..20 {
                    assert_eq!(
                        capture_body.matches(&format!("both-marker-{i:02}")).count(),
                        1
                    );
                }
            }
        }
    });
}

#[test]
fn dropping_shutdown_handle_keeps_capture_worker() {
    if boot::is_child() {
        boot::block_on(async {
            let handle = cja::setup::setup_tracing("fixture").unwrap();
            drop(handle);
            cja::product_event!(name: "after_handle_drop", user_id: None, message: "After handle drop", properties: {});
            tokio::time::sleep(Duration::from_millis(1500)).await;
        });
        return;
    }
    boot::block_on(async {
        let (host, captures) = mock(Duration::ZERO).await;
        let child = boot::spawn(
            "dropping_shutdown_handle_keeps_capture_worker",
            &[
                ("POSTHOG_HOST", &host),
                ("POSTHOG_PROJECT_TOKEN", "phc_fixture"),
            ],
        );
        let exit = child.finish();
        assert!(exit.success, "{}", exit.output());
        assert!(
            posthog_support::payload(&captures, |p| p == "/i/v1/analytics/events")
                .contains("after_handle_drop")
        );
    });
}

#[test]
fn bare_properties_keep_tracing_types() {
    use tracing::field::{Field, Visit};
    use tracing_subscriber::{Layer, layer::Context, prelude::*};
    struct Visitor(std::sync::Arc<std::sync::Mutex<Vec<String>>>);
    impl Visit for Visitor {
        fn record_bool(&mut self, field: &Field, _: bool) {
            self.0
                .lock()
                .unwrap()
                .push(format!("bool:{}", field.name()));
        }
        fn record_str(&mut self, field: &Field, _: &str) {
            self.0.lock().unwrap().push(format!("str:{}", field.name()));
        }
        fn record_debug(&mut self, field: &Field, _: &dyn std::fmt::Debug) {
            self.0
                .lock()
                .unwrap()
                .push(format!("debug:{}", field.name()));
        }
    }
    struct Capture(std::sync::Arc<std::sync::Mutex<Vec<String>>>);
    impl<S: tracing::Subscriber> Layer<S> for Capture {
        fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
            event.record(&mut Visitor(self.0.clone()));
        }
    }
    let fields = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(Capture(fields.clone()));
    let _guard = tracing::subscriber::set_default(subscriber);
    let id = uuid::Uuid::new_v4();
    cja::product_event!(name: "typed_view", user_id: Some(&id), message: "Message { literal", properties: { first = true, slug = "winner" });
    let fields = fields.lock().unwrap();
    assert!(fields.contains(&"bool:first".to_string()), "{fields:?}");
    assert!(fields.contains(&"str:slug".to_string()), "{fields:?}");
    assert!(fields.iter().any(|f| f.ends_with(":message")), "{fields:?}");
    assert!(
        fields
            .iter()
            .any(|f| f == "debug:user_id" || f == "str:user_id"),
        "{fields:?}"
    );
    assert!(!fields.contains(&"debug:first".to_string()));
    assert!(!fields.contains(&"debug:slug".to_string()));
}

#[test]
fn capture_v1_event_drop_is_reported() {
    if boot::is_child() {
        boot::child(
            &[&|| {
                cja::product_event!(name: "rejected_view", user_id: None, message: "Rejected view", properties: {});
            }],
            |_| {},
        );
        return;
    }
    boot::block_on(async {
        let app = axum::Router::new().fallback(|body: axum::body::Bytes| async move {
            let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let uuid = request["batch"][0]["uuid"].as_str().unwrap();
            let mut results = serde_json::Map::new();
            results.insert(
                uuid.to_owned(),
                serde_json::json!({"result": "drop", "details": "invalid_options"}),
            );
            axum::Json(serde_json::json!({"results": results}))
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut child = boot::spawn(
            "capture_v1_event_drop_is_reported",
            &[
                ("POSTHOG_HOST", &host),
                ("POSTHOG_PROJECT_TOKEN", "phc_fixture"),
            ],
        );
        assert!(boot::eventually(|| child.stdout().contains(boot::BOOTED)).await);
        child.release();
        let exit = child.finish();
        server.abort();
        assert!(exit.success, "{}", exit.output());
        assert!(
            exit.stdout.contains("capture rejected events"),
            "{}",
            exit.output()
        );
    });
}

#[test]
fn capture_warning_is_delivered() {
    capture_verdict_case("capture_warning_is_delivered", "warning", 0, false);
}

#[test]
fn capture_duplicate_is_rejected() {
    capture_verdict_case("capture_duplicate_is_rejected", "ok", 1, true);
}

fn capture_verdict_case(test: &str, verdict: &str, duplicates: u64, rejected: bool) {
    if boot::is_child() {
        boot::child(
            &[&|| {
                cja::product_event!(name: "verdict_view", user_id: None, message: "Verdict view", properties: {});
            }],
            |_| {},
        );
        return;
    }
    boot::block_on(async {
        let verdict = verdict.to_owned();
        let received = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let recorder = received.clone();
        let app = axum::Router::new().fallback(move |body: axum::body::Bytes| {
            let verdict = verdict.clone();
            let recorder = recorder.clone();
            async move {
                recorder.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
                let uuid = request["batch"][0]["uuid"].as_str().unwrap();
                let mut results = serde_json::Map::new();
                results.insert(
                    uuid.to_owned(),
                    serde_json::json!({"result": verdict, "duplicates_dropped": duplicates}),
                );
                axum::Json(serde_json::json!({"results": results}))
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut child = boot::spawn(
            test,
            &[
                ("POSTHOG_HOST", &host),
                ("POSTHOG_PROJECT_TOKEN", "phc_fixture"),
            ],
        );
        assert!(boot::eventually(|| child.stdout().contains(boot::BOOTED)).await);
        child.release();
        let exit = child.finish();
        server.abort();
        assert!(exit.success, "{}", exit.output());
        assert_eq!(received.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert!(!exit.stdout.contains("capture request failed"), "{}", exit.output());
        assert!(!exit.stdout.contains("capture response unreadable"), "{}", exit.output());
        assert_eq!(
            exit.stdout.contains("capture rejected events"),
            rejected,
            "{}",
            exit.output()
        );
    });
}

#[test]
fn shutdown_drains_events_queued_during_flush() {
    if boot::is_child() {
        boot::child(
            &[
                &|| cja::product_event!(name: "drain_a", user_id: None, message: "Drain A", properties: {}),
                &|| cja::product_event!(name: "drain_b", user_id: None, message: "Drain B", properties: {}),
            ],
            |_| {},
        );
        return;
    }
    boot::block_on(async {
        let (host, captures) = mock(Duration::from_millis(1500)).await;
        let mut child = boot::spawn(
            "shutdown_drains_events_queued_during_flush",
            &[
                ("POSTHOG_HOST", &host),
                ("POSTHOG_PROJECT_TOKEN", "phc_fixture"),
            ],
        );
        assert!(boot::eventually(|| child.stdout().contains(boot::BOOTED)).await);
        assert!(boot::eventually(|| captures.lock().unwrap().iter().any(|r| r.uri.path() == "/i/v1/analytics/events")).await);
        child.release();
        child.release();
        let exit = child.finish();
        assert!(exit.success, "{}", exit.output());
        let requests = captures.lock().unwrap();
        let batches: Vec<serde_json::Value> = requests.iter().filter(|r| r.uri.path() == "/i/v1/analytics/events")
            .map(|r| serde_json::from_slice(&r.body).unwrap()).collect();
        assert_eq!(batches.len(), 2, "{batches:?}");
        assert_eq!(batches[0]["batch"][0]["event"], "drain_a");
        assert_eq!(batches[0]["batch"].as_array().unwrap().len(), 1);
        assert_eq!(batches[1]["batch"][0]["event"], "drain_b");
        assert_eq!(batches[1]["batch"].as_array().unwrap().len(), 1);
    });
}
