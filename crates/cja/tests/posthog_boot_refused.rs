mod posthog_support;
use posthog_support::mock;
#[path = "posthog_support/boot.rs"]
mod boot;
#[test]
fn dual_sink_boot() {
    boot::boot(false);
}
