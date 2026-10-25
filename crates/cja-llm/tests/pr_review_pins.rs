//! Pinned PR-review findings (DEV-1923, PR #73). Each test encodes a
//! contract the transport must keep; do not weaken or delete them.

use std::sync::Arc;
use std::time::Duration;

use cja_llm::testing::FakeGateway;
use cja_llm::{Error, Llm, LlmConfig, RetryPolicy, models};

fn config(url: url::Url) -> LlmConfig {
    LlmConfig::new(url)
        .app_name("pin-test")
        .connect_timeout(Duration::from_millis(500))
        .request_timeout(Duration::from_secs(5))
        .retries(RetryPolicy {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(10),
            ..RetryPolicy::default()
        })
        .dialer(Arc::new(setec::TcpDialer))
}

/// An error body larger than the 64 KiB classification cap whose cap offset
/// falls inside a multi-byte UTF-8 character must still classify as a
/// typed `Upstream` error. `String::truncate` at a non-char boundary panics,
/// so a large non-ASCII upstream error envelope must not crash the caller.
#[tokio::test]
async fn oversized_multibyte_error_body_classifies_instead_of_panicking() {
    let fake = FakeGateway::start().await;
    // "€" is 3 bytes; 65536 % 3 == 1, so byte 65536 is mid-character.
    let body = "€".repeat(30_000);
    fake.status(400, &body);
    let llm = Llm::new(&config(fake.url())).unwrap();

    let error = llm
        .complete(models::DEFAULT, "ping")
        .await
        .expect_err("a 400 reply is an error");
    assert!(
        matches!(error, Error::Upstream { status: 400, .. }),
        "{error:?}"
    );
    assert_eq!(fake.requests().len(), 1, "400 is terminal");
}
