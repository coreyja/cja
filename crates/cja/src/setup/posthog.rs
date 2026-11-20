use std::{collections::HashMap, fmt, io::Read as _, time::Duration};

use color_eyre::eyre::{Result, WrapErr as _, ensure, eyre};
use opentelemetry::KeyValue;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_appender_tracing::layer::{OpenTelemetryTracingBridge, TracingSpanAttributes};
use opentelemetry_http::{Bytes, HttpClient, HttpError, Request, Response, ResponseBodyTooLarge};
use opentelemetry_otlp::{
    LogExporter, Protocol, SpanExporter, WithExportConfig as _, WithHttpConfig as _,
};
use opentelemetry_proto::tonic::collector::{
    logs::v1::ExportLogsServiceRequest, trace::v1::ExportTraceServiceRequest,
};
use opentelemetry_proto::tonic::common::v1::{
    AnyValue, KeyValue as ProtoKeyValue, any_value::Value,
};
use opentelemetry_sdk::{
    Resource,
    logs::{self, BatchLogProcessor, SdkLogger, SdkLoggerProvider},
    trace::{self, BatchSpanProcessor, Sampler, SdkTracer, SdkTracerProvider},
};
use prost::Message as _;
use tracing_subscriber::filter::{LevelFilter, Targets};

/// Per-request bound on one OTLP export (client and exporter).
const TIMEOUT: Duration = Duration::from_secs(5);
/// Records each batch queue holds before it drops (and counts) on overflow.
pub(super) const MAX_QUEUE_SIZE: usize = 8192;
/// Records per export request; clamped to the queue size in [`PostHogSink::build`].
const MAX_EXPORT_BATCH_SIZE: usize = 512;
/// How often the log worker exports a partial batch.
const LOGS_SCHEDULED_DELAY: Duration = Duration::from_secs(1);
/// How often the span worker exports a partial batch.
const TRACES_SCHEDULED_DELAY: Duration = Duration::from_secs(5);
/// How long each provider's final flush may take. Both providers flush in
/// parallel, so this is also the whole sink's flush time.
///
/// Invariant: `PROVIDER_SHUTDOWN_TIMEOUT` < [`SHUTDOWN_WORKER_TIMEOUT`] <=
/// [`super::TELEMETRY_SHUTDOWN_BUDGET`]. The inner bound has to fire first so
/// the SDK reports its own timeout and the blocking worker is never abandoned
/// mid-flush. It is deliberately shorter than [`TIMEOUT`]: a request stalled
/// at shutdown is abandoned so the flush fits the platform kill window.
const PROVIDER_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(2000);
/// Backstop on the blocking task that runs both provider flushes.
const SHUTDOWN_WORKER_TIMEOUT: Duration = Duration::from_millis(2500);
const _: () = assert!(PROVIDER_SHUTDOWN_TIMEOUT.as_millis() < SHUTDOWN_WORKER_TIMEOUT.as_millis());
const _: () =
    assert!(SHUTDOWN_WORKER_TIMEOUT.as_millis() <= super::TELEMETRY_SHUTDOWN_BUDGET.as_millis());

pub(super) enum PostHogEnv {
    Enabled(PostHogConfig),
    Skipped { host_set_without_token: bool },
}

/// Capture credentials are independent of OTLP filtering knobs.
pub(crate) struct PostHogCredentials {
    pub(crate) host: String,
    pub(crate) token: String,
}

impl PostHogCredentials {
    #[cfg(feature = "jobs")]
    pub(crate) fn from_env() -> Result<Option<Self>> {
        Self::from_env_with(|key| std::env::var(key).ok())
    }

    fn from_env_with(read: impl Fn(&str) -> Option<String>) -> Result<Option<Self>> {
        let Some(token) = read("POSTHOG_PROJECT_TOKEN").filter(|t| !t.trim().is_empty()) else {
            return Ok(None);
        };
        let host = read("POSTHOG_HOST").unwrap_or_else(|| "https://us.i.posthog.com".into());
        let url = url::Url::parse(host.trim()).map_err(|_| eyre!("invalid POSTHOG_HOST"))?;
        ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.host().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "POSTHOG_HOST must be an HTTP(S) base URL without credentials, query, or fragment"
        );
        http::HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| eyre!("invalid POSTHOG_PROJECT_TOKEN"))?;
        Ok(Some(Self {
            host: url.as_str().trim_end_matches('/').to_owned(),
            token,
        }))
    }
}

impl PostHogEnv {
    pub(super) fn from_env() -> Result<Self> {
        Self::from_env_with(|key| std::env::var(key).ok())
    }

