//! Join a tailnet from inside the process, as an ephemeral node, with no
//! stored secret.
//!
//! At boot the process proves its platform identity (today: a Google-signed ID
//! token from the GCP metadata server), exchanges it with Tailscale's workload
//! identity federation for a short-lived API token, mints a single-use,
//! ephemeral, pre-authorized auth key for its tag, and joins with fresh
//! in-memory node keys ([tailscale-rs](https://github.com/tailscale/tailscale-rs)).
//! Nothing touches disk. When the process stops, the node goes offline and
//! Tailscale removes it.
//!
//! [`Tailnet`] implements [`setec::Dialer`], so a setec client can reach the
//! secrets server over it.

use std::{io, net::SocketAddr, time::Duration};

use setec::{Connection, DialFuture, Dialer};
use tokio::sync::RwLock;
use url::Url;
use zeroize::Zeroizing;

const DEFAULT_JOIN_TIMEOUT: Duration = Duration::from_secs(30);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const AUTH_KEY_TTL_SECS: u64 = 300;
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const GCP_METADATA: &str = "http://metadata.google.internal";
const TAILSCALE_API: &str = "https://api.tailscale.com";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid tailnet config: {0}")]
    Config(String),
    /// Getting a workload identity token or exchanging it for a key failed.
    #[error("{step} failed")]
    Identity {
        step: &'static str,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("{step} returned HTTP {status}: {body}")]
    IdentityStatus {
        step: &'static str,
        status: u16,
        body: String,
    },
    #[error("joining the tailnet did not finish within {0:?}")]
    JoinTimeout(Duration),
    #[error("tailnet node failed")]
    Device(#[source] tailscale::Error),
}

/// How the process proves who it is to Tailscale.
#[derive(Debug, Clone)]
pub enum Identity {
    /// Google Cloud (Cloud Run, GCE, GKE): an ID token for the runtime service
    /// account, from the metadata server. Pair with a Tailscale federated
    /// identity whose issuer is `https://accounts.google.com` and whose subject
    /// is the service account's unique ID.
    Gcp {
        /// The federated identity's client ID.
        client_id: String,
        /// The federated identity's audience; the ID token is requested for it.
        audience: String,
    },
}

#[derive(Debug, Clone)]
pub struct TailnetConfig {
    /// Requested hostname. Tailscale appends `-N` if it's taken.
    pub hostname: String,
    /// The tag the node joins with; the federated identity must allow it.
    pub tag: String,
    pub identity: Identity,
    /// Upper bound on minting a key and joining.
    pub join_timeout: Duration,
}

impl TailnetConfig {
    pub fn new(hostname: impl Into<String>, tag: impl Into<String>, identity: Identity) -> Self {
        Self {
            hostname: hostname.into(),
            tag: tag.into(),
            identity,
            join_timeout: DEFAULT_JOIN_TIMEOUT,
        }
    }

    /// Reads the tailnet config from the environment. None of it is secret.
    ///
    /// | Variable | Default |
    /// |---|---|
    /// | `TS_TAG` | required |
    /// | `TS_WIF_CLIENT_ID` | required |
    /// | `TS_WIF_AUDIENCE` | `api.tailscale.com/<client ID>`, the audience Tailscale generates |
    /// | `TS_WIF_PROVIDER` | `gcp` on Cloud Run (`K_SERVICE` set), otherwise required |
    /// | `TS_HOSTNAME` | the Cloud Run service name (`K_SERVICE`), otherwise required |
    /// | `TS_JOIN_TIMEOUT_SECS` | 30 |
    pub fn from_env() -> Result<Self, Error> {
        Self::from_lookup(|var| std::env::var(var).ok())
    }

    fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, Error> {
        let required =
            |var: &str| get(var).ok_or_else(|| Error::Config(format!("{var} must be set")));
        let cloud_run_service = get("K_SERVICE");

        let provider = match (get("TS_WIF_PROVIDER"), &cloud_run_service) {
            (Some(provider), _) => provider,
            (None, Some(_)) => "gcp".to_owned(),
            (None, None) => {
                return Err(Error::Config(
                    "TS_WIF_PROVIDER must be set outside Cloud Run".into(),
                ));
            }
        };
        let identity = match provider.as_str() {
            "gcp" => {
                let client_id = required("TS_WIF_CLIENT_ID")?;
                let audience = get("TS_WIF_AUDIENCE")
                    .unwrap_or_else(|| format!("api.tailscale.com/{client_id}"));
                Identity::Gcp {
                    client_id,
                    audience,
                }
            }
            other => {
                return Err(Error::Config(format!(
                    "TS_WIF_PROVIDER={other:?} is not supported (expected \"gcp\")"
                )));
            }
        };
        let hostname = get("TS_HOSTNAME")
            .or(cloud_run_service)
            .ok_or_else(|| Error::Config("TS_HOSTNAME must be set outside Cloud Run".into()))?;

        let mut config = Self::new(hostname, required("TS_TAG")?, identity);
        if let Some(secs) = get("TS_JOIN_TIMEOUT_SECS") {
            let secs = secs.parse().map_err(|e| {
                Error::Config(format!(
                    "TS_JOIN_TIMEOUT_SECS is not a whole number of seconds: {e}"
                ))
            })?;
            config.join_timeout = Duration::from_secs(secs);
        }
        Ok(config)
    }
}

/// A running in-process tailnet node.
pub struct Tailnet {
    device: RwLock<Option<tailscale::Device>>,
    ip: std::net::Ipv4Addr,
}

impl std::fmt::Debug for Tailnet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tailnet")
            .field("ip", &self.ip)
            .finish_non_exhaustive()
    }
}

