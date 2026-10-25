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

/// Once a request is queued, the total deadline must remain a timeout even
/// if the gateway has sent an error status but stalls in the error body.
/// Returning the status instead hides the timeout and may allow callers to
/// treat an incomplete provider reply as a definitive refusal.
#[tokio::test]
async fn stalled_error_body_reports_the_request_timeout() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        socket.read(&mut request).await.unwrap();
        socket
            .write_all(b"HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n")
            .await
            .unwrap();
        std::future::pending::<()>().await;
    });

    let url = url::Url::parse(&format!("http://{address}")).unwrap();
    let llm = Llm::new(
        &config(url)
            .request_timeout(Duration::from_millis(150))
            .retries(RetryPolicy {
                max_attempts: 1,
                ..RetryPolicy::default()
            }),
    )
    .unwrap();
    let error = llm.complete(models::DEFAULT, "ping").await.unwrap_err();
    server.abort();
    assert!(matches!(error, Error::Timeout { .. }), "{error:?}");
}

/// The gateway's final SSE usage chunk must make the per-call completion
/// event carry the reported token counts, as required for event-only sinks.
#[tokio::test]
async fn streamed_usage_reaches_the_completion_event() {
    use futures::StreamExt as _;
    use std::sync::Mutex;

    struct Writer(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Writer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let captured = Arc::new(Mutex::new(Vec::new()));
    let output = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || Writer(output.clone()))
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false)
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);

    let fake = FakeGateway::start().await;
    fake.chat_stream(models::DEFAULT, &["hello"]);
    let llm = Llm::new(&config(fake.url())).unwrap();
    let chunks: Vec<String> = llm
        .stream(models::DEFAULT, "ping")
        .map(|item| item.unwrap())
        .collect()
        .await;
    assert_eq!(chunks, ["hello"]);
    drop(guard);

    let text = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
    let event = text
        .lines()
        .find(|line| line.contains("gateway http call finished"))
        .expect("transport completion event was captured");
    assert!(event.contains("input_tokens=3"), "{event}");
    assert!(event.contains("output_tokens=2"), "{event}");
}