    fn from_env_with(read: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let Some(credentials) = PostHogCredentials::from_env_with(&read)? else {
            return Ok(Self::Skipped {
                host_set_without_token: read("POSTHOG_HOST").is_some(),
            });
        };
        let log_level = read("POSTHOG_LOG_LEVEL")
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.trim().parse())
            .transpose()
            .wrap_err("invalid POSTHOG_LOG_LEVEL")?
            .unwrap_or(LevelFilter::INFO);
        let ratio = read("POSTHOG_TRACES_SAMPLE_RATIO")
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.trim().parse::<f64>())
            .transpose()
            .wrap_err("invalid POSTHOG_TRACES_SAMPLE_RATIO")?
            .unwrap_or(1.0);
        ensure!(
            (0.0..=1.0).contains(&ratio),
            "invalid POSTHOG_TRACES_SAMPLE_RATIO"
        );
        Ok(Self::Enabled(PostHogConfig {
            host: credentials.host,
            token: credentials.token,
            log_level,
            traces_sample_ratio: ratio,
        }))
    }
}

pub struct PostHogConfig {
    pub(super) host: String,
    pub(super) token: String,
    pub(super) log_level: LevelFilter,
    pub(super) traces_sample_ratio: f64,
}

impl fmt::Debug for PostHogConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PostHogConfig")
    }
}

impl PostHogConfig {
    pub fn new_for_tests(
        host: impl Into<String>,
        token: impl Into<String>,
        log_level: LevelFilter,
        traces_sample_ratio: f64,
    ) -> Self {
        Self {
            host: host.into(),
            token: token.into(),
            log_level,
            traces_sample_ratio,
        }
    }
}

fn posthog_filter(level: LevelFilter) -> Targets {
    // Prevent exporter feedback loops, including reqwest's I/O thread.
    // https://posthog.com/docs/logs/installation/rust
    Targets::new().with_default(level).with_targets(
        [
            "opentelemetry",
            "tracing_opentelemetry",
            "hyper",
            "hyper_util",
            "h2",
            "reqwest",
            "rustls",
            "want",
            "mio",
            "posthog",
        ]
        .map(|t| (t, LevelFilter::OFF)),
    )
}

fn posthog_resource(
    name: &str,
    version: Option<&str>,
    process: Option<&eyes_subscriber::ProcessIdentity>,
) -> Resource {
    let mut resource = Resource::builder_empty().with_service_name(name.to_owned());
    if let Some(v) = version {
        resource = resource.with_attribute(KeyValue::new("service.version", v.to_owned()));
    }
    if let Some(p) = process {
        resource = resource.with_attribute(KeyValue::new(
            "service.instance.id",
            p.instance_id().to_string(),
        ));
    }
    resource.build()
}

fn build_on_sync_thread<T: Send>(build: impl FnOnce() -> Result<T> + Send) -> Result<T> {
    std::thread::scope(|scope| scope.spawn(build).join())
        .map_err(|_| eyre!("PostHog exporter construction panicked"))?
}

pub struct PostHogSink {
    providers: PostHogShutdownHandle,
    tracer: SdkTracer,
    filter: Targets,
}

/// Largest response body read back from `PostHog`, as in `opentelemetry-http`.
const MAX_RESPONSE_BODY_BYTES: u64 = 4 * 1024 * 1024;

/// The exporters' HTTP client: reqwest 0.12's blocking client over rustls
/// with ring and the platform verifier (system roots).
///
/// It carries its own ring `ClientConfig`, so building it installs no
/// process-wide crypto provider and leaves the app's TLS setup alone. Why
/// reqwest 0.12 and not the workspace's 0.13: see `reqwest_012` in
/// `crates/cja/Cargo.toml`.
#[derive(Clone)]
struct PostHogHttpClient {
    client: reqwest_012::blocking::Client,
    authorization: http::HeaderValue,
}

impl fmt::Debug for PostHogHttpClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PostHogHttpClient")
    }
}

impl PostHogHttpClient {
    fn new(token: &str) -> Result<Self> {
        let tls = crate::tls::ring_client_config()?;
        let client = reqwest_012::blocking::Client::builder()
            .use_preconfigured_tls(tls)
            .timeout(TIMEOUT)
            .redirect(reqwest_012::redirect::Policy::none())
            .build()
            .wrap_err("building the HTTP client")?;
        let mut authorization = http::HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| eyre!("invalid POSTHOG_PROJECT_TOKEN"))?;
        authorization.set_sensitive(true);
        Ok(Self {
            client,
            authorization,
        })
    }
}

fn redact_attribute(key: &str, value: &mut String) -> bool {
    let header = key
        .rsplit('.')
        .next()
        .unwrap_or(key)
        .to_ascii_lowercase()
        .replace('-', "_");
    if matches!(
        header.as_str(),
        "authorization" | "proxy_authorization" | "cookie" | "set_cookie"
    ) {
        return false;
    }
    let query = match key.to_ascii_lowercase().as_str() {
        "uri" | "http.request.header.referer" => {
            if let Some((prefix, query)) = value.split_once('?') {
                Some((format!("{prefix}?"), query.to_owned()))
            } else {
                None
            }
        }
        "url.query" => Some((String::new(), value.clone())),
        _ => None,
    };
    if let Some((prefix, query)) = query {
        let (query_text, fragment) = query
            .split_once('#')
            .map_or((query.as_str(), ""), |(q, f)| (q, f));
        let redacted = redact_query(query_text);
        *value = if redacted == "[REDACTED]" || fragment.is_empty() {
            format!("{prefix}{redacted}")
        } else {
            format!("{prefix}{redacted}#{fragment}")
        };
    }
    true
}

