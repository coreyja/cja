//! Non-secret configuration for cja-llm.
//!
//! Mirrors `cja_secret::SetecConfig`'s env/builder conventions: everything is
//! plain data with defaults, settable from the environment or by builder
//! setters, and nothing here is a secret. The only environment variable cja-llm
//! ever reads is the optional, non-secret `CJA_LLM_URL` (plus the documented
//! app-name fallbacks).

use std::sync::Arc;
use std::time::Duration;

use url::Url;

use crate::{
    DEFAULT_CONNECT_TIMEOUT, DEFAULT_GATEWAY_URL, DEFAULT_REQUEST_TIMEOUT,
    DEFAULT_STREAM_IDLE_TIMEOUT, Error,
};

/// How cja-llm's connections reach the gateway.
///
/// The gateway identifies the caller by tailnet `WhoIs` of the connecting
/// node, so the dialer choice is the identity choice.
#[derive(Clone, Default)]
#[non_exhaustive]
pub enum DialerChoice {
    /// The host's network via `setec::TcpDialer`. The host must be on the
    /// tailnet (`tailscaled`): byte-vm, attic. On a shared host every
    /// container egresses as the host's node — one identity key and one
    /// monthly budget for all of them.
    #[default]
    Host,
    /// An already-joined in-process tailnet node. Cloud Run apps share
    /// cja-secret's node via `Refresher::tailnet()`; standalone apps join
    /// via `cja_tailnet::Tailnet::join` and own shutdown themselves.
    #[cfg(feature = "tailnet")]
    Tailnet(Arc<cja_tailnet::Tailnet>),
    /// Any other [`setec::Dialer`] (exotic hosts, test stubs).
    Custom(Arc<dyn setec::Dialer>),
}

impl std::fmt::Debug for DialerChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Variant name only: node and dialer internals are never printed.
        match self {
            Self::Host => f.write_str("Host"),
            #[cfg(feature = "tailnet")]
            Self::Tailnet(_) => f.write_str("Tailnet"),
            Self::Custom(_) => f.write_str("Custom"),
        }
    }
}

/// How hard the transport retries before surfacing a failure.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Total attempts per request, including the first. Default 3.
    pub max_attempts: u32,
    /// The first retry's backoff. Default 500ms.
    pub initial_backoff: Duration,
    /// The backoff ceiling, and the clamp for a seconds-form `Retry-After`.
    /// Default 10s.
    pub max_backoff: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(10),
        }
    }
}

/// Everything cja-llm needs to build its client. All of it is non-secret.
#[derive(Clone)]
pub struct LlmConfig {
    /// The gateway's origin, e.g. `https://llm.rooster-mermaid.ts.net`.
    pub gateway: Url,
    /// The `X-App` value: a log dimension only — it never affects access,
    /// identity, or budget. Never empty.
    pub app_name: String,
    /// The network path (and tailnet identity) requests take.
    pub dialer: DialerChoice,
    /// Per-attempt budget for dial + TLS + HTTP handshake. Default 10s.
    pub connect_timeout: Duration,
    /// Total deadline across attempts, backoff included, through the final
    /// response head. Default 120s. Not a money-saver: the gateway bills
    /// abandoned attempts.
    pub request_timeout: Duration,
    /// Per-chunk budget once a streamed body starts. Default 60s.
    pub stream_idle_timeout: Duration,
    pub retries: RetryPolicy,
}

impl std::fmt::Debug for LlmConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmConfig")
            .field("gateway", &self.gateway.as_str())
            .field("app_name", &self.app_name)
            .field("dialer", &self.dialer)
            .field("connect_timeout", &self.connect_timeout)
            .field("request_timeout", &self.request_timeout)
            .field("stream_idle_timeout", &self.stream_idle_timeout)
            .field("retries", &self.retries)
            .finish()
    }
}

impl LlmConfig {
    /// Defaults for `gateway`, with [`DialerChoice::Host`].
    pub fn new(gateway: Url) -> Self {
        Self {
            gateway,
            app_name: default_app_name(),
            dialer: DialerChoice::Host,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            stream_idle_timeout: DEFAULT_STREAM_IDLE_TIMEOUT,
            retries: RetryPolicy::default(),
        }
    }

