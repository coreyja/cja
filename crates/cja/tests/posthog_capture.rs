mod posthog_support;
use posthog_support::{boot, mock};
use std::time::Duration;

#[test]
fn capture_payload() {
    if boot::is_child() {
        boot::child(
            &[&|| {
                let user = uuid::Uuid::parse_str("11111111-1111-4111-8111-111111111111").unwrap();
                cja::product_event!(name: "achievement_earned", user_id: Some(&user), message: "Achievement earned", properties: { first = true, slug = "winner" });
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
        assert!(exit.stdout.contains("game_phase"));
        let requests = captures.lock().unwrap();
        let request = requests
            .iter()
            .find(|r| r.uri.path() == "/i/v1/analytics/events")
            .expect("capture request");
        assert!(request.uri.query().is_none());
        assert_eq!(request.headers["authorization"], "Bearer phc_fixture");
        assert!(request.headers.contains_key("user-agent"));
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
        assert_eq!(body["batch"].as_array().unwrap().len(), 1);
        assert_eq!(event["event"], "anonymous_view");
        assert_eq!(event["options"]["process_person_profile"], false);
        assert!(event["properties"].get("$process_person_profile").is_none());
        assert_eq!(event["properties"]["service.name"], "fixture");
        assert_ne!(event["distinct_id"], event["uuid"]);
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
    let mut child = boot::spawn(
        "blank_token_disables_capture",
        &[
            ("POSTHOG_HOST", "invalid host"),
            ("POSTHOG_PROJECT_TOKEN", " "),
        ],
    );
    boot::block_on(async {
        assert!(boot::eventually(|| child.stdout().contains(boot::BOOTED)).await);
    });
    child.release();
    let exit = child.finish();
    assert!(exit.success, "{}", exit.output());
    assert!(exit.stdout.contains("No capture"));
}

#[test]
fn unreachable_capture_does_not_delay_emission() {
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