impl Tailnet {
    /// Mints a single-use auth key from the platform identity and joins.
    pub async fn join(config: TailnetConfig) -> Result<Self, Error> {
        let timeout = config.join_timeout;
        tokio::time::timeout(timeout, Self::join_inner(config, &Endpoints::default()))
            .await
            .map_err(|_| Error::JoinTimeout(timeout))?
    }

    async fn join_inner(config: TailnetConfig, endpoints: &Endpoints) -> Result<Self, Error> {
        // tailscale-rs uses the process-wide rustls provider. If more than one
        // backend is linked and none is installed, an internal task panics and
        // the join never completes, so pin one. An already-installed provider
        // wins; that's fine.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

        let auth_key = mint_auth_key(&config, endpoints).await?;
        Self::join_with_key(&config, &auth_key).await
    }

    async fn join_with_key(config: &TailnetConfig, auth_key: &str) -> Result<Self, Error> {
        let ts_config = tailscale::Config {
            requested_hostname: Some(config.hostname.clone()),
            requested_tags: vec![config.tag.clone()],
            ephemeral: true,
            // Includes fresh in-memory node keys.
            ..tailscale::Config::default()
        };
        let device = tailscale::Device::new(&ts_config, Some(auth_key.to_owned()))
            .await
            .map_err(Error::Device)?;
        let ip = device.ipv4_addr().await.map_err(Error::Device)?;
        tracing::info!(hostname = config.hostname, tag = config.tag, %ip, "Joined tailnet");
        Ok(Self {
            device: RwLock::new(Some(device)),
            ip,
        })
    }

    /// This node's tailnet IPv4 address.
    pub fn ipv4(&self) -> std::net::Ipv4Addr {
        self.ip
    }

    /// Leaves the tailnet. Dials after this fail.
    pub async fn shutdown(&self) {
        if let Some(device) = self.device.write().await.take() {
            device.shutdown(Some(SHUTDOWN_TIMEOUT)).await;
            tracing::info!("Left tailnet");
        }
    }

    async fn connect(&self, host: &str, port: u16) -> io::Result<Box<dyn Connection>> {
        let guard = self.device.read().await;
        let device = guard
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "tailnet node shut down"))?;

        // No MagicDNS in tailscale-rs: find the peer in the netmap by name,
        // trying the full name, then its first label (`setec` for
        // `setec.example.ts.net`).
        let short = host.split('.').next().unwrap_or(host);
        let mut peer = device.peer_by_name(host).await.map_err(io::Error::other)?;
        if peer.is_none() && short != host {
            peer = device.peer_by_name(short).await.map_err(io::Error::other)?;
        }
        let peer = peer.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no tailnet peer named {host:?}"),
            )
        })?;
        let addr = SocketAddr::from((peer.tailnet_address.ipv4.addr(), port));
        let stream = device.tcp_connect(addr).await.map_err(io::Error::other)?;
        Ok(Box::new(stream))
    }
}

