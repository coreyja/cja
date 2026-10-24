//! HTTP client for [setec](https://github.com/tailscale/setec), Tailscale's
//! secrets server.
//!
//! setec authenticates callers by their tailnet identity, so the client carries
//! no credentials: the process must be able to reach the server over the
//! tailnet. Every API method is a JSON `POST` and requires the
//! `Sec-X-Tailscale-No-Browsers: setec` header, which this client always sends.
//!
//! Secret values are returned as [`SecretBytes`], which zeroes its buffer on
//! drop and never prints its contents.

use std::{fmt, time::Duration};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use reqwest::{StatusCode, Url};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use zeroize::Zeroizing;

const NO_BROWSERS_HEADER: &str = "Sec-X-Tailscale-No-Browsers";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// A version number of a secret. Versions start at 1; 0 means "the active
/// version" in requests.
pub type SecretVersion = u32;

/// Secret bytes that are zeroed on drop and redacted from `Debug`.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretBytes(Zeroizing<Vec<u8>>);

impl SecretBytes {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(Zeroizing::new(bytes))
    }

    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for SecretBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SecretBytes([REDACTED; {} bytes])", self.0.len())
    }
}

/// One version of a secret's value.
#[derive(Debug, Clone)]
pub struct SecretValue {
    pub value: SecretBytes,
    pub version: SecretVersion,
}

/// Metadata about a secret. Requires the `info` permission.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SecretInfo {
    pub name: String,
    pub versions: Vec<SecretVersion>,
    pub active_version: SecretVersion,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The secret (or version) does not exist. HTTP 404.
    #[error("secret not found")]
    NotFound,
    /// The caller's tailnet identity lacks a grant for this action. HTTP 403.
    #[error("access denied by tailnet policy")]
    AccessDenied,
    /// `create-version` targeted a version number that was already used. HTTP 412.
    #[error("version is already claimed")]
    VersionClaimed,
    /// setec rejected the request parameters. HTTP 400.
    #[error("invalid request: {body}")]
    InvalidRequest { body: String },
    /// Any other non-success status, usually a 5xx.
    #[error("setec returned HTTP {status}: {body}")]
    Server { status: StatusCode, body: String },
    /// The request never got a response: DNS, connect, TLS, or timeout.
    #[error("could not reach setec")]
    Transport(#[source] reqwest::Error),
    /// The response was not the shape the API documents.
    #[error("malformed setec response: {0}")]
    Decode(String),
}

impl Error {
    /// Whether retrying the same request could succeed. Transport failures and
    /// server errors are transient; everything else is an answer.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transport(_) => true,
            Self::Server { status, .. } => status.is_server_error(),
            Self::NotFound
            | Self::AccessDenied
            | Self::VersionClaimed
            | Self::InvalidRequest { .. }
            | Self::Decode(_) => false,
        }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A setec API client. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Client {
    base: Url,
    http: reqwest::Client,
}