fn redact_query(query: &str) -> String {
    let mut parts = Vec::new();
    for segment in query.split('&') {
        let (key, value) = segment.split_once('=').unwrap_or((segment, ""));
        if ![key, value].iter().all(|part| valid_percent_encoding(part)) {
            return "[REDACTED]".into();
        }
        let form_key = key.replace('+', " ");
        let Ok(decoded) = percent_encoding::percent_decode_str(&form_key).decode_utf8() else {
            return "[REDACTED]".into();
        };
        if percent_encoding::percent_decode_str(&value.replace('+', " "))
            .decode_utf8()
            .is_err()
        {
            return "[REDACTED]".into();
        }
        let key_lower = decoded.to_ascii_lowercase();
        let sensitive = matches!(
            key_lower.as_str(),
            "code"
                | "state"
                | "token"
                | "access_token"
                | "id_token"
                | "refresh_token"
                | "code_verifier"
                | "password"
                | "secret"
                | "client_secret"
                | "key"
                | "api_key"
                | "apikey"
                | "signature"
                | "sig"
                | "otp"
        ) || ["token", "secret", "password", "passwd", "signature"]
            .iter()
            .any(|part| key_lower.contains(part));
        if sensitive {
            parts.push(format!("{key}=[REDACTED]"));
        } else {
            parts.push(segment.to_owned());
        }
    }
    parts.join("&")
}

fn valid_percent_encoding(part: &str) -> bool {
    let mut bytes = part.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%'
            && !matches!((bytes.next(), bytes.next()), (Some(a), Some(b)) if a.is_ascii_hexdigit() && b.is_ascii_hexdigit())
        {
            return false;
        }
    }
    true
}

fn sanitize_attributes(attributes: &mut Vec<ProtoKeyValue>) {
    attributes.retain_mut(|attribute| {
        let header = attribute
            .key
            .rsplit('.')
            .next()
            .unwrap_or(&attribute.key)
            .to_ascii_lowercase()
            .replace('-', "_");
        if matches!(
            header.as_str(),
            "authorization" | "proxy_authorization" | "cookie" | "set_cookie"
        ) {
            return false;
        }
        let Some(AnyValue {
            value: Some(Value::StringValue(value)),
        }) = attribute.value.as_mut()
        else {
            return true;
        };
        redact_attribute(&attribute.key, value)
    });
}

fn sanitize_body(path: &str, body: &[u8]) -> Result<Vec<u8>> {
    if path.ends_with("/i/v1/logs") {
        let mut request =
            ExportLogsServiceRequest::decode(body).wrap_err("invalid OTLP logs batch")?;
        for resource in &mut request.resource_logs {
            for scope in &mut resource.scope_logs {
                for record in &mut scope.log_records {
                    sanitize_attributes(&mut record.attributes);
                }
            }
        }
        Ok(request.encode_to_vec())
    } else if path.ends_with("/i/v1/traces") {
        let mut request =
            ExportTraceServiceRequest::decode(body).wrap_err("invalid OTLP traces batch")?;
        for resource in &mut request.resource_spans {
            for scope in &mut resource.scope_spans {
                for span in &mut scope.spans {
                    sanitize_attributes(&mut span.attributes);
                    for event in &mut span.events {
                        sanitize_attributes(&mut event.attributes);
                    }
                    for link in &mut span.links {
                        sanitize_attributes(&mut link.attributes);
                    }
                }
            }
        }
        Ok(request.encode_to_vec())
    } else {
        Err(eyre!("unexpected PostHog export path"))
    }
}

#[async_trait::async_trait]
impl HttpClient for PostHogHttpClient {
    async fn send_bytes(&self, request: Request<Bytes>) -> Result<Response<Bytes>, HttpError> {
        let path = request.uri().path().to_owned();
        let body = sanitize_body(&path, request.body()).map_err(|error| {
            tracing::warn!(target: "posthog", "PostHog export batch rejected: {error:#}");
            std::io::Error::other("PostHog export batch rejected")
        })?;
        let mut outgoing: reqwest_012::blocking::Request = request.try_into()?;
        *outgoing.body_mut() = Some(body.into());
        outgoing
            .headers_mut()
            .remove(http::header::CONTENT_ENCODING);
        outgoing.headers_mut().remove(http::header::CONTENT_LENGTH);
        outgoing
            .headers_mut()
            .insert(http::header::AUTHORIZATION, self.authorization.clone());
        let mut response = self.client.execute(outgoing)?;
        if response.status().is_redirection() {
            tracing::warn!(target: "posthog", "PostHog export redirect rejected");
            return Err(std::io::Error::other("PostHog export redirect rejected").into());
        }
        let status = response.status();
        let headers = std::mem::take(response.headers_mut());
        let mut body = Vec::new();
        response
            .take(MAX_RESPONSE_BODY_BYTES + 1)
            .read_to_end(&mut body)?;
        if body.len() as u64 > MAX_RESPONSE_BODY_BYTES {
            return Err(Box::new(ResponseBodyTooLarge::new()));
        }
        let mut http_response = Response::builder().status(status).body(Bytes::from(body))?;
        *http_response.headers_mut() = headers;
        Ok(http_response)
    }
}

