//! The shared HTTP transport to the LLM gateway.
//!
//! `GatewayHttp` implements rig's [`HttpClientExt`] over a `setec::Dialer`:
//! the host network by default, or an in-process tailnet node. Everything the
//! crate sends goes through it — staged retries, timeouts, tracing, and the
//! two-layer stripping of key-shaped headers.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt as _;
use http_body_util::{BodyExt as _, Full};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use rig_core::http_client::{
    BoxedStream, Error as HttpError, HttpClientExt, LazyBody, MultipartForm, Result as HttpResult,
    StreamingResponse,
};
use tokio_rustls::TlsConnector;

use crate::Error;
use crate::config::{DialerChoice, LlmConfig, RetryPolicy};

/// Marker prefix on [`TransportError::Connect`] variants. The classifier in
/// `error.rs` matches this substring in rig's relayed error text to classify
/// connect-phase failures; keep it stable.
pub(crate) const CONNECT_MARKER: &str = "cja-llm transport connect:";

/// Marker prefix on [`TransportError::Timeout`]. Like [`CONNECT_MARKER`], the
/// classifier depends on this exact string.
pub(crate) const TIMEOUT_MARKER: &str = "cja-llm transport timeout:";

/// Key-shaped headers that never leave the process. The middleware
/// ([`GatewayHeaders`]) removes rig's placeholder `authorization`; this list
/// is the transport-side backstop on every rebuild of the request.
const STRIPPED_HEADERS: &[&str] = &["authorization", "x-api-key", "api-key", "x-goog-api-key"];

/// Error bodies are read for classification but never trusted to be huge.
const MAX_ERROR_BODY: usize = 64 * 1024;

/// Response-less transport failures, wrapped into
/// `rig_core::http_client::Error::Instance` for rig.
///
/// The `Display` strings are load-bearing: they start with the marker
/// constants and ride verbatim into rig's error text on both paths rig can
/// surface them (`PromptError::Provider` via `ProviderError::Http`, and
/// `PromptError::Report` via the serve adapter, whose `message` is the
/// source's `Display`). The classifier in `error.rs` matches the markers
/// there, so never reword these casually.
#[derive(Debug, thiserror::Error)]
pub(crate) enum TransportError {
    /// The connect phase (dial, TLS, HTTP handshake) exceeded its budget.
    /// Nothing was sent.
    #[error(
        "cja-llm transport connect: timed out after {0:?} before the request was sent; is this node on the tailnet with a policy grant to llm:443 (tag:llm on port 443)?"
    )]
    ConnectTimeout(Duration),
    /// The connect phase failed outright. Nothing was sent.
    #[error(
        "cja-llm transport connect: failed before the request was sent: {source}; is this node on the tailnet with a policy grant to llm:443 (tag:llm on port 443)?"
    )]
    Connect {
        #[source]
        source: std::io::Error,
    },
    /// The request deadline elapsed after the request was queued to the
    /// connection. The gateway drains and bills such attempts upstream, so
    /// this is never retried.
    #[error(
        "cja-llm transport timeout: request exceeded its deadline after the request was sent ({0:?} budget; the gateway may still bill the attempt)"
    )]
    Timeout(Duration),
}

/// rig middleware applied at the transport boundary: strip rig's placeholder
/// `Authorization` header and stamp `X-App`. The gateway ignores `X-App` for
/// access, identity, and budget — it is a Bifrost log dimension only.
#[derive(Clone)]
pub(crate) struct GatewayHeaders {
    pub(crate) app: http::HeaderValue,
}

impl rig_core::http_client::middleware::HttpMiddleware for GatewayHeaders {
    fn before_request_headers<'a>(
        &'a self,
        _exchange: rig_core::http_client::middleware::Exchange<'a>,
        headers: &'a mut http::HeaderMap,
    ) -> rig_core::wasm_compat::WasmBoxedFuture<'a, HttpResult<()>> {
        Box::pin(async move {
            headers.remove(http::header::AUTHORIZATION);
            headers.insert(
                http::header::HeaderName::from_static("x-app"),
                self.app.clone(),
            );
            Ok(())
        })
    }
}

