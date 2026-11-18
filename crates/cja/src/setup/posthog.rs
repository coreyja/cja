use std::{collections::HashMap, fmt, time::Duration};

use color_eyre::eyre::{Result, WrapErr as _, ensure, eyre};
use opentelemetry::KeyValue;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_appender_tracing::layer::{OpenTelemetryTracingBridge, TracingSpanAttributes};
use opentelemetry_otlp::{
    LogExporter, Protocol, SpanExporter, WithExportConfig as _, WithHttpConfig as _,
};
use opentelemetry_sdk::{
    Resource,
    logs::{self, BatchLogProcessor, SdkLogger, SdkLoggerProvider},
    trace::{self, BatchSpanProcessor, Sampler, SdkTracer, SdkTracerProvider},
};
use tracing_subscriber::filter::{LevelFilter, Targets};

const TIMEOUT: Duration = Duration::from_secs(5);

pub(super) enum PostHogEnv {
    Enabled(PostHogConfig),
    Skipped { host_set_without_token: bool },
}

impl PostHogEnv {
    pub(super) fn from_env() -> Result<Self> {
        Self::from_env_with(|key| std::env::var(key).ok())
    }

    fn from_env_with(read: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let Some(token) = read("POSTHOG_PROJECT_TOKEN").filter(|t| !t.trim().is_empty()) else {
            return Ok(Self::Skipped {
                host_set_without_token: read("POSTHOG_HOST").is_some(),
            });
        };
        let host = read("POSTHOG_HOST").unwrap_or_else(|| "https://us.i.posthog.com".into());
        let url = reqwest::Url::parse(&host).map_err(|_| eyre!("invalid POSTHOG_HOST"))?;
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
        let log_level = read("POSTHOG_LOG_LEVEL")
            .map(|s| s.parse())
            .transpose()
            .wrap_err("invalid POSTHOG_LOG_LEVEL")?
            .unwrap_or(LevelFilter::INFO);
        let ratio = read("POSTHOG_TRACES_SAMPLE_RATIO")
            .map(|s| s.parse::<f64>())
            .transpose()
            .wrap_err("invalid POSTHOG_TRACES_SAMPLE_RATIO")?
            .unwrap_or(1.0);
        ensure!(
            (0.0..=1.0).contains(&ratio),
            "invalid POSTHOG_TRACES_SAMPLE_RATIO"
        );
        Ok(Self::Enabled(PostHogConfig {
            host: url.as_str().trim_end_matches('/').to_owned(),
            token,
            log_level,
            traces_sample_ratio: ratio,
        }))
    }
}

pub struct PostHogConfig {
    pub(super) host: String,
    token: String,
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

impl PostHogSink {
    pub fn build(
        config: &PostHogConfig,
        crate_name: &str,
        version: Option<&str>,
        process: Option<&eyes_subscriber::ProcessIdentity>,
        max_queue_size: usize,
    ) -> Result<Self> {
        let (logs, spans) = build_on_sync_thread(|| {
            // The async client can panic on the batch worker. Construct and drop blocking clients outside Tokio.
            // https://posthog.com/docs/logs/installation/rust
            let client = reqwest::blocking::Client::builder()
                .timeout(TIMEOUT)
                .build()
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
                    .with_max_export_batch_size(512.min(max_queue_size))
                    .with_scheduled_delay(Duration::from_secs(1))
                    .build(),
            )
            .build();
        let spans = BatchSpanProcessor::builder(spans)
            .with_batch_config(
                trace::BatchConfigBuilder::default()
                    .with_max_queue_size(max_queue_size)
                    .with_max_export_batch_size(512.min(max_queue_size))
                    .with_scheduled_delay(Duration::from_secs(5))
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
    pub async fn shutdown(self) {
        let job = tokio::task::spawn_blocking(move || {
            std::thread::scope(|s| {
                let timeout = Duration::from_secs(6);
                let logs = s.spawn(move || self.logs.shutdown_with_timeout(timeout));
                let spans = s.spawn(move || self.spans.shutdown_with_timeout(timeout));
                for result in [logs.join(), spans.join()] {
                    if !matches!(result, Ok(Ok(()))) {
                        tracing::warn!(target:"posthog", ?result, "PostHog shutdown failed");
                    }
                }
            });
        });
        let result = tokio::time::timeout(Duration::from_secs(8), job).await;
        if !matches!(result, Ok(Ok(()))) {
            tracing::warn!(target:"posthog", ?result, "PostHog shutdown worker failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        for value in ["NaN", "1.01", "-0.01", "bad"] {
            assert!(parse(&[token, ("POSTHOG_TRACES_SAMPLE_RATIO", value)]).is_err());
        }
        assert!(parse(&[token, ("POSTHOG_LOG_LEVEL", "bad")]).is_err());
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