    /// Reads `CJA_LLM_URL` (optional, non-secret) and the app-name fallbacks.
    ///
    /// | Variable | Meaning | Default |
    /// |---|---|---|
    /// | `CJA_LLM_URL` | gateway origin override | `https://llm.rooster-mermaid.ts.net` |
    /// | `CJA_LLM_APP` | `X-App` value | `K_SERVICE`, else the executable's file stem, else `cja-app` |
    ///
    /// Only the URL's origin is kept: paths and queries are dropped, so a
    /// trailing `/` can never turn `/v1` into `//v1`.
    pub fn from_env() -> Result<Self, Error> {
        Self::from_lookup(|var| std::env::var(var).ok())
    }

    fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, Error> {
        let gateway = match get("CJA_LLM_URL") {
            Some(raw) => {
                let url = Url::parse(&raw)
                    .map_err(|e| Error::Config(format!("CJA_LLM_URL is not a URL: {e}")))?;
                if !matches!(url.scheme(), "http" | "https") {
                    return Err(Error::Config(format!(
                        "CJA_LLM_URL must be http or https, got {:?}",
                        url.scheme()
                    )));
                }
                if url.host_str().is_none() {
                    return Err(Error::Config("CJA_LLM_URL has no host".into()));
                }
                Url::parse(&url.origin().ascii_serialization())
                    .map_err(|e| Error::Config(format!("CJA_LLM_URL origin is not a URL: {e}")))?
            }
            None => Url::parse(DEFAULT_GATEWAY_URL).expect("default gateway URL parses"),
        };

        let app_name = match get("CJA_LLM_APP") {
            Some(name) => {
                if name.trim().is_empty() {
                    return Err(Error::Config(
                        "CJA_LLM_APP is set but empty; give the app a name".into(),
                    ));
                }
                name
            }
            None => get("K_SERVICE").unwrap_or_else(default_app_name),
        };

        let mut config = Self::new(gateway);
        config.app_name = app_name;
        Ok(config)
    }

    /// The OpenAI-SDK base URL rig points at: `<gateway origin>/v1`. rig
    /// appends `/chat/completions` and trims a trailing `/`, and
    /// [`Url::origin`](url::Url::origin) round-trips without one, so the wire
    /// path is always exactly `/v1/chat/completions`.
    pub fn openai_base_url(&self) -> String {
        format!("{}/v1", self.gateway.origin().ascii_serialization())
    }

    /// The configured app name as an `X-App` header value.
    pub(crate) fn app_header_value(&self) -> Result<http::HeaderValue, Error> {
        http::HeaderValue::from_str(&self.app_name).map_err(|e| {
            Error::Config(format!(
                "app name {:?} is not a valid header value: {e}",
                self.app_name
            ))
        })
    }

    /// Override the `X-App` value.
    pub fn app_name(mut self, app_name: impl Into<String>) -> Self {
        self.app_name = app_name.into();
        self
    }

    /// Override the per-attempt connect budget (dial + TLS + handshake).
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// Override the total request deadline.
    pub fn request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Override the per-chunk stream idle timeout.
    pub fn stream_idle_timeout(mut self, timeout: Duration) -> Self {
        self.stream_idle_timeout = timeout;
        self
    }

    /// Override the retry policy.
    pub fn retries(mut self, retries: RetryPolicy) -> Self {
        self.retries = retries;
        self
    }

    /// Dial through an already-joined in-process tailnet node.
    #[cfg(feature = "tailnet")]
    pub fn tailnet(mut self, tailnet: Arc<cja_tailnet::Tailnet>) -> Self {
        self.dialer = DialerChoice::Tailnet(tailnet);
        self
    }

    /// Dial through a custom [`setec::Dialer`].
    pub fn dialer(mut self, dialer: Arc<dyn setec::Dialer>) -> Self {
        self.dialer = DialerChoice::Custom(dialer);
        self
    }
}