/// The spawned hyper connection driver: resolves when the connection ends.
type Driver = tokio::task::JoinHandle<Result<(), hyper::Error>>;

/// The single HTTP transport every cja-llm request takes: HTTPS (or loopback
/// HTTP, tests only) over a `setec::Dialer`, one fresh connection per attempt.
#[derive(Clone)]
pub(crate) struct GatewayHttp {
    inner: Arc<Inner>,
}

struct Inner {
    host: String,
    port: u16,
    host_header: String,
    tls: Option<TlsConnector>,
    dialer: Arc<dyn setec::Dialer>,
    app: String,
    connect_timeout: Duration,
    request_timeout: Duration,
    stream_idle_timeout: Duration,
    retries: RetryPolicy,
    user_agent: http::HeaderValue,
}

impl GatewayHttp {
    /// Builds the transport for `config`. `https` verifies the server against
    /// the Mozilla root set; `http` exists solely so the test fake can serve
    /// on loopback.
    pub(crate) fn new(config: &LlmConfig) -> Result<Self, Error> {
        let tls = match config.gateway.scheme() {
            "https" => Some(tls_connector()?),
            "http" => None,
            other => {
                return Err(Error::Config(format!(
                    "gateway URL scheme {other:?} is not supported; use https in production (http is for tests)"
                )));
            }
        };
        let host = config
            .gateway
            .host_str()
            .ok_or_else(|| Error::Config("gateway URL has no host".into()))?
            .to_owned();
        let port = config
            .gateway
            .port_or_known_default()
            .ok_or_else(|| Error::Config("gateway URL has no port".into()))?;
        let host_header = match config.gateway.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.clone(),
        };
        let dialer: Arc<dyn setec::Dialer> = match &config.dialer {
            DialerChoice::Host => Arc::new(setec::TcpDialer),
            #[cfg(feature = "tailnet")]
            DialerChoice::Tailnet(tailnet) => tailnet.clone(),
            DialerChoice::Custom(dialer) => dialer.clone(),
        };
        Ok(Self {
            inner: Arc::new(Inner {
                host,
                port,
                host_header,
                tls,
                dialer,
                app: config.app_name.clone(),
                connect_timeout: config.connect_timeout,
                request_timeout: config.request_timeout,
                stream_idle_timeout: config.stream_idle_timeout,
                retries: config.retries.clone(),
                user_agent: http::HeaderValue::from_str(&format!(
                    "cja-llm/{}",
                    env!("CARGO_PKG_VERSION")
                ))
                .expect("user agent is a valid header value"),
            }),
        })
    }
}

/// An explicit provider, so the connector never depends on the process-wide
/// rustls default when several crypto backends are linked. No ALPN: the
/// gateway's tsnet `ListenTLS` listener is HTTP/1.1 only. Copied from
/// `setec::tls_connector`.
fn tls_connector() -> Result<TlsConnector, Error> {
    let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| Error::Config(format!("building the TLS connector: {e}")))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(config)))
}

/// Whether a `try_send_request` failure may be retried: only when hyper
/// recovered the request (its documented "never fully sent" guarantee), which
/// is exactly `message().is_some()`.
///
/// The wire-level middle case (a queue reject on a live connection) cannot be
/// produced with one-request-per-connection — hyper rejects only when the
/// dispatch channel is full or closed, and `TrySendError`'s fields are
/// `pub(crate)` in hyper, so it cannot be hand-built either. The real tests
/// bound both sides (`connect_timeout…` proves pre-queue failures retry,
/// `post_queue_reset_is_terminal` proves post-queue failures don't), and
/// hyper's documented `message()` contract covers the rest. Do not "add" a
/// fake test for the middle case.
fn should_retry_queue_reject(recovered: Option<&http::Request<Full<Bytes>>>) -> bool {
    recovered.is_some()
}