impl PostHogSink {
    pub fn build(
        config: &PostHogConfig,
        crate_name: &str,
        version: Option<&str>,
        process: Option<&eyes_subscriber::ProcessIdentity>,
        max_queue_size: usize,
    ) -> Result<Self> {
        for key in [
            "OTEL_EXPORTER_OTLP_HEADERS",
            "OTEL_EXPORTER_OTLP_LOGS_HEADERS",
            "OTEL_EXPORTER_OTLP_TRACES_HEADERS",
            "OTEL_EXPORTER_OTLP_COMPRESSION",
            "OTEL_EXPORTER_OTLP_LOGS_COMPRESSION",
            "OTEL_EXPORTER_OTLP_TRACES_COMPRESSION",
        ] {
            if std::env::var_os(key).is_some_and(|value| !value.to_string_lossy().trim().is_empty())
            {
                return Err(eyre!("PostHog exporter rejects {key}"));
            }
        }
        let (logs, spans) = build_on_sync_thread(|| {
            // The async client can panic on the batch worker. Construct and drop blocking clients outside Tokio.
            // https://posthog.com/docs/logs/installation/rust
            let client = PostHogHttpClient::new(&config.token)
                .wrap_err("building PostHog exporter or HTTP client")?;
            let headers = HashMap::from([(
                "Authorization".to_owned(),
                format!("Bearer {}", config.token),
            )]);
            let logs = LogExporter::builder()
                .with_http()
                .with_endpoint(format!("{}/i/v1/logs", config.host))
                .with_protocol(Protocol::HttpBinary)
                .with_timeout(TIMEOUT)
                .with_http_client(client.clone())
                .with_headers(headers.clone())
                .build()
                .wrap_err("building PostHog exporter or HTTP client")?;
            let spans = SpanExporter::builder()
                .with_http()
                .with_endpoint(format!("{}/i/v1/traces", config.host))
                .with_protocol(Protocol::HttpBinary)
                .with_timeout(TIMEOUT)
                .with_http_client(client)
                .with_headers(headers)
                .build()
                .wrap_err("building PostHog exporter or HTTP client")?;
            Ok((logs, spans))
        })?;
        let resource = posthog_resource(crate_name, version, process);
        let logs = BatchLogProcessor::builder(logs)
            .with_batch_config(
                logs::BatchConfigBuilder::default()
                    .with_max_queue_size(max_queue_size)
                    .with_max_export_batch_size(MAX_EXPORT_BATCH_SIZE.min(max_queue_size))
                    .with_scheduled_delay(LOGS_SCHEDULED_DELAY)
                    .build(),
            )
            .build();
        let spans = BatchSpanProcessor::builder(spans)
            .with_batch_config(
                trace::BatchConfigBuilder::default()
                    .with_max_queue_size(max_queue_size)
                    .with_max_export_batch_size(MAX_EXPORT_BATCH_SIZE.min(max_queue_size))
                    .with_scheduled_delay(TRACES_SCHEDULED_DELAY)
                    .build(),
            )
            .build();
        let logs = SdkLoggerProvider::builder()
            .with_resource(resource.clone())
            .with_log_processor(logs)
            .build();
        let spans = SdkTracerProvider::builder()
            .with_resource(resource)
            .with_sampler(Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(
                config.traces_sample_ratio,
            ))))
            .with_span_processor(spans)
            .build();
        Ok(Self {
            tracer: spans.tracer(crate_name.to_owned()),
            providers: PostHogShutdownHandle { logs, spans },
            filter: posthog_filter(config.log_level),
        })
    }
    pub fn log_bridge(&self) -> OpenTelemetryTracingBridge<SdkLoggerProvider, SdkLogger> {
        OpenTelemetryTracingBridge::builder(&self.providers.logs)
            .with_tracing_span_attributes(TracingSpanAttributes::all())
            .build()
    }
    pub fn tracer(&self) -> SdkTracer {
        self.tracer.clone()
    }
    pub fn filter(&self) -> Targets {
        self.filter.clone()
    }
    pub fn shutdown_handle(&self) -> PostHogShutdownHandle {
        self.providers.clone()
    }
}

