//! Both sinks through the real `setup_tracing` while `PostHog` REFUSES every
//! connection: Eyes still receives every event, emission is not slowed, and
//! the shutdown flush fits the kill window.
use posthog_support::boot;
mod posthog_support;
use std::time::Duration;

#[test]
fn dual_sink_boot() {
    if boot::is_child() {
        return boot::child(&[&boot::emit_markers], |_| {});
    }
    boot::block_on(async {
        let (eyes, events) = posthog_support::mock(Duration::ZERO).await;
        let mut envs = boot::EYES_ENV.to_vec();
        envs.extend([
            ("EYES_URL", eyes.as_str()),
            ("POSTHOG_PROJECT_TOKEN", "phc_test"),
            ("POSTHOG_HOST", "http://127.0.0.1:1"),
        ]);
        let child = boot::spawn("dual_sink_boot", &envs);
        let delivered = boot::eventually(|| boot::has_markers(&boot::eyes_payload(&events))).await;
        let exit = child.finish();
        assert!(exit.success, "{}", exit.output());
        assert!(delivered, "missing telemetry within ten seconds");
    });
}
