//! Pinned PR review finding (DEV-2483 round 1): the capture worker must batch
//! "at most 100 events or 1 second of arrivals". A steady trickle of product
//! events (one every 500 ms) must still produce a capture request within a few
//! seconds, not only once 100 events pile up or the process shuts down.
mod posthog_support;
use posthog_support::{boot, mock};
use std::time::Duration;

const PHASES: usize = 7;

#[test]
fn steady_trickle_flushes_within_batch_window() {
    if boot::is_child() {
        let emit: &dyn Fn() = &|| {
            cja::product_event!(name: "trickle_event", user_id: None, message: "Trickle event", properties: {});
        };
        boot::child(&[emit; PHASES], |_| {});
        return;
    }
    boot::block_on(async {
        let (host, captures) = mock(Duration::ZERO).await;
        let mut child = boot::spawn(
            "steady_trickle_flushes_within_batch_window",
            &[
                ("POSTHOG_HOST", &host),
                ("POSTHOG_PROJECT_TOKEN", "phc_fixture"),
            ],
        );
        assert!(boot::eventually(|| child.stdout().contains(boot::BOOTED)).await);
        // Phase 0 has emitted the first event. Release one phase every 500 ms,
        // so a new event arrives every 500 ms for ~3 s.
        for _ in 0..PHASES - 1 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            child.release();
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        let sent_before_shutdown = captures
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.uri.path() == "/i/v1/analytics/events")
            .count();
        child.release();
        let exit = child.finish();
        assert!(exit.success, "{}", exit.output());
        assert!(
            sent_before_shutdown > 0,
            "no capture request ~3.3 s after the first product event while events kept \
             arriving every 500 ms; the 1 s batch window must bound latency from the \
             batch's first event, not reset on every arrival"
        );
    });
}
