use openidconnect::{ClientId, IssuerUrl, RedirectUrl};
use std::{
    collections::HashSet,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::watch;
use url::Url;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct AllowedSubject(String);

impl AllowedSubject {
    pub fn new(value: impl Into<String>) -> cja::Result<Self> {
        let value = value.into();
        let Some(id) = value.strip_prefix("userid:") else {
            return Err(cja::color_eyre::eyre::eyre!(
                "subject must start with userid:"
            ));
        };
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
            return Err(cja::color_eyre::eyre::eyre!(
                "subject must contain a decimal user ID"
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SessionTimings {
    pub max_age: Duration,
    pub refresh_interval: Duration,
    pub transient_retry: Duration,
}

impl Default for SessionTimings {
    fn default() -> Self {
        Self {
            max_age: Duration::from_hours(8),
            refresh_interval: Duration::from_hours(1),
            transient_retry: Duration::from_mins(5),
        }
    }
}

#[derive(Clone)]
pub struct PassportConfig {
    pub issuer: IssuerUrl,
    pub callback: RedirectUrl,
    pub client_id: ClientId,
    pub client_secret: watch::Receiver<Arc<String>>,
    pub allowlist: HashSet<AllowedSubject>,
    pub native_schemes: HashSet<String>,
    pub timings: SessionTimings,
    pub(crate) http: reqwest::Client,
    pub(crate) discovery:
        Arc<tokio::sync::RwLock<Option<openidconnect::core::CoreProviderMetadata>>>,
}

pub trait HasPassportConfig {
    fn passport_config(&self) -> &PassportConfig;
}

#[derive(Clone, Copy, Debug)]
pub struct OAuthTimings {
    pub code_ttl: Duration,
    pub access_ttl: Duration,
    pub grant_idle_ttl: Duration,
    pub max_upstream_outage: Duration,
}

impl Default for OAuthTimings {
    fn default() -> Self {
        Self {
            code_ttl: Duration::from_mins(5),
            access_ttl: Duration::from_hours(1),
            grant_idle_ttl: Duration::from_hours(180 * 24),
            max_upstream_outage: Duration::from_hours(8),
        }
    }
}

#[derive(Clone)]
pub struct DcrLimiter(Arc<tokio::sync::Mutex<(u32, Instant)>>, u32);

impl DcrLimiter {
    pub fn new(cap: u32) -> Self {
        Self(
            Arc::new(tokio::sync::Mutex::new((cap, Instant::now()))),
            cap,
        )
    }

    pub async fn take(&self) -> bool {
        let mut guard = self.0.lock().await;
        if guard.1.elapsed() >= Duration::from_secs(60) {
            *guard = (self.1, Instant::now());
        }
        if guard.0 == 0 {
            false
        } else {
            guard.0 -= 1;
            true
        }
    }
}

#[derive(Clone)]
pub struct OAuthOptions {
    pub allowed_redirect_uris: HashSet<String>,
    pub scopes: HashSet<String>,
    pub timings: OAuthTimings,
    pub dcr_per_minute: u32,
    pub client_cap: i64,
}

impl Default for OAuthOptions {
    fn default() -> Self {
        Self {
            allowed_redirect_uris: HashSet::from([
                "https://claude.ai/api/mcp/auth_callback".to_owned(),
                "https://claude.com/api/mcp/auth_callback".to_owned(),
            ]),
            scopes: HashSet::from(["mcp".to_owned(), "offline_access".to_owned()]),
            timings: OAuthTimings::default(),
            dcr_per_minute: 30,
            client_cap: 1000,
        }
    }
}

#[derive(Clone)]
pub struct McpOAuthConfig {
    public_base_url: Url,
    mcp_resource_url: Url,
    options: OAuthOptions,
    limiter: DcrLimiter,
}

pub trait HasMcpOAuthConfig {
    fn mcp_oauth_config(&self) -> &McpOAuthConfig;
}

impl McpOAuthConfig {
    pub fn new(
        public_base_url: Url,
        mcp_resource_url: Url,
        options: OAuthOptions,
    ) -> cja::Result<Self> {
        let base = public_base_url.as_str().trim_end_matches('/');
        let expected = format!("{base}/mcp");
        let valid_redirects = !options.allowed_redirect_uris.is_empty()
            && options.allowed_redirect_uris.iter().all(|uri| {
                Url::parse(uri).is_ok_and(|parsed| {
                    parsed.scheme() == "https"
                        && parsed.as_str() == uri
                        && parsed.fragment().is_none()
                        && parsed.username().is_empty()
                        && parsed.password().is_none()
                })
            });
        let valid_scope = |s: &str| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| (0x21..=0x7e).contains(&b) && b != b'"' && b != b'\\')
        };
        let t = options.timings;
        if !secure_url(&public_base_url)
            || public_base_url.path() != "/"
            || public_base_url.query().is_some()
            || public_base_url.fragment().is_some()
            || !public_base_url.username().is_empty()
            || public_base_url.password().is_some()
            || public_base_url.as_str() != format!("{base}/")
            || mcp_resource_url.as_str() != expected
            || !valid_redirects
            || !options.scopes.contains("mcp")
            || !options.scopes.iter().all(|s| valid_scope(s))
            || t.code_ttl.is_zero()
            || t.access_ttl.is_zero()
            || t.grant_idle_ttl.is_zero()
            || t.max_upstream_outage.is_zero()
            || options.dcr_per_minute == 0
            || options.client_cap <= 0
        {
            return Err(cja::color_eyre::eyre::eyre!(
                "invalid MCP OAuth configuration"
            ));
        }
        let limiter = DcrLimiter::new(options.dcr_per_minute);
        Ok(Self {
            public_base_url,
            mcp_resource_url,
            options,
            limiter,
        })
    }

    pub fn public_base_url(&self) -> &Url {
        &self.public_base_url
    }
    pub fn mcp_resource_url(&self) -> &Url {
        &self.mcp_resource_url
    }
    pub fn options(&self) -> &OAuthOptions {
        &self.options
    }
    pub fn limiter(&self) -> &DcrLimiter {
        &self.limiter
    }
    pub fn endpoint(&self, path: &str) -> String {
        format!(
            "{}{}",
            self.public_base_url.as_str(),
            path.trim_start_matches('/')
        )
    }
}

fn secure_url(value: &Url) -> bool {
    value.scheme() == "https"
        || (value.scheme() == "http"
            && matches!(value.host_str(), Some("localhost" | "127.0.0.1" | "::1")))
}

pub(crate) fn valid_native_scheme(s: &str) -> bool {
    let mut bytes = s.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_alphabetic())
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
        && !matches!(
            s.to_ascii_lowercase().as_str(),
            "http"
                | "https"
                | "ftp"
                | "file"
                | "data"
                | "javascript"
                | "mailto"
                | "ws"
                | "wss"
                | "about"
                | "blob"
                | "tel"
                | "sms"
                | "intent"
        )
}

impl PassportConfig {
    pub fn new(
        issuer: IssuerUrl,
        callback: RedirectUrl,
        client_id: ClientId,
        client_secret: watch::Receiver<Arc<String>>,
        allowlist: HashSet<AllowedSubject>,
        native_schemes: HashSet<String>,
        timings: SessionTimings,
    ) -> cja::Result<Self> {
        let issuer_url = Url::parse(issuer.as_str())?;
        let callback_url = Url::parse(callback.as_str())?;
        if !secure_url(&issuer_url)
            || !secure_url(&callback_url)
            || issuer_url.query().is_some()
            || issuer_url.fragment().is_some()
            || !issuer_url.username().is_empty()
            || issuer_url.password().is_some()
            || issuer_url.path() != "/" && issuer_url.path().ends_with('/')
            || issuer.as_str().ends_with('/')
            || issuer.as_str() != issuer_url.as_str().trim_end_matches('/')
            || callback_url.fragment().is_some()
            || callback_url.query().is_some()
            || !callback_url.username().is_empty()
            || callback_url.password().is_some()
            || callback_url.path() != "/auth/callback"
            || client_id.as_str().is_empty()
            || client_secret.borrow().is_empty()
            || allowlist.is_empty()
            || timings.max_age.is_zero()
            || timings.refresh_interval.is_zero()
            || timings.refresh_interval >= timings.max_age
            || timings.transient_retry.is_zero()
            || !native_schemes.iter().all(|s| valid_native_scheme(s))
        {
            return Err(cja::color_eyre::eyre::eyre!(
                "invalid passport configuration"
            ));
        }
        let http = reqwest::Client::builder()
            .use_preconfigured_tls(cja::tls::ring_client_config()?)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()?;
        let native_schemes = native_schemes
            .into_iter()
            .map(|scheme| scheme.to_ascii_lowercase())
            .collect();
        Ok(Self {
            issuer,
            callback,
            client_id,
            client_secret,
            allowlist,
            native_schemes,
            timings,
            http,
            discovery: Arc::new(tokio::sync::RwLock::new(None)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(
        issuer: &str,
        callback: &str,
        allowed: HashSet<AllowedSubject>,
        schemes: HashSet<String>,
        timings: SessionTimings,
    ) -> cja::Result<PassportConfig> {
        let (_tx, rx) = watch::channel(Arc::new("secret".to_owned()));
        PassportConfig::new(
            IssuerUrl::new(issuer.to_owned())?,
            RedirectUrl::new(callback.to_owned())?,
            ClientId::new("client".to_owned()),
            rx,
            allowed,
            schemes,
            timings,
        )
    }

    #[test]
    fn subject_validation_has_both_sides() {
        for allowed in ["userid:0", "userid:123", "userid:001"] {
            assert_eq!(AllowedSubject::new(allowed).unwrap().as_str(), allowed);
        }
        for refused in ["", "userid:", "userid:-1", "userid:abc", "node:123"] {
            assert!(AllowedSubject::new(refused).is_err(), "{refused}");
        }
    }

    #[test]
    fn https_client_and_config_validation() {
        let allowlist = HashSet::from([AllowedSubject::new("userid:123").unwrap()]);
        let schemes = HashSet::from(["testapp".to_owned()]);
        let config = setup(
            "https://passport.rooster-mermaid.ts.net",
            "https://app.example.test/auth/callback",
            allowlist.clone(),
            schemes.clone(),
            SessionTimings::default(),
        )
        .unwrap();
        assert!(
            config
                .http
                .get("https://passport.rooster-mermaid.ts.net/jwks")
                .build()
                .is_ok()
        );
        assert!(
            setup(
                "http://passport.example",
                "https://app.example.test/auth/callback",
                allowlist.clone(),
                schemes.clone(),
                SessionTimings::default()
            )
            .is_err()
        );
        assert!(
            setup(
                "https://passport.example",
                "http://app.example.test/auth/callback",
                allowlist.clone(),
                schemes.clone(),
                SessionTimings::default()
            )
            .is_err()
        );
        assert!(
            setup(
                "https://passport.example",
                "https://app.example.test/auth/callback",
                HashSet::new(),
                schemes.clone(),
                SessionTimings::default()
            )
            .is_err()
        );
        assert!(
            setup(
                "https://passport.example",
                "https://app.example.test/auth/callback",
                allowlist.clone(),
                HashSet::from(["https".to_owned()]),
                SessionTimings::default()
            )
            .is_err()
        );
        assert!(
            setup(
                "https://passport.example",
                "https://app.example.test/auth/callback",
                allowlist.clone(),
                HashSet::from(["javascript".to_owned()]),
                SessionTimings::default()
            )
            .is_err()
        );
        assert!(
            setup(
                "https://passport.example",
                "https://app.example.test/auth/callback",
                allowlist,
                schemes,
                SessionTimings {
                    max_age: Duration::from_hours(1),
                    refresh_interval: Duration::from_hours(1),
                    transient_retry: Duration::from_secs(1),
                }
            )
            .is_err()
        );
    }

    #[test]
    fn mcp_oauth_config_requires_exact_resource_and_safe_policy() {
        let base = Url::parse("https://app.example.test/").unwrap();
        let resource = Url::parse("https://app.example.test/mcp").unwrap();
        assert!(
            McpOAuthConfig::new(base.clone(), resource.clone(), OAuthOptions::default()).is_ok()
        );
        for bad in [
            "https://other.example.test/mcp",
            "https://app.example.test/mcp/",
            "https://app.example.test/mcp/secret",
        ] {
            assert!(
                McpOAuthConfig::new(
                    base.clone(),
                    Url::parse(bad).unwrap(),
                    OAuthOptions::default()
                )
                .is_err()
            );
        }
        assert!(
            McpOAuthConfig::new(
                Url::parse("https://app.example.test/path").unwrap(),
                resource.clone(),
                OAuthOptions::default()
            )
            .is_err()
        );
        let mut options = OAuthOptions::default();
        options.allowed_redirect_uris.clear();
        assert!(McpOAuthConfig::new(base.clone(), resource.clone(), options).is_err());
        let options = OAuthOptions {
            allowed_redirect_uris: HashSet::from(["http://evil.example/callback".to_owned()]),
            ..Default::default()
        };
        assert!(McpOAuthConfig::new(base.clone(), resource.clone(), options).is_err());
        let mut options = OAuthOptions::default();
        options.scopes.remove("mcp");
        assert!(McpOAuthConfig::new(base.clone(), resource.clone(), options).is_err());
        let mut options = OAuthOptions::default();
        options.timings.code_ttl = Duration::ZERO;
        assert!(McpOAuthConfig::new(base.clone(), resource.clone(), options).is_err());
        let options = OAuthOptions {
            client_cap: 0,
            ..Default::default()
        };
        assert!(McpOAuthConfig::new(base, resource, options).is_err());
    }

    #[tokio::test]
    async fn dcr_limiter_accepts_exact_capacity_then_refuses() {
        let limiter = DcrLimiter::new(2);
        let clone = limiter.clone();
        assert!(limiter.take().await);
        assert!(clone.take().await);
        assert!(!limiter.take().await);
    }
}
