//! Fake gateway for tests: an in-process scripted `OpenAI` chat-completions
//! server.
//!
//! Requests flow through a real HTTP server (`hyper`, loopback) and are meant
//! to be driven through the real [`crate::transport`] over the host dialer,
//! so header stripping, staged retries, timeouts, and SSE parsing behave
//! exactly as they do against the live gateway — with no network, gateway,
//! or tailnet. Replies are scripted FIFO, one per request; every request the
//! server saw is recorded.
//!
//! Build a client against it with tiny timeouts:
//!
//! ```no_run
//! # async fn example() -> cja_llm::Result<()> {
//! use std::time::Duration;
//! use cja_llm::{DialerChoice, Llm, LlmConfig};
//! use cja_llm::config::RetryPolicy;
//!
//! let fake = cja_llm::testing::FakeGateway::start().await;
//! fake.chat_completion("anthropic/claude-haiku-5-5", "hello");
//! let url = fake.url();
//! let config = LlmConfig::new(url.clone())
//!     .app_name("test")
//!     .connect_timeout(Duration::from_millis(250))
//!     .request_timeout(Duration::from_millis(250))
//!     .retries(RetryPolicy { max_attempts: 3, initial_backoff: Duration::from_millis(10), ..RetryPolicy::default() });
//! let llm = Llm::new(&config)?;
//! let text = llm.complete(cja_llm::models::DEFAULT, "hi").await?;
//! # Ok(())
//! # }
//! ```

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt as _;
use http_body_util::{BodyExt as _, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use url::Url;

/// One request the fake gateway saw, in arrival order.
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    /// Request method, e.g. `POST`.
    pub method: String,
    /// The URI path, e.g. `/v1/chat/completions`.
    pub path: String,
    /// The request headers, verbatim (what crossed the wire).
    pub headers: http::HeaderMap,
    /// The request body, verbatim.
    pub body: Vec<u8>,
}

impl RecordedRequest {
    /// The body parsed as JSON, panicking if it is not.
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).expect("recorded body is JSON")
    }
}

/// A scripted reply, or a scripted non-reply.
enum Scripted {
    /// Sleep before continuing to the next entry.
    Delay(Duration),
    /// A full response.
    Response {
        status: u16,
        content_type: &'static str,
        body: Vec<u8>,
    },
    /// An SSE response whose chunks are separated by `gap` (the first chunk
    /// goes out immediately).
    Stream {
        status: u16,
        chunks: Vec<Bytes>,
        gap: Duration,
    },
    /// Read the full request, then close the connection without replying.
    DropConnection,
}

#[derive(Default)]
struct Shared {
    script: Mutex<VecDeque<Scripted>>,
    requests: Mutex<Vec<RecordedRequest>>,
    open: AtomicUsize,
}

impl Shared {
    fn record(&self, request: RecordedRequest) {
        self.requests
            .lock()
            .expect("fake requests lock")
            .push(request);
    }

    fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().expect("fake requests lock").clone()
    }
}

/// The fake gateway. Stops on drop. Replies are served strictly in the order
/// they were scripted, one per request.
pub struct FakeGateway {
    addr: std::net::SocketAddr,
    shared: Arc<Shared>,
    accept_task: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for FakeGateway {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeGateway")
            .field("addr", &self.addr)
            .finish_non_exhaustive()
    }
}

impl Drop for FakeGateway {
    fn drop(&mut self) {
        // Genuinely stop: abort the accept loop, which drops the listener.
        self.accept_task.abort();
    }
}