#[derive(Clone)]
pub struct PostHogShutdownHandle {
    logs: SdkLoggerProvider,
    spans: SdkTracerProvider,
}
impl fmt::Debug for PostHogShutdownHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PostHogShutdownHandle")
    }
}
impl PostHogShutdownHandle {
    /// Flushes both providers in parallel, each bounded by
    /// [`PROVIDER_SHUTDOWN_TIMEOUT`]. Failures are logged, never returned.
    pub async fn shutdown(self) {
        let job = tokio::task::spawn_blocking(move || {
            std::thread::scope(|s| {
                let timeout = PROVIDER_SHUTDOWN_TIMEOUT;
                let logs = s.spawn(move || self.logs.shutdown_with_timeout(timeout));
                let spans = s.spawn(move || self.spans.shutdown_with_timeout(timeout));
                for result in [logs.join(), spans.join()] {
                    if !matches!(result, Ok(Ok(()))) {
                        tracing::warn!(target:"posthog", ?result, "PostHog shutdown failed");
                    }
                }
            });
        });
        let result = tokio::time::timeout(SHUTDOWN_WORKER_TIMEOUT, job).await;
        if !matches!(result, Ok(Ok(()))) {
            tracing::warn!(target:"posthog", ?result, "PostHog shutdown worker failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::{Layer, layer::Context, prelude::*};

    fn attribute(key: &str, value: &str) -> ProtoKeyValue {
        ProtoKeyValue {
            key: key.into(),
            value: Some(AnyValue {
                value: Some(Value::StringValue(value.into())),
            }),
            ..Default::default()
        }
    }

    fn value<'a>(attributes: &'a [ProtoKeyValue], key: &str) -> Option<&'a str> {
        attributes
            .iter()
            .find(|attribute| attribute.key == key)
            .and_then(
                |attribute| match attribute.value.as_ref()?.value.as_ref()? {
                    Value::StringValue(value) => Some(value.as_str()),
                    _ => None,
                },
            )
    }

    #[test]
    fn query_policy_covers_all_sensitive_keys_and_fail_closed() {
        for key in [
            "code",
            "state",
            "token",
            "access_token",
            "id_token",
            "refresh_token",
            "code_verifier",
            "password",
            "secret",
            "client_secret",
            "key",
            "api_key",
            "apikey",
            "signature",
            "sig",
            "otp",
            "magicToken",
            "mySecret",
            "passWordHint",
            "Passwd",
            "X-Amz-Signature",
            "%74oken",
        ] {
            let query = format!("{key}=synthetic&page=2");
            assert_eq!(redact_query(&query), format!("{key}=[REDACTED]&page=2"));
            assert_eq!(redact_query(&redact_query(&query)), redact_query(&query));
        }
        for query in [
            "code=%GG&page=2",
            "code=%FF&page=2",
            "page=%GG&code=x",
            "page=%FF",
        ] {
            assert_eq!(redact_query(query), "[REDACTED]");
        }
        assert_eq!(redact_query("page=2&sort=asc"), "page=2&sort=asc");
        assert_eq!(redact_query("CODE=&page=2"), "CODE=[REDACTED]&page=2");
        for (key, raw, expected) in [
            (
                "uri",
                "/callback?code=synthetic&page=2",
                "/callback?code=[REDACTED]&page=2",
            ),
            (
                "url.query",
                "code=synthetic&page=2",
                "code=[REDACTED]&page=2",
            ),
            (
                "http.request.header.referer",
                "https://local/callback?code=synthetic&page=2",
                "https://local/callback?code=[REDACTED]&page=2",
            ),
            ("uri", "/callback?code=%GG", "/callback?[REDACTED]"),
            (
                "http.request.header.referer",
                "https://local/callback?code=%FF",
                "https://local/callback?[REDACTED]",
            ),
            (
                "http.request.header.referer",
                "https://local/callback?code=synthetic&page=2#section",
                "https://local/callback?code=[REDACTED]&page=2#section",
            ),
            ("uri", "/callback", "/callback"),
        ] {
            let mut actual = raw.to_owned();
            assert!(redact_attribute(key, &mut actual));
            assert_eq!(actual, expected);
        }
    }

    fn privacy_fields() -> Vec<ProtoKeyValue> {
        vec![
            attribute("uri", "/callback?code=synthetic&page=2"),
            attribute("url.query", "state=synthetic&page=2"),
            attribute(
                "http.request.header.referer",
                "https://local/?token=synthetic&page=2",
            ),
            attribute("http.request.header.Authorization", "synthetic"),
            attribute("http.request.header.proxy-authorization", "synthetic"),
            attribute("http.request.header.cookie", "synthetic"),
            attribute("http.request.header.set-cookie", "synthetic"),
            attribute("self", "Job { recipient_override: synthetic@example.test }"),
            attribute("user_agent.original", "test-agent"),
            attribute("http.request.header.x-forwarded-for", "127.0.0.1"),
            attribute("http.request.header.host", "local"),
            attribute("http.request.header.via", "local"),
            attribute("http.request.header.x-forwarded-proto", "https"),
            attribute("http.request.header.x-forwarded-ssl", "on"),
            attribute("http.request.header.fly-region", "ewr"),
            attribute("url.path", "/callback"),
            attribute("http.route", "/callback"),
            attribute("job.id", "job-1"),
            attribute("job.name", "Mail"),
            attribute("job.context", "test"),
            attribute("job.priority", "5"),
        ]
    }

    fn assert_request_credentials_redacted(attributes: &[ProtoKeyValue]) {
        assert_eq!(
            value(attributes, "uri"),
            Some("/callback?code=[REDACTED]&page=2")
        );
        assert_eq!(
            value(attributes, "url.query"),
            Some("state=[REDACTED]&page=2")
        );
        assert_eq!(
            value(attributes, "http.request.header.referer"),
            Some("https://local/?token=[REDACTED]&page=2")
        );
        for key in [
            "http.request.header.Authorization",
            "http.request.header.proxy-authorization",
            "http.request.header.cookie",
            "http.request.header.set-cookie",
        ] {
            assert!(value(attributes, key).is_none(), "{key}");
        }
    }

    #[test]
    fn otlp_policy_covers_spans_events_links_and_copied_logs() {
        use opentelemetry_proto::tonic::{
            logs::v1::{LogRecord, ResourceLogs, ScopeLogs},
            trace::v1::{
                ResourceSpans, ScopeSpans, Span,
                span::{Event, Link},
            },
        };
        let fields = privacy_fields();
        let traces = ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                scope_spans: vec![ScopeSpans {
                    spans: vec![Span {
                        name: "jobs.enqueue".into(),
                        attributes: fields.clone(),
                        events: vec![Event {
                            attributes: fields.clone(),
                            ..Default::default()
                        }],
                        links: vec![Link {
                            attributes: fields.clone(),
                            ..Default::default()
                        }],
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };
        let encoded = sanitize_body("/base/i/v1/traces", &traces.encode_to_vec()).unwrap();
        let decoded = ExportTraceServiceRequest::decode(encoded.as_slice()).unwrap();
        let span = &decoded.resource_spans[0].scope_spans[0].spans[0];
        for attributes in [
            &span.attributes,
            &span.events[0].attributes,
            &span.links[0].attributes,
        ] {
            assert_eq!(
                value(attributes, "self"),
                Some("Job { recipient_override: synthetic@example.test }")
            );
            assert_request_credentials_redacted(attributes);
            for key in [
                "user_agent.original",
                "http.request.header.x-forwarded-for",
                "http.request.header.host",
                "http.request.header.via",
                "http.request.header.x-forwarded-proto",
                "http.request.header.x-forwarded-ssl",
                "http.request.header.fly-region",
                "url.path",
                "http.route",
                "job.id",
                "job.name",
                "job.context",
                "job.priority",
            ] {
                assert!(value(attributes, key).is_some(), "{key}");
            }
        }
        let logs = ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                scope_logs: vec![ScopeLogs {
                    log_records: vec![LogRecord {
                        attributes: fields,
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };
        let encoded = sanitize_body("/base/i/v1/logs", &logs.encode_to_vec()).unwrap();
        let decoded = ExportLogsServiceRequest::decode(encoded.as_slice()).unwrap();
        let attributes = &decoded.resource_logs[0].scope_logs[0].log_records[0].attributes;
        assert_request_credentials_redacted(attributes);
        assert_eq!(
            value(attributes, "self"),
            Some("Job { recipient_override: synthetic@example.test }")
        );
        assert_eq!(value(attributes, "job.id"), Some("job-1"));
        assert!(sanitize_body("/other", &logs.encode_to_vec()).is_err());
        assert!(sanitize_body("/i/v1/traces", b"not protobuf").is_err());
    }

    #[test]
    fn job_run_value_is_kept_in_job_context() {
        use opentelemetry_proto::tonic::{
            logs::v1::{LogRecord, ResourceLogs, ScopeLogs},
            trace::v1::{
                ResourceSpans, ScopeSpans, Span,
                span::{Event, Link},
            },
        };
        let fields = vec![
            attribute("value", "synthetic-run@example.test"),
            attribute("job.name", "Mail"),
        ];
        let traces = ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                scope_spans: vec![ScopeSpans {
                    spans: vec![
                        Span {
                            name: "jobs.run_from_value".into(),
                            attributes: fields.clone(),
                            events: vec![Event {
                                attributes: fields.clone(),
                                ..Default::default()
                            }],
                            links: vec![Link {
                                attributes: fields.clone(),
                                ..Default::default()
                            }],
                            ..Default::default()
                        },
                        Span {
                            name: "unrelated".into(),
                            attributes: vec![attribute("value", "keep-me")],
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };
        let body = sanitize_body("/i/v1/traces", &traces.encode_to_vec()).unwrap();
        let decoded = ExportTraceServiceRequest::decode(body.as_slice()).unwrap();
        let span = &decoded.resource_spans[0].scope_spans[0].spans[0];
        for attrs in [
            &span.attributes,
            &span.events[0].attributes,
            &span.links[0].attributes,
        ] {
            assert_eq!(value(attrs, "value"), Some("synthetic-run@example.test"));
            assert_eq!(value(attrs, "job.name"), Some("Mail"));
        }
        let unrelated = &decoded.resource_spans[0].scope_spans[0].spans[1];
        assert_eq!(value(&unrelated.attributes, "value"), Some("keep-me"));
        let logs = ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                scope_logs: vec![ScopeLogs {
                    log_records: vec![
                        LogRecord {
                            attributes: {
                                let mut attributes = fields;
                                attributes
                                    .push(attribute("error.detail", "synthetic detail marker"));
                                attributes
                            },
                            ..Default::default()
                        },
                        LogRecord {
                            attributes: vec![attribute("value", "keep-me")],
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };
        let body = sanitize_body("/i/v1/logs", &logs.encode_to_vec()).unwrap();
        let decoded = ExportLogsServiceRequest::decode(body.as_slice()).unwrap();
        let records = &decoded.resource_logs[0].scope_logs[0].log_records;
        assert_eq!(
            value(&records[0].attributes, "value"),
            Some("synthetic-run@example.test")
        );
        assert_eq!(
            value(&records[0].attributes, "error.detail"),
            Some("synthetic detail marker")
        );
        assert_eq!(value(&records[0].attributes, "job.name"), Some("Mail"));
        assert_eq!(value(&records[1].attributes, "value"), Some("keep-me"));
    }

    struct Warnings(Arc<Mutex<Vec<String>>>);
    struct Message<'a>(&'a mut String);
    impl tracing::field::Visit for Message<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
            if field.name() == "message" {
                *self.0 = format!("{value:?}");
            }
        }
    }
    impl<S: tracing::Subscriber> Layer<S> for Warnings {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            if *event.metadata().level() == tracing::Level::WARN {
                let mut message = String::new();
                event.record(&mut Message(&mut message));
                self.0
                    .lock()
                    .unwrap()
                    .push(format!("{}:{message}", event.metadata().target()));
            }
        }
    }
    #[allow(clippy::too_many_lines)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http_client_pins_authorization_rejects_bad_batches_and_redirects() {
        use axum::response::IntoResponse as _;
        let captures = Arc::new(Mutex::new(Vec::<(String, http::HeaderMap)>::new()));
        let warnings = Arc::new(Mutex::new(Vec::new()));
        let seen = captures.clone();
        let app = axum::Router::new().fallback(move |uri: http::Uri, headers: http::HeaderMap| {
            let seen = seen.clone();
            async move {
                seen.lock().unwrap().push((uri.path().into(), headers));
                if uri.path().ends_with("/logs") {
                    (
                        http::StatusCode::FOUND,
                        [(http::header::LOCATION, "/followed")],
                    )
                        .into_response()
                } else {
                    http::StatusCode::OK.into_response()
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = std::thread::spawn(|| PostHogHttpClient::new("phc_test").unwrap())
            .join()
            .unwrap();
        assert!(!format!("{client:?}").contains("phc_test"));
        let send = |path: &'static str, body: Vec<u8>| {
            let client = client.clone();
            let uri = format!("{host}{path}");
            let warnings = warnings.clone();
            std::thread::spawn(move || {
                let request = Request::builder()
                    .uri(uri)
                    .header(http::header::AUTHORIZATION, "Bearer foreign")
                    .header(http::header::CONTENT_ENCODING, "gzip")
                    .body(Bytes::from(body))
                    .unwrap();
                tracing::subscriber::with_default(
                    tracing_subscriber::registry().with(Warnings(warnings)),
                    || {
                        futures::executor::block_on(client.send_bytes(request))
                            .map(|response| response.status())
                    },
                )
            })
            .join()
            .unwrap()
        };
        assert_eq!(
            send(
                "/base/i/v1/traces",
                ExportTraceServiceRequest::default().encode_to_vec()
            )
            .unwrap(),
            http::StatusCode::OK
        );
        assert!(
            send(
                "/base/i/v1/logs",
                ExportLogsServiceRequest::default().encode_to_vec()
            )
            .is_err()
        );
        assert!(send("/base/i/v1/traces", b"malformed-protobuf".to_vec()).is_err());
        assert!(
            send(
                "/unknown",
                ExportLogsServiceRequest::default().encode_to_vec()
            )
            .is_err()
        );
        let warnings = warnings.lock().unwrap();
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(
            warnings
                .iter()
                .all(|warning| warning.starts_with("posthog:")),
            "{warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("batch rejected")),
            "{warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("redirect rejected")),
            "{warnings:?}"
        );
        let requests = captures.lock().unwrap();
        assert_eq!(
            requests.len(),
            2,
            "redirect followed or invalid batch leaked"
        );
        assert_eq!(requests[0].0, "/base/i/v1/traces");
        assert_eq!(requests[1].0, "/base/i/v1/logs");
        for (_, headers) in requests.iter() {
            assert_eq!(headers[http::header::AUTHORIZATION], "Bearer phc_test");
            assert!(!headers.contains_key(http::header::CONTENT_ENCODING));
        }
        drop(requests);
        server.abort();
    }

    fn parse(vars: &[(&str, &str)]) -> Result<PostHogEnv> {
        PostHogEnv::from_env_with(|key| {
            vars.iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (*v).into())
        })
    }
    #[test]
    fn gating() {
        for blank in ["", "   "] {
            assert!(matches!(
                parse(&[
                    ("POSTHOG_PROJECT_TOKEN", blank),
                    ("POSTHOG_HOST", "host"),
                    ("POSTHOG_LOG_LEVEL", "invalid")
                ])
                .unwrap(),
                PostHogEnv::Skipped {
                    host_set_without_token: true
                }
            ));
        }
        assert!(matches!(
            parse(&[]).unwrap(),
            PostHogEnv::Skipped {
                host_set_without_token: false
            }
        ));
        assert!(matches!(
            parse(&[("POSTHOG_HOST", "host")]).unwrap(),
            PostHogEnv::Skipped {
                host_set_without_token: true
            }
        ));
        let token = ("POSTHOG_PROJECT_TOKEN", "sensitive");
        for host in [
            "broken",
            "ftp://local",
            "https://local?token=sensitive",
            "https://user:sensitive@local",
        ] {
            let error = parse(&[token, ("POSTHOG_HOST", host)]).err().unwrap();
            assert!(!error.to_string().contains("sensitive"));
        }
        assert!(parse(&[("POSTHOG_PROJECT_TOKEN", "bad\nheader")]).is_err());
        let PostHogEnv::Enabled(default) = parse(&[token]).unwrap() else {
            panic!()
        };
        assert_eq!(default.host, "https://us.i.posthog.com");
        assert_eq!(default.log_level, LevelFilter::INFO);
        assert_eq!(default.traces_sample_ratio.to_bits(), 1.0_f64.to_bits());
        assert!(!format!("{default:?}").contains("sensitive"));
        for blank in ["", "   "] {
            let PostHogEnv::Enabled(config) = parse(&[
                token,
                ("POSTHOG_LOG_LEVEL", blank),
                ("POSTHOG_TRACES_SAMPLE_RATIO", blank),
            ])
            .unwrap() else {
                panic!()
            };
            assert_eq!(config.log_level, LevelFilter::INFO);
            assert_eq!(config.traces_sample_ratio, 1.0);
        }
        let PostHogEnv::Enabled(trimmed) = parse(&[
            token,
            ("POSTHOG_LOG_LEVEL", " warn "),
            ("POSTHOG_TRACES_SAMPLE_RATIO", " 0.25 "),
        ])
        .unwrap() else {
            panic!()
        };
        assert_eq!(trimmed.log_level, LevelFilter::WARN);
        assert_eq!(trimmed.traces_sample_ratio, 0.25);
        for value in ["NaN", "1.01", "-0.01", "bad"] {
            assert!(parse(&[token, ("POSTHOG_TRACES_SAMPLE_RATIO", value)]).is_err());
        }
        assert!(parse(&[token, ("POSTHOG_LOG_LEVEL", "bad")]).is_err());
        for (key, value) in [
            ("POSTHOG_LOG_LEVEL", "bad"),
            ("POSTHOG_TRACES_SAMPLE_RATIO", "bad"),
        ] {
            let error = parse(&[token, (key, value)]).err().unwrap();
            assert!(format!("{error:#}").contains(key));
        }
        for host in ["https://us.i.posthog.com\n", " https://us.i.posthog.com"] {
            let PostHogEnv::Enabled(config) = parse(&[token, ("POSTHOG_HOST", host)]).unwrap()
            else {
                panic!()
            };
            assert_eq!(config.host, "https://us.i.posthog.com");
        }

        let PostHogEnv::Enabled(custom) = parse(&[
            token,
            ("POSTHOG_TRACES_SAMPLE_RATIO", "0"),
            ("POSTHOG_LOG_LEVEL", "off"),
            ("POSTHOG_HOST", "http://local///"),
        ])
        .unwrap() else {
            panic!()
        };
        assert_eq!(custom.host, "http://local");
        assert_eq!(custom.log_level, LevelFilter::OFF);
        assert_eq!(custom.traces_sample_ratio.to_bits(), 0.0_f64.to_bits());
    }
    #[test]
    fn resources() {
        let process = eyes_subscriber::ProcessIdentity::new("test");
        for (version, identity, length) in [
            (None, None, 1),
            (Some("1.2"), None, 2),
            (None, Some(&process), 2),
            (Some("1.2"), Some(&process), 3),
        ] {
            let resource = posthog_resource("example", version, identity);
            assert_eq!(resource.len(), length);
            for (key, value) in [
                ("service.name", Some("example".into())),
                ("service.version", version.map(str::to_owned)),
                (
                    "service.instance.id",
                    identity.map(|p| p.instance_id().to_string()),
                ),
            ] {
                assert_eq!(
                    resource
                        .iter()
                        .find(|(k, _)| k.as_str() == key)
                        .map(|(_, v)| v.to_string()),
                    value
                );
            }
        }
    }
    #[test]
    fn exclusions() {
        let filter = posthog_filter(LevelFilter::TRACE);
        for target in [
            "opentelemetry_sdk::trace",
            "tracing_opentelemetry",
            "hyper",
            "hyper_util",
            "h2",
            "reqwest",
            "rustls",
            "want",
            "mio",
            "posthog",
        ] {
            for level in [
                tracing::Level::ERROR,
                tracing::Level::WARN,
                tracing::Level::INFO,
                tracing::Level::DEBUG,
                tracing::Level::TRACE,
            ] {
                assert!(!filter.would_enable(target, &level));
            }
        }
        let filter = posthog_filter(LevelFilter::INFO);
        assert!(filter.would_enable("tower_http", &tracing::Level::INFO));
        assert!(!filter.would_enable("tower_http", &tracing::Level::DEBUG));
    }
}