impl Dialer for Tailnet {
    fn dial<'a>(&'a self, host: &'a str, port: u16) -> DialFuture<'a> {
        Box::pin(self.connect(host, port))
    }
}

struct Endpoints {
    gcp_metadata: Url,
    tailscale_api: Url,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            gcp_metadata: Url::parse(GCP_METADATA).expect("valid URL"),
            tailscale_api: Url::parse(TAILSCALE_API).expect("valid URL"),
        }
    }
}

/// Platform identity token -> Tailscale API token -> single-use auth key.
async fn mint_auth_key(
    config: &TailnetConfig,
    endpoints: &Endpoints,
) -> Result<Zeroizing<String>, Error> {
    let http = reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|e| identity_error("building HTTP client", e))?;

    let Identity::Gcp {
        client_id,
        audience,
    } = &config.identity;

    let jwt = gcp_id_token(&http, endpoints, audience).await?;
    let api_token = exchange(&http, endpoints, client_id, &jwt).await?;
    create_key(&http, endpoints, &api_token, &config.tag).await
}

async fn gcp_id_token(
    http: &reqwest::Client,
    endpoints: &Endpoints,
    audience: &str,
) -> Result<Zeroizing<String>, Error> {
    const STEP: &str = "GCP metadata ID token";
    let mut url = endpoints
        .gcp_metadata
        .join("computeMetadata/v1/instance/service-accounts/default/identity")
        .map_err(|e| identity_error(STEP, e))?;
    url.query_pairs_mut()
        .append_pair("audience", audience)
        .append_pair("format", "full");
    let response = http
        .get(url)
        .header("Metadata-Flavor", "Google")
        .send()
        .await
        .map_err(|e| identity_error(STEP, e))?;
    let body = checked_text(STEP, response).await?;
    Ok(Zeroizing::new(body.trim().to_owned()))
}

async fn exchange(
    http: &reqwest::Client,
    endpoints: &Endpoints,
    client_id: &str,
    jwt: &str,
) -> Result<Zeroizing<String>, Error> {
    const STEP: &str = "Tailscale token exchange";
    #[derive(serde::Deserialize)]
    struct TokenResponse {
        access_token: Zeroizing<String>,
    }
    let url = endpoints
        .tailscale_api
        .join("api/v2/oauth/token-exchange")
        .map_err(|e| identity_error(STEP, e))?;
    let response = http
        .post(url)
        .form(&[("client_id", client_id), ("jwt", jwt)])
        .send()
        .await
        .map_err(|e| identity_error(STEP, e))?;
    let body = checked_text(STEP, response).await?;
    let parsed: TokenResponse = serde_json::from_str(&body).map_err(|e| identity_error(STEP, e))?;
    Ok(parsed.access_token)
}

async fn create_key(
    http: &reqwest::Client,
    endpoints: &Endpoints,
    api_token: &str,
    tag: &str,
) -> Result<Zeroizing<String>, Error> {
    const STEP: &str = "Tailscale auth key creation";
    #[derive(serde::Deserialize)]
    struct KeyResponse {
        key: Zeroizing<String>,
    }
    let url = endpoints
        .tailscale_api
        .join("api/v2/tailnet/-/keys")
        .map_err(|e| identity_error(STEP, e))?;
    let request = serde_json::json!({
        "capabilities": {"devices": {"create": {
            "reusable": false,
            "ephemeral": true,
            "preauthorized": true,
            "tags": [tag],
        }}},
        "expirySeconds": AUTH_KEY_TTL_SECS,
        "description": "cja-tailnet boot",
    });
    let response = http
        .post(url)
        .bearer_auth(api_token)
        .json(&request)
        .send()
        .await
        .map_err(|e| identity_error(STEP, e))?;
    let body = checked_text(STEP, response).await?;
    let parsed: KeyResponse = serde_json::from_str(&body).map_err(|e| identity_error(STEP, e))?;
    Ok(parsed.key)
}