impl FakeGateway {
    /// Binds `127.0.0.1:0` and starts serving the script.
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("loopback addr");
        let shared = Arc::new(Shared::default());
        let accept_shared = shared.clone();
        let accept_task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let shared = accept_shared.clone();
                shared.open.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    serve(stream, shared.clone()).await;
                    shared.open.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        Self {
            addr,
            shared,
            accept_task,
        }
    }

    /// The fake's origin, ready for [`crate::LlmConfig::new`].
    pub fn url(&self) -> Url {
        Url::parse(&format!("http://{}", self.addr)).expect("loopback url")
    }

    /// Scripts a 200 with a full chat-completion carrying `text`.
    pub fn chat_completion(&self, model: &str, text: &str) {
        let body = serde_json::json!({
            "id": "fake",
            "object": "chat.completion",
            "created": 0,
            "model": model,
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": text},
                "finish_reason": "stop",
            }],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2},
        });
        self.push(Scripted::Response {
            status: 200,
            content_type: "application/json",
            body: body.to_string().into_bytes(),
        });
    }

    /// Scripts a 200 whose content is `json` serialized to a string — the
    /// shape structured-output extraction parses.
    pub fn chat_json(&self, model: &str, json: &serde_json::Value) {
        self.chat_completion(model, &json.to_string());
    }

    /// Scripts a 200 SSE stream yielding `deltas` as content chunks, then a
    /// usage chunk, then `[DONE]`.
    pub fn chat_stream(&self, model: &str, deltas: &[&str]) {
        self.chat_stream_with_gap(model, deltas, Duration::ZERO);
    }

    /// [`FakeGateway::chat_stream`] with a pause between chunks (the first
    /// chunk still goes out immediately), for idle-timeout tests.
    pub fn chat_stream_with_gap(&self, model: &str, deltas: &[&str], gap: Duration) {
        let chunks: Vec<Bytes> = deltas
            .iter()
            .map(|delta| sse_chunk(model, delta))
            .chain([
                sse_finish_chunk(model),
                sse_usage_chunk(model),
                Bytes::from_static(b"data: [DONE]\n\n"),
            ])
            .collect();
        self.push(Scripted::Stream {
            status: 200,
            chunks,
            gap,
        });
    }

    /// Scripts a 200 whose assistant message carries `calls` tool calls in
    /// call order, with `finish_reason: "tool_calls"`. Each entry is
    /// `(call_id, tool_name, arguments)`.
    pub fn tool_calls(&self, model: &str, calls: &[(&str, &str, &serde_json::Value)]) {
        let tool_calls: Vec<_> = calls
            .iter()
            .map(|(call_id, name, args)| {
                serde_json::json!({
                    "id": call_id,
                    "type": "function",
                    "function": {"name": name, "arguments": args.to_string()},
                })
            })
            .collect();
        let body = serde_json::json!({
            "id": "fake",
            "object": "chat.completion",
            "created": 0,
            "model": model,
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "tool_calls": tool_calls,
                },
                "finish_reason": "tool_calls",
            }],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2},
        });
        self.push(Scripted::Response {
            status: 200,
            content_type: "application/json",
            body: body.to_string().into_bytes(),
        });
    }

    /// Scripts a 200 whose assistant message is a single tool call, with
    /// `finish_reason: "tool_calls"`.
    pub fn tool_call(&self, model: &str, call_id: &str, name: &str, args: &serde_json::Value) {
        self.tool_calls(model, &[(call_id, name, args)]);
    }

    /// Scripts a raw status + body reply (error envelopes).
    pub fn status(&self, status: u16, body: &str) {
        self.push(Scripted::Response {
            status,
            content_type: "application/json",
            body: body.as_bytes().to_vec(),
        });
    }

    /// Scripts a pause before the next reply's headers.
    pub fn delay(&self, delay: Duration) {
        self.push(Scripted::Delay(delay));
    }

    /// Scripts reading the full request, then closing the connection without
    /// replying — the wire shape of a post-queue reset.
    pub fn drop_connection(&self) {
        self.push(Scripted::DropConnection);
    }

    /// Every request the fake saw, in order.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.shared.requests()
    }

    /// Waits until no connection is open, panicking after ~5s. The
    /// drop-cancels proof: a leaked client connection keeps this waiting.
    pub async fn wait_until_disconnected(&self) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while self.shared.open.load(Ordering::SeqCst) > 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "fake gateway still has open connections"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn push(&self, scripted: Scripted) {
        self.shared
            .script
            .lock()
            .expect("fake script lock")
            .push_back(scripted);
    }
}

fn sse_line(payload: &serde_json::Value) -> Bytes {
    Bytes::from(format!("data: {payload}\n\n"))
}

fn sse_chunk(model: &str, delta: &str) -> Bytes {
    sse_line(&serde_json::json!({
        "id": "fake",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": {"content": delta},
            "finish_reason": null,
        }],
    }))
}

fn sse_finish_chunk(model: &str) -> Bytes {
    sse_line(&serde_json::json!({
        "id": "fake",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": "stop",
        }],
    }))
}

fn sse_usage_chunk(model: &str) -> Bytes {
    sse_line(&serde_json::json!({
        "id": "fake",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": model,
        "choices": [],
        "usage": {"prompt_tokens": 3, "completion_tokens": 2},
    }))
}

/// Pops the next scripted reply, sleeping through leading delays. Panics
/// when the script runs dry: every request must have a reply.
async fn next_reply(shared: &Shared) -> Scripted {
    loop {
        let entry = shared.script.lock().expect("fake script lock").pop_front();
        match entry {
            Some(Scripted::Delay(delay)) => tokio::time::sleep(delay).await,
            other => return other.expect("a scripted reply for every request"),
        }
    }
}