fn default_app_name() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|path| {
            path.file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "cja-app".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |var| map.get(var).cloned()
    }

    #[test]
    fn defaults_point_at_the_gateway() {
        let config = LlmConfig::new(Url::parse(DEFAULT_GATEWAY_URL).unwrap());
        assert_eq!(
            config.openai_base_url(),
            "https://llm.rooster-mermaid.ts.net/v1"
        );
        assert_eq!(config.connect_timeout, DEFAULT_CONNECT_TIMEOUT);
        assert_eq!(config.request_timeout, DEFAULT_REQUEST_TIMEOUT);
        assert_eq!(config.stream_idle_timeout, DEFAULT_STREAM_IDLE_TIMEOUT);
        assert_eq!(config.retries.max_attempts, 3);
        assert!(matches!(config.dialer, DialerChoice::Host));
        assert!(!config.app_name.is_empty());
    }

    #[test]
    fn a_trailing_slash_on_the_url_cannot_double_the_version_segment() {
        let config =
            LlmConfig::from_lookup(lookup(&[("CJA_LLM_URL", "https://host.example/")])).unwrap();
        assert_eq!(config.gateway.as_str(), "https://host.example/");
        assert_eq!(config.openai_base_url(), "https://host.example/v1");

        let config = LlmConfig::from_lookup(lookup(&[(
            "CJA_LLM_URL",
            "https://host.example:8443/some/path?q=1",
        )]))
        .unwrap();
        // Only the origin survives: paths and queries are dropped.
        assert_eq!(config.openai_base_url(), "https://host.example:8443/v1");
    }

    #[test]
    fn bad_urls_are_config_errors() {
        let err = LlmConfig::from_lookup(lookup(&[("CJA_LLM_URL", "ftp://host")])).unwrap_err();
        assert!(err.to_string().contains("http or https"), "{err}");

        let err = LlmConfig::from_lookup(lookup(&[("CJA_LLM_URL", "not a url")])).unwrap_err();
        assert!(err.to_string().contains("CJA_LLM_URL"), "{err}");
    }

    #[test]
    fn app_name_falls_back_from_env_to_service_to_exe() {
        let config = LlmConfig::from_lookup(lookup(&[("CJA_LLM_APP", "my-app")])).unwrap();
        assert_eq!(config.app_name, "my-app");

        let config = LlmConfig::from_lookup(lookup(&[("K_SERVICE", "tf-arena")])).unwrap();
        assert_eq!(config.app_name, "tf-arena");

        // Neither set: the executable's file stem, never empty.
        let config = LlmConfig::from_lookup(lookup(&[])).unwrap();
        assert!(!config.app_name.is_empty());

        // An explicitly empty CJA_LLM_APP is rejected, not silently defaulted.
        let err = LlmConfig::from_lookup(lookup(&[("CJA_LLM_APP", "")])).unwrap_err();
        assert!(err.to_string().contains("CJA_LLM_APP"), "{err}");
        let err = LlmConfig::from_lookup(lookup(&[("CJA_LLM_APP", "  ")])).unwrap_err();
        assert!(err.to_string().contains("CJA_LLM_APP"), "{err}");
    }

    #[test]
    fn setters_override_the_defaults() {
        let config = LlmConfig::new(Url::parse(DEFAULT_GATEWAY_URL).unwrap())
            .app_name("app")
            .connect_timeout(Duration::from_millis(250))
            .request_timeout(Duration::from_millis(250))
            .stream_idle_timeout(Duration::from_millis(250))
            .retries(RetryPolicy {
                max_attempts: 1,
                ..RetryPolicy::default()
            });
        assert_eq!(config.app_name, "app");
        assert_eq!(config.connect_timeout, Duration::from_millis(250));
        assert_eq!(config.retries.max_attempts, 1);
    }

    #[test]
    fn debug_prints_config_never_dialer_internals() {
        let config = LlmConfig::new(Url::parse(DEFAULT_GATEWAY_URL).unwrap());
        let debug = format!("{config:?}");
        assert!(debug.contains("Host"), "{debug}");
        assert!(debug.contains("llm.rooster-mermaid"), "{debug}");
    }

    #[test]
    fn header_value_rejects_control_characters() {
        let config = LlmConfig::new(Url::parse(DEFAULT_GATEWAY_URL).unwrap()).app_name("bad\napp");
        assert!(config.app_header_value().is_err());
    }
}