/// The body of a successful response, held in zeroizing memory because it
/// carries a token. Error bodies are returned as-is: Tailscale and the
/// metadata server put no credentials in them.
async fn checked_text(
    step: &'static str,
    response: reqwest::Response,
) -> Result<Zeroizing<String>, Error> {
    let status = response.status();
    let body = Zeroizing::new(response.text().await.map_err(|e| identity_error(step, e))?);
    if !status.is_success() {
        return Err(Error::IdentityStatus {
            step,
            status: status.as_u16(),
            body: body.trim().chars().take(500).collect(),
        });
    }
    Ok(body)
}

fn identity_error(step: &'static str, e: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::Identity {
        step,
        source: Box::new(e),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_partial_json, body_string_contains, header, method, path, query_param},
    };

    use super::*;

    fn gcp_config() -> TailnetConfig {
        TailnetConfig::new(
            "arena",
            "tag:arena",
            Identity::Gcp {
                client_id: "client-123".into(),
                audience: "api.tailscale.com/client-123".into(),
            },
        )
    }

    fn endpoints(server: &MockServer) -> Endpoints {
        Endpoints {
            gcp_metadata: Url::parse(&server.uri()).unwrap(),
            tailscale_api: Url::parse(&server.uri()).unwrap(),
        }
    }

    async fn mount_happy_path(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path(
                "/computeMetadata/v1/instance/service-accounts/default/identity",
            ))
            .and(header("Metadata-Flavor", "Google"))
            .and(query_param("audience", "api.tailscale.com/client-123"))
            .and(query_param("format", "full"))
            .respond_with(ResponseTemplate::new(200).set_body_string("google.jwt.token\n"))
            .expect(1)
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v2/oauth/token-exchange"))
            .and(body_string_contains("client_id=client-123"))
            .and(body_string_contains("jwt=google.jwt.token"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"access_token": "ts-api-token"})),
            )
            .expect(1)
            .mount(server)
            .await;
    }

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |var| map.get(var).cloned()
    }

    #[test]
    fn cloud_run_needs_only_the_client_id_and_tag() {
        let config = TailnetConfig::from_lookup(lookup(&[
            ("K_SERVICE", "tf-arena"),
            ("TS_WIF_CLIENT_ID", "kAbc123CNTRL"),
            ("TS_TAG", "tag:arena"),
        ]))
        .unwrap();

        assert_eq!(config.hostname, "tf-arena");
        assert_eq!(config.tag, "tag:arena");
        assert_eq!(config.join_timeout, DEFAULT_JOIN_TIMEOUT);
        let Identity::Gcp {
            client_id,
            audience,
        } = config.identity;
        assert_eq!(client_id, "kAbc123CNTRL");
        assert_eq!(audience, "api.tailscale.com/kAbc123CNTRL");
    }

    #[test]
    fn explicit_values_override_the_defaults() {
        let config = TailnetConfig::from_lookup(lookup(&[
            ("K_SERVICE", "tf-arena"),
            ("TS_WIF_PROVIDER", "gcp"),
            ("TS_WIF_CLIENT_ID", "kAbc123CNTRL"),
            ("TS_WIF_AUDIENCE", "custom-audience"),
            ("TS_HOSTNAME", "arena"),
            ("TS_TAG", "tag:arena"),
            ("TS_JOIN_TIMEOUT_SECS", "5"),
        ]))
        .unwrap();

        assert_eq!(config.hostname, "arena");
        assert_eq!(config.join_timeout, Duration::from_secs(5));
        let Identity::Gcp { audience, .. } = config.identity;
        assert_eq!(audience, "custom-audience");
    }

    #[test]
    fn outside_cloud_run_provider_and_hostname_are_required() {
        let base = [("TS_WIF_CLIENT_ID", "k"), ("TS_TAG", "tag:x")];

        let err = TailnetConfig::from_lookup(lookup(&base)).unwrap_err();
        assert!(err.to_string().contains("TS_WIF_PROVIDER"), "{err}");

        let mut with_provider = base.to_vec();
        with_provider.push(("TS_WIF_PROVIDER", "gcp"));
        let err = TailnetConfig::from_lookup(lookup(&with_provider)).unwrap_err();
        assert!(err.to_string().contains("TS_HOSTNAME"), "{err}");
    }

    #[test]
    fn tag_and_client_id_are_always_required() {
        let err = TailnetConfig::from_lookup(lookup(&[("K_SERVICE", "s"), ("TS_TAG", "tag:x")]))
            .unwrap_err();
        assert!(err.to_string().contains("TS_WIF_CLIENT_ID"), "{err}");

        let err =
            TailnetConfig::from_lookup(lookup(&[("K_SERVICE", "s"), ("TS_WIF_CLIENT_ID", "k")]))
                .unwrap_err();
        assert!(err.to_string().contains("TS_TAG"), "{err}");
    }

    #[tokio::test]
    async fn mints_a_single_use_ephemeral_key_for_the_tag() {
        let server = MockServer::start().await;
        mount_happy_path(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/v2/tailnet/-/keys"))
            .and(header("authorization", "Bearer ts-api-token"))
            .and(body_partial_json(json!({
                "capabilities": {"devices": {"create": {
                    "reusable": false, "ephemeral": true, "preauthorized": true,
                    "tags": ["tag:arena"],
                }}},
                "expirySeconds": 300,
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"key": "tskey-auth-x"})))
            .expect(1)
            .mount(&server)
            .await;

        let key = mint_auth_key(&gcp_config(), &endpoints(&server))
            .await
            .unwrap();

        assert_eq!(key.as_str(), "tskey-auth-x");
    }

    #[tokio::test]
    async fn rejected_exchange_reports_the_step_and_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("google.jwt.token"))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v2/oauth/token-exchange"))
            .respond_with(ResponseTemplate::new(401).set_body_string("subject mismatch"))
            .mount(&server)
            .await;

        let err = mint_auth_key(&gcp_config(), &endpoints(&server))
            .await
            .unwrap_err();

        match err {
            Error::IdentityStatus { step, status, body } => {
                assert_eq!(step, "Tailscale token exchange");
                assert_eq!(status, 401);
                assert_eq!(body, "subject mismatch");
            }
            other => panic!("unexpected error {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_metadata_server_is_an_identity_error() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        drop(listener);
        let endpoints = Endpoints {
            gcp_metadata: url.clone(),
            tailscale_api: url,
        };

        let err = mint_auth_key(&gcp_config(), &endpoints).await.unwrap_err();

        assert!(
            matches!(
                err,
                Error::Identity {
                    step: "GCP metadata ID token",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    /// Live check of the whole path: an in-process node dials setec over
    /// the tailnet and the setec client speaks TLS + HTTP through it. Run by
    /// hand with a single-use ephemeral key whose tag has no setec grant:
    ///
    /// `TS_AUTH_KEY=... TS_TAG=tag:server cargo test -p cja-tailnet -- --ignored live_setec`
    ///
    /// setec must answer 403: it reached setec, which identified this node.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a tailnet auth key"]
    async fn live_setec_over_embedded_node() {
        use std::sync::Arc;

        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let key = std::env::var("TS_AUTH_KEY").expect("TS_AUTH_KEY");
        let mut config = gcp_config();
        config.hostname = "cja-tailnet-live-test".into();
        config.tag = std::env::var("TS_TAG").expect("TS_TAG");
        let tailnet = Arc::new(Tailnet::join_with_key(&config, &key).await.unwrap());
        let client = setec::Client::new(
            Url::parse("https://setec.rooster-mermaid.ts.net").unwrap(),
            tailnet.clone(),
        )
        .unwrap();

        let err = client.get("byte/smoke/test").await.unwrap_err();

        assert!(matches!(err, setec::Error::AccessDenied), "{err:?}");
        tailnet.shutdown().await;
    }
}
