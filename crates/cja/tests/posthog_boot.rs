//! Both sinks through the real `setup_tracing` while `PostHog` is STALLED (the
//! mock accepts each export and holds it for 30 s): Eyes still receives every
//! event, emission is not slowed, and the shutdown flush fits the kill window.
use posthog_support::boot;
mod posthog_support;
use std::time::Duration;

#[test]
fn dual_sink_boot() {
    if boot::is_child() {
        return boot::child(&[&boot::emit_markers], |shutdown| {
            // Non-vacuity: the stalled export was in flight, so the flush had
            // to run into its own bound rather than finish instantly.
            assert!(
                shutdown > Duration::from_secs(1),
                "shutdown returned in {shutdown:?}: PostHog was not stalled"
            );
        });
    }
    boot::block_on(async {
        let (eyes, events) = posthog_support::mock(Duration::ZERO).await;
        let (host, arrivals) = posthog_support::mock(Duration::from_secs(30)).await;
        let mut envs = boot::EYES_ENV.to_vec();
        envs.extend([
            ("EYES_URL", eyes.as_str()),
            ("POSTHOG_PROJECT_TOKEN", "phc_test"),
            ("POSTHOG_HOST", host.as_str()),
        ]);
        let child = boot::spawn("dual_sink_boot", &envs);
        let delivered = boot::eventually(|| {
            boot::has_markers(&boot::eyes_payload(&events))
                && arrivals.lock().unwrap().iter().any(|c| {
                    c.uri.path() == "/i/v1/logs" && c.headers.contains_key("authorization")
                })
        })
        .await;
        // The child asserts emission < 1 s and shutdown < 3.5 s while stalled.
        let exit = child.finish();
        assert!(exit.success, "{}", exit.output());
        assert!(delivered, "missing telemetry within ten seconds");
    });
}