/// Aborts the spawned hyper connection driver when dropped, unless disarmed
/// (the streaming path hands the driver to the body stream's own guard).
struct AbortOnDrop(Option<Driver>);

impl AbortOnDrop {
    fn disarm(mut self) -> Driver {
        self.0.take().expect("driver present until disarmed")
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(driver) = self.0.take() {
            driver.abort();
        }
    }
}

/// The model name in a JSON request body, for the tracing span. The model
/// string only — never any other body content.
fn request_model(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()?
        .get("model")?
        .as_str()
        .map(str::to_owned)
}

/// `prompt_tokens`/`completion_tokens` from a buffered chat-completions
/// reply, for the tracing span.
fn response_usage(body: &[u8]) -> Option<(u64, u64)> {
    let value = serde_json::from_slice::<serde_json::Value>(body).ok()?;
    usage_from(value.get("usage")?)
}

/// Rebuilds the outgoing request for one attempt: rig's method and URI
/// (origin form: path and query verbatim), headers minus every key-shaped
/// one (the process-boundary backstop), explicit `host`, and the fixed
/// `user-agent`. hyper derives `content-length` from the body.
fn build_request(
    inner: &Inner,
    parts: &http::request::Parts,
    body: Bytes,
) -> Result<http::Request<Full<Bytes>>, HttpError> {
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str().to_owned())
        .unwrap_or_else(|| "/".to_owned());
    let mut builder = http::Request::builder()
        .method(parts.method.clone())
        .uri(path_and_query)
        .version(parts.version);
    for (name, value) in parts.headers.iter() {
        if STRIPPED_HEADERS.contains(&name.as_str()) {
            continue;
        }
        builder = builder.header(name.clone(), value.clone());
    }
    builder = builder
        .header(http::header::HOST, &inner.host_header)
        .header(http::header::USER_AGENT, &inner.user_agent);
    builder.body(Full::new(body)).map_err(HttpError::from)
}

/// Dial + TLS + HTTP/1 handshake + driver spawn, bounded by the connect
/// timeout. Nothing has been sent when this fails, so failures here are
/// retryable [`TransportError::Connect*`]s.
async fn connect(
    inner: &Inner,
) -> Result<
    (
        hyper::client::conn::http1::SendRequest<Full<Bytes>>,
        AbortOnDrop,
    ),
    TransportError,
> {
    let timeout = inner.connect_timeout;
    match tokio::time::timeout(timeout, connect_phase(inner)).await {
        Ok(Ok(pair)) => Ok(pair),
        Ok(Err(transport_error)) => Err(transport_error),
        Err(_elapsed) => Err(TransportError::ConnectTimeout(timeout)),
    }
}

async fn connect_phase(
    inner: &Inner,
) -> Result<
    (
        hyper::client::conn::http1::SendRequest<Full<Bytes>>,
        AbortOnDrop,
    ),
    TransportError,
> {
    let conn = inner
        .dialer
        .dial(&inner.host, inner.port)
        .await
        .map_err(|source| TransportError::Connect { source })?;
    let conn: Box<dyn setec::Connection> = match &inner.tls {
        Some(tls) => {
            let name =
                rustls::pki_types::ServerName::try_from(inner.host.clone()).map_err(|e| {
                    TransportError::Connect {
                        source: std::io::Error::other(e),
                    }
                })?;
            Box::new(
                tls.connect(name, conn)
                    .await
                    .map_err(|source| TransportError::Connect { source })?,
            )
        }
        None => conn,
    };
    let (sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(conn))
        .await
        .map_err(|e| TransportError::Connect {
            source: std::io::Error::other(e),
        })?;
    Ok((sender, AbortOnDrop(Some(tokio::spawn(connection)))))
}