impl Client {
    /// Builds a client for the server at `base`, e.g.
    /// `https://setec.example.ts.net`.
    pub fn new(base: Url) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(Error::Transport)?;
        Ok(Self { base, http })
    }

    pub fn base_url(&self) -> &Url {
        &self.base
    }

    /// Fetches the active version of `name`.
    pub async fn get(&self, name: &str) -> Result<SecretValue> {
        self.get_version(name, 0).await
    }

    /// Fetches a specific version of `name`. Version 0 means the active one.
    pub async fn get_version(&self, name: &str, version: SecretVersion) -> Result<SecretValue> {
        let body = GetRequest {
            name,
            version,
            update_if_changed: false,
        };
        let raw: Option<RawSecretValue> = self.call("get", &body).await?;
        raw.ok_or_else(|| Error::Decode("get returned 304 for an unconditional request".into()))?
            .decode()
    }

    /// Fetches the active version of `name` only if it differs from `known`.
    /// Returns `None` when `known` is still active. Unchanged polls are not
    /// written to setec's audit log.
    pub async fn get_if_changed(
        &self,
        name: &str,
        known: SecretVersion,
    ) -> Result<Option<SecretValue>> {
        let body = GetRequest {
            name,
            version: known,
            update_if_changed: true,
        };
        let raw: Option<RawSecretValue> = self.call("get", &body).await?;
        raw.map(RawSecretValue::decode).transpose()
    }

    pub async fn info(&self, name: &str) -> Result<SecretInfo> {
        self.call_ok("info", &NameRequest { name }).await
    }

    /// Lists metadata for every secret the caller has `info` access to.
    pub async fn list(&self) -> Result<Vec<SecretInfo>> {
        // setec is Go: an empty result is a nil slice, encoded as `null`.
        let infos: Option<Vec<SecretInfo>> = self.call_ok("list", &serde_json::json!({})).await?;
        Ok(infos.unwrap_or_default())
    }

    /// Stores a new version of `name` and returns its version number. It does
    /// not activate the new version unless the secret is new.
    pub async fn put(&self, name: &str, value: &[u8]) -> Result<SecretVersion> {
        let body = PutRequest {
            name,
            value: Zeroizing::new(BASE64.encode(value)),
        };
        self.call_ok("put", &body).await
    }

    /// Creates `version` of `name` with `value` and activates it.
    pub async fn create_version(
        &self,
        name: &str,
        version: SecretVersion,
        value: &[u8],
    ) -> Result<()> {
        let body = CreateVersionRequest {
            name,
            version,
            value: Zeroizing::new(BASE64.encode(value)),
        };
        self.call_unit("create-version", &body).await
    }

    pub async fn activate(&self, name: &str, version: SecretVersion) -> Result<()> {
        self.call_unit("activate", &VersionRequest { name, version })
            .await
    }

    /// Deletes one non-active version of `name`.
    pub async fn delete_version(&self, name: &str, version: SecretVersion) -> Result<()> {
        self.call_unit("delete-version", &VersionRequest { name, version })
            .await
    }

    /// Deletes every version of `name`.
    pub async fn delete(&self, name: &str) -> Result<()> {
        self.call_unit("delete", &NameRequest { name }).await
    }

    async fn call_ok<Req: Serialize, Resp: DeserializeOwned>(
        &self,
        method: &str,
        body: &Req,
    ) -> Result<Resp> {
        self.call(method, body)
            .await?
            .ok_or_else(|| Error::Decode(format!("{method} returned 304")))
    }

    async fn call_unit<Req: Serialize>(&self, method: &str, body: &Req) -> Result<()> {
        let _: Option<serde_json::Value> = self.call(method, body).await?;
        Ok(())
    }

    /// Sends one API call. `Ok(None)` means HTTP 304.
    async fn call<Req: Serialize, Resp: DeserializeOwned>(
        &self,
        method: &str,
        body: &Req,
    ) -> Result<Option<Resp>> {
        let url = self
            .base
            .join(&format!("api/{method}"))
            .map_err(|e| Error::Decode(format!("bad API url: {e}")))?;
        let response = self
            .http
            .post(url)
            .header(NO_BROWSERS_HEADER, "setec")
            .json(body)
            .send()
            .await
            .map_err(Error::Transport)?;

        let status = response.status();
        if status == StatusCode::NOT_MODIFIED {
            return Ok(None);
        }
        let bytes = response.bytes().await.map_err(Error::Transport)?;
        if !status.is_success() {
            let body = String::from_utf8_lossy(&bytes).trim().to_owned();
            return Err(match status {
                StatusCode::NOT_FOUND => Error::NotFound,
                StatusCode::FORBIDDEN => Error::AccessDenied,
                StatusCode::PRECONDITION_FAILED => Error::VersionClaimed,
                StatusCode::BAD_REQUEST => Error::InvalidRequest { body },
                _ => Error::Server { status, body },
            });
        }
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| Error::Decode(format!("{method}: {e}")))
    }
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct GetRequest<'a> {
    name: &'a str,
    version: SecretVersion,
    update_if_changed: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct NameRequest<'a> {
    name: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct VersionRequest<'a> {
    name: &'a str,
    version: SecretVersion,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct PutRequest<'a> {
    name: &'a str,
    value: Zeroizing<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct CreateVersionRequest<'a> {
    name: &'a str,
    version: SecretVersion,
    value: Zeroizing<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawSecretValue {
    value: Zeroizing<String>,
    version: SecretVersion,
}

impl RawSecretValue {
    fn decode(self) -> Result<SecretValue> {
        let bytes = BASE64
            .decode(self.value.as_bytes())
            .map_err(|e| Error::Decode(format!("secret value is not base64: {e}")))?;
        Ok(SecretValue {
            value: SecretBytes::new(bytes),
            version: self.version,
        })
    }
}