/// Serves exactly one request per connection (the transport's shape), then
/// closes. `DropConnection` closes after reading the request instead of
/// replying, by notifying the connection wrapper to drop the connection.
async fn serve(stream: tokio::net::TcpStream, shared: Arc<Shared>) {
    let kill = Arc::new(tokio::sync::Notify::new());
    let service_kill = kill.clone();
    let service = service_fn(move |request: http::Request<Incoming>| {
        let shared = shared.clone();
        let kill = service_kill.clone();
        async move {
            let (parts, body) = request.into_parts();
            let bytes = body
                .collect()
                .await
                .expect("fake reads the request body")
                .to_bytes();
            shared.record(RecordedRequest {
                method: parts.method.as_str().to_owned(),
                path: parts.uri.path().to_owned(),
                headers: parts.headers.clone(),
                body: bytes.to_vec(),
            });

            // Delays run out before the reply's headers.
            let reply = next_reply(&shared).await;
            match reply {
                Scripted::Response {
                    status,
                    content_type,
                    body,
                } => Ok::<_, std::convert::Infallible>(
                    http::Response::builder()
                        .status(status)
                        .header(http::header::CONTENT_TYPE, content_type)
                        .body(Full::new(Bytes::from(body)).boxed())
                        .expect("scripted response builds"),
                ),
                Scripted::Stream {
                    status,
                    chunks,
                    gap,
                } => {
                    // The first chunk goes out immediately; later chunks
                    // wait `gap`, so an idle-timeout test can watch the
                    // silence between them.
                    let stream = futures::stream::iter(chunks.into_iter().enumerate()).then(
                        move |(index, chunk)| async move {
                            if index > 0 {
                                tokio::time::sleep(gap).await;
                            }
                            Ok::<_, std::convert::Infallible>(Frame::data(chunk))
                        },
                    );
                    Ok(http::Response::builder()
                        .status(status)
                        .header(http::header::CONTENT_TYPE, "text/event-stream")
                        .body(http_body_util::BodyExt::boxed(StreamBody::new(stream)))
                        .expect("scripted stream builds"))
                }
                Scripted::DropConnection => {
                    // Tell the connection wrapper to drop the connection
                    // before any response is written, and never finish this
                    // service future so no reply races the drop.
                    kill.notify_one();
                    futures::future::pending().await
                }
                Scripted::Delay(_) => unreachable!("delays drain before the reply is popped"),
            }
        }
    });

    let connection = http1::Builder::new().serve_connection(TokioIo::new(stream), service);
    tokio::pin!(connection);
    tokio::select! {
        result = &mut connection => {
            if let Err(error) = result {
                tracing::debug!(%error, "fake gateway connection ended with an error");
            }
        }
        () = kill.notified() => {
            // Dropping the connection closes the socket without a reply.
        }
    }
}

/// A [`setec::Dialer`] test double that counts dials and never reaches any
/// gateway. Inject it through [`crate::DialerChoice::Custom`]; the dial
/// count is the attempt-count oracle for retry tests.
pub struct CountingDialer {
    dials: AtomicUsize,
    behavior: DialBehavior,
}

enum DialBehavior {
    /// Fails immediately with `ConnectionRefused`.
    Refuse,
    /// Never resolves, until the connect timeout cancels it.
    NeverCompletes,
}

impl CountingDialer {
    /// A dialer whose dials fail fast: nothing sent, retryable.
    pub fn refusing() -> Arc<Self> {
        Arc::new(Self {
            dials: AtomicUsize::new(0),
            behavior: DialBehavior::Refuse,
        })
    }

    /// A dialer whose dials hang until cancelled: connect-timeout material.
    pub fn never_completes() -> Arc<Self> {
        Arc::new(Self {
            dials: AtomicUsize::new(0),
            behavior: DialBehavior::NeverCompletes,
        })
    }

    /// How many dials were attempted.
    pub fn dials(&self) -> usize {
        self.dials.load(Ordering::SeqCst)
    }
}

impl setec::Dialer for CountingDialer {
    fn dial<'a>(&'a self, _host: &'a str, _port: u16) -> setec::DialFuture<'a> {
        self.dials.fetch_add(1, Ordering::SeqCst);
        let behavior = match self.behavior {
            DialBehavior::Refuse => 0,
            DialBehavior::NeverCompletes => 1,
        };
        Box::pin(async move {
            match behavior {
                0 => Err(std::io::Error::from(std::io::ErrorKind::ConnectionRefused)),
                _ => futures::future::pending().await,
            }
        })
    }
}