/// Reads a non-success reply's body (bounded by the deadline, capped) and its
/// seconds-form `Retry-After`, for status classification.
async fn read_error_body(
    response: http::Response<Incoming>,
    deadline: tokio::time::Instant,
) -> (String, http::HeaderMap, Option<Duration>) {
    let (parts, body) = response.into_parts();
    let retry_after = parts
        .headers
        .get(http::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    let bytes = match tokio::time::timeout_at(deadline, body.collect()).await {
        Ok(Ok(collected)) => collected.to_bytes(),
        Ok(Err(_)) | Err(_) => Bytes::new(),
    };
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    text.truncate(MAX_ERROR_BODY);
    (text, parts.headers, retry_after)
}

/// The gateway body for a streamed 2xx reply, as rig's `BoxedStream`: each
/// frame bounded by the idle timeout; a timeout ends the stream with
/// [`TransportError::Timeout`]; dropping the stream aborts the connection
/// driver, which cancels the request.
fn into_boxed_stream(
    response: http::Response<Incoming>,
    driver: Driver,
    idle_timeout: Duration,
    span: tracing::Span,
) -> StreamingResponse {
    struct StreamGuard(Option<Driver>);
    impl Drop for StreamGuard {
        fn drop(&mut self) {
            if let Some(driver) = self.0.take() {
                driver.abort();
            }
        }
    }

    struct StreamState {
        body: http_body_util::BodyStream<Incoming>,
        _guard: StreamGuard,
        scanner: UsageScanner,
        span: tracing::Span,
        ended: bool,
    }

    fn next_item(
        mut state: StreamState,
        idle_timeout: Duration,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<(HttpResult<Bytes>, StreamState)>> + Send>,
    > {
        Box::pin(async move {
            loop {
                if state.ended {
                    return None;
                }
                match tokio::time::timeout(idle_timeout, state.body.next()).await {
                    Ok(Some(Ok(frame))) => match frame.into_data() {
                        Ok(bytes) => {
                            if let Some((input, output)) = state.scanner.scan(&bytes) {
                                state.span.record("input_tokens", input);
                                state.span.record("output_tokens", output);
                            }
                            return Some((Ok(bytes), state));
                        }
                        // Trailers and other non-data frames never reach rig.
                        Err(_frame) => continue,
                    },
                    Ok(Some(Err(error))) => {
                        state.ended = true;
                        return Some((Err(HttpError::instance(error)), state));
                    }
                    Ok(None) => return None,
                    Err(_elapsed) => {
                        state.ended = true;
                        let error = TransportError::Timeout(idle_timeout);
                        return Some((Err(HttpError::instance(error)), state));
                    }
                }
            }
        })
    }

    let (parts, body) = response.into_parts();
    let state = StreamState {
        body: http_body_util::BodyStream::new(body),
        _guard: StreamGuard(Some(driver)),
        scanner: UsageScanner::default(),
        span,
        ended: false,
    };
    let boxed: BoxedStream = Box::pin(futures::stream::unfold(state, move |state| {
        next_item(state, idle_timeout)
    }));
    http::Response::from_parts(parts, boxed)
}

/// Scans SSE-ish byte chunks for a chat-completions usage object, for the
/// tracing span only. Never blocks the stream: incomplete lines buffer until
/// a newline arrives; the buffer resets past 64 KiB so a pathological stream
/// cannot grow it without bound.
#[derive(Default)]
struct UsageScanner {
    buf: Vec<u8>,
}

impl UsageScanner {
    fn scan(&mut self, chunk: &[u8]) -> Option<(u64, u64)> {
        self.buf.extend_from_slice(chunk);
        let mut usage = None;
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim();
            let Some(payload) = line.strip_prefix("data: ") else {
                continue;
            };
            if payload == "[DONE]" {
                continue;
            }
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(payload)
                && let Some(found) = value.get("usage").and_then(usage_from)
            {
                usage = Some(found);
            }
        }
        if self.buf.len() > MAX_ERROR_BODY {
            self.buf.clear();
        }
        usage
    }
}

fn usage_from(usage: &serde_json::Value) -> Option<(u64, u64)> {
    Some((
        usage.get("prompt_tokens")?.as_u64()?,
        usage.get("completion_tokens")?.as_u64()?,
    ))
}

/// Executes one request attempt to a buffered 2xx reply. The staged retry
/// loop lands with the retry policy; this is the connection core it drives.
async fn execute(
    inner: Arc<Inner>,
    parts: http::request::Parts,
    body: Bytes,
) -> Result<http::Response<Bytes>, HttpError> {
    let deadline = tokio::time::Instant::now() + inner.request_timeout;
    let (mut sender, driver) = connect(&inner).await.map_err(HttpError::instance)?;
    let request = build_request(&inner, &parts, body)?;

    // try_send_request's future resolves with the response head, a protocol
    // error after the request was queued (message() is None), or a queue
    // reject with the request recovered (message() is Some) — the one
    // post-handshake failure the retry loop may retry.
    let response = match tokio::time::timeout_at(deadline, sender.try_send_request(request)).await {
        Ok(Ok(response)) => response,
        Ok(Err(e)) => {
            let recovered = e.message().is_some();
            let error = HttpError::instance(std::io::Error::other(e.into_error()));
            drop(sender);
            if recovered {
                // Queue reject: retryable once the loop lands. Single attempt
                // here.
                return Err(error);
            }
            // Queued, maybe written: terminal — the gateway may bill it.
            return Err(error);
        }
        Err(_elapsed) => {
            drop(sender);
            return Err(HttpError::instance(TransportError::Timeout(
                inner.request_timeout,
            )));
        }
    };

    let status = response.status();
    if !status.is_success() {
        let (text, headers, _retry_after) = read_error_body(response, deadline).await;
        drop(sender);
        return Err(HttpError::non_success_with_details(status, headers, text));
    }

    let (response_parts, incoming) = response.into_parts();
    let bytes = match tokio::time::timeout_at(deadline, incoming.collect()).await {
        Ok(Ok(collected)) => collected.to_bytes(),
        Ok(Err(e)) => {
            drop(sender);
            return Err(HttpError::instance(e));
        }
        Err(_elapsed) => {
            drop(sender);
            return Err(HttpError::instance(TransportError::Timeout(
                inner.request_timeout,
            )));
        }
    };
    drop(sender);
    drop(driver);
    Ok(http::Response::from_parts(response_parts, bytes))
}

/// Executes one request attempt to a 2xx response head, leaving the body
/// streaming under the idle timeout. Dropping the returned stream cancels
/// the request (the connection driver is aborted).
async fn execute_streaming(
    inner: Arc<Inner>,
    parts: http::request::Parts,
    body: Bytes,
) -> Result<StreamingResponse, HttpError> {
    let deadline = tokio::time::Instant::now() + inner.request_timeout;
    let (mut sender, driver) = connect(&inner).await.map_err(HttpError::instance)?;
    let request = build_request(&inner, &parts, body)?;

    let response = match tokio::time::timeout_at(deadline, sender.try_send_request(request)).await {
        Ok(Ok(response)) => response,
        Ok(Err(e)) => {
            let error = HttpError::instance(std::io::Error::other(e.into_error()));
            drop(sender);
            return Err(error);
        }
        Err(_elapsed) => {
            drop(sender);
            return Err(HttpError::instance(TransportError::Timeout(
                inner.request_timeout,
            )));
        }
    };

    let status = response.status();
    if !status.is_success() {
        let (text, headers, _retry_after) = read_error_body(response, deadline).await;
        drop(sender);
        return Err(HttpError::non_success_with_details(status, headers, text));
    }

    drop(sender);
    Ok(into_boxed_stream(
        response,
        driver.disarm(),
        inner.stream_idle_timeout,
        tracing::Span::none(),
    ))
}

impl HttpClientExt for GatewayHttp {
    /// Sends a unary request: the attempt loop runs to a buffered 2xx body,
    /// then the lazy body resolves immediately to `U::from(bytes)`.
    fn send<T, U>(
        &self,
        req: http::Request<T>,
    ) -> impl Future<Output = HttpResult<http::Response<LazyBody<U>>>> + Send + 'static
    where
        T: Into<Bytes> + Send,
        U: From<Bytes> + Send + 'static,
    {
        let inner = self.inner.clone();
        let (parts, body) = req.into_parts();
        let body = body.into();
        async move {
            let response = execute(inner, parts, body).await?;
            Ok(response.map(|bytes| {
                let lazy: LazyBody<U> = Box::pin(async move { Ok(U::from(bytes)) });
                lazy
            }))
        }
    }

    /// Sends a multipart request through the same unary path. cja-llm never
    /// sends multipart; the method satisfies the trait.
    fn send_multipart<U>(
        &self,
        req: http::Request<MultipartForm>,
    ) -> impl Future<Output = HttpResult<http::Response<LazyBody<U>>>> + Send + 'static
    where
        U: From<Bytes> + Send + 'static,
    {
        let (mut parts, form) = req.into_parts();
        let (content_type, body) = form.encode();
        if let Ok(value) = http::HeaderValue::from_str(&content_type) {
            parts.headers.insert(http::header::CONTENT_TYPE, value);
        }
        let inner = self.inner.clone();
        async move {
            let response = execute(inner, parts, body).await?;
            Ok(response.map(|bytes| {
                let lazy: LazyBody<U> = Box::pin(async move { Ok(U::from(bytes)) });
                lazy
            }))
        }
    }

    /// Sends a streaming request: the attempt loop runs only to the 2xx
    /// response head; the body then streams frame by frame under the idle
    /// timeout. Dropping the returned stream cancels the request.
    fn send_streaming<T>(
        &self,
        req: http::Request<T>,
    ) -> impl Future<Output = HttpResult<StreamingResponse>> + Send
    where
        T: Into<Bytes> + Send,
    {
        let inner = self.inner.clone();
        let (parts, body) = req.into_parts();
        let body = body.into();
        async move { execute_streaming(inner, parts, body).await }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_reject_decision_is_pure() {
        let request = http::Request::post("/v1/chat/completions")
            .body(Full::new(Bytes::new()))
            .unwrap();
        // See the comment on should_retry_queue_reject: the middle case is
        // bounded by the real transport tests plus hyper's documented
        // message() contract, not by a hand-built fake.
        assert!(should_retry_queue_reject(Some(&request)));
        assert!(!should_retry_queue_reject(None));
    }

    #[test]
    fn usage_extraction_reads_only_the_usage_object() {
        let body = serde_json::json!({
            "choices": [{"message": {"content": "secret-ish text"}}],
            "usage": {"prompt_tokens": 11, "completion_tokens": 7}
        });
        assert_eq!(response_usage(body.to_string().as_bytes()), Some((11, 7)));
        assert_eq!(response_usage(b"not json"), None);
        assert_eq!(
            response_usage(serde_json::json!({"usage": {}}).to_string().as_bytes()),
            None
        );
    }

    #[test]
    fn request_model_extracts_the_model_string() {
        let body = serde_json::json!({"model": "anthropic/claude-haiku-5-5", "messages": []});
        assert_eq!(
            request_model(body.to_string().as_bytes()).as_deref(),
            Some("anthropic/claude-haiku-5-5")
        );
        assert_eq!(request_model(b"multipart-bytes"), None);
    }

    #[test]
    fn scanner_finds_usage_across_chunk_splits() {
        let mut scanner = UsageScanner::default();
        let chunk = b"data: {\"choices\":[]}\n\ndata: {\"usage\":{\"prompt_tok";
        assert_eq!(scanner.scan(chunk), None);
        let rest = b"ens\":3,\"completion_tokens\":2}}\n\ndata: [DONE]\n\n";
        assert_eq!(scanner.scan(rest), Some((3, 2)));
    }
}
