use openidconnect::{ClientId, IssuerUrl, RedirectUrl};
use std::{collections::HashSet, sync::Arc, time::Duration};
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
}