/// Host-side side-effect flag for `#[rig_tool]` test tools: 0.45's macro
/// generates unit structs, so host state rides the run's tool context scope.
/// Test-only, shared across this crate's test modules.
#[cfg(test)]
pub(crate) struct Ran(pub tokio::sync::Mutex<bool>);

/// A config pointing at `url` with the tiny timeouts the crate's fake-gateway
/// tests use. Test-only, shared across this crate's test modules.
#[cfg(test)]
pub(crate) fn test_config(url: Url) -> crate::LlmConfig {
    crate::LlmConfig::new(url)
        .app_name("fake-test")
        .connect_timeout(Duration::from_millis(250))
        .request_timeout(Duration::from_millis(2000))
        .stream_idle_timeout(Duration::from_millis(250))
        .retries(crate::config::RetryPolicy {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(10),
            ..crate::config::RetryPolicy::default()
        })
        .dialer(Arc::new(setec::TcpDialer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::GatewayHttp;

    use rig_core::http_client::HttpClientExt as _;

    /// The raw transport round-trips a request against the fake: no
    #[tokio::test]
    async fn raw_transport_round_trips_against_the_fake() {
        let fake = FakeGateway::start().await;
        fake.chat_completion("anthropic/claude-haiku-5-5", "pong");
        let config = test_config(fake.url());
        let http = GatewayHttp::new(&config).unwrap();

        let request = http::Request::post("/v1/chat/completions")
            .header(http::header::AUTHORIZATION, "Bearer cja-llm-keyless")
            .header("x-api-key", "should-not-leave")
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Bytes::from(
                serde_json::json!({"model": "anthropic/claude-haiku-5-5", "messages": []})
                    .to_string(),
            ))
            .unwrap();
        let response = http.send::<_, Bytes>(request).await.unwrap();
        assert_eq!(response.status(), 200);
        let body = response.into_body().await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            json["choices"][0]["message"]["content"], "pong",
            "body round-trips"
        );

        let requests = fake.requests();
        assert_eq!(requests.len(), 1);
        let recorded = &requests[0];
        assert_eq!(recorded.method, "POST");
        assert_eq!(recorded.path, "/v1/chat/completions");
        for stripped in ["authorization", "x-api-key", "api-key", "x-goog-api-key"] {
            assert!(
                recorded.headers.get(stripped).is_none(),
                "{stripped} must not leave the process"
            );
        }
        assert!(
            recorded
                .headers
                .get(http::header::USER_AGENT)
                .is_some_and(|value| value.to_str().unwrap().starts_with("cja-llm/")),
            "the fixed user agent is on the wire"
        );
    }

    /// The fake serves every scripted kind, and unscripted requests panic
    /// loudly rather than hanging.
    #[tokio::test]
    async fn fake_serves_errors_and_retries() {
        let fake = FakeGateway::start().await;
        let config = test_config(fake.url());
        let http = GatewayHttp::new(&config).unwrap();

        fake.status(429, r#"{"error":"rate limited"}"#);
        fake.status(429, r#"{"error":"rate limited"}"#);
        fake.chat_completion("m", "third time lucky");

        let request = http::Request::post("/v1/chat/completions")
            .body(Bytes::from_static(b"{}"))
            .unwrap();
        let response = http.send::<_, Bytes>(request).await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(fake.requests().len(), 3, "429s retried, then success");
    }

    #[tokio::test]
    async fn counting_dialer_counts_and_refuses() {
        let dialer = CountingDialer::refusing();
        let config = test_config(Url::parse("http://127.0.0.1:1").unwrap()).dialer(dialer.clone());
        let http = GatewayHttp::new(&config).unwrap();
        let request = http::Request::post("/v1/chat/completions")
            .body(Bytes::from_static(b"{}"))
            .unwrap();
        let Err(error) = http.send::<_, Bytes>(request).await else {
            panic!("the refusing dialer must fail");
        };
        assert!(error.to_string().contains("cja-llm transport connect:"));
        assert_eq!(dialer.dials(), 3, "every attempt dials once");
    }
}

#[cfg(test)]
mod smoke {
    use super::*;
    use crate::{Llm, models};

    #[tokio::test]
    async fn llm_complete_round_trips_through_the_middleware() {
        let fake = FakeGateway::start().await;
        fake.chat_completion(models::DEFAULT, "pong");
        let config = test_config(fake.url());
        let llm = Llm::new(&config).unwrap();
        let text = llm.complete(models::DEFAULT, "ping").await.unwrap();
        assert_eq!(text, "pong");
        let requests = fake.requests();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].headers.get("x-app").is_some());
        assert!(requests[0].headers.get("authorization").is_none());
    }
}
