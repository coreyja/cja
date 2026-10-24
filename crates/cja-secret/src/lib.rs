//! Typed secrets from [setec](https://github.com/tailscale/setec), held in
//! memory only.
//!
//! Secrets are declared on a struct with `#[derive(Secrets)]`, fetched once at
//! boot (failing fast if any is missing or unreachable), and kept current by a
//! background refresher that polls setec for newly activated versions. Values
//! never touch disk.
//!
//! ```rust,ignore
//! use cja_secret::{Secret, SecretStore, Secrets, SetecConfig};
//!
//! #[derive(Clone, Secrets)]
//! struct AppSecrets {
//!     #[secret("attic/shop/stripe")]
//!     stripe: Secret<String>,
//! }
//!
//! let store = SecretStore::connect(SetecConfig::from_env()?)?;
//! let secrets: AppSecrets = store.load().await?;
//! store.supervise(&mut supervisor);
//!
//! let key = secrets.stripe.current(); // Arc<String>
//! ```
//!
//! # Reachability
//!
//! setec authorizes callers by tailnet identity, so the process must reach the
//! server over the tailnet: a host `tailscaled`, or a sidecar exposing an
//! HTTP(S) proxy via `HTTPS_PROXY`.

#[cfg(feature = "testing")]
use std::collections::HashMap;
use std::{
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

use cja::tasks::Supervisor;
use reqwest::Url;
use setec::{SecretBytes, SecretValue, SecretVersion};
use tokio::{sync::watch, time::Instant};
use tokio_util::sync::CancellationToken;

pub use cja_secret_macros::Secrets;
pub use setec;

#[doc(hidden)]
pub mod __private {
    pub use tokio::try_join;
}

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_hours(1);
const DEFAULT_BOOT_TIMEOUT: Duration = Duration::from_secs(30);
const INITIAL_RETRY_WAIT: Duration = Duration::from_millis(250);
const MAX_RETRY_WAIT: Duration = Duration::from_secs(4);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid setec config: {0}")]
    Config(String),
    #[error("setec request for secret {name:?} failed")]
    Setec {
        name: &'static str,
        #[source]
        source: setec::Error,
    },
    #[error("secret {name:?} was still unavailable when the {timeout:?} boot timeout ran out")]
    BootTimeout {
        name: &'static str,
        timeout: Duration,
        #[source]
        last: Option<setec::Error>,
    },
    #[error("secret {name:?} version {version} could not be parsed")]
    Parse {
        name: &'static str,
        version: SecretVersion,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("{} secret(s) failed to refresh", .0.len())]
    Refresh(Vec<Error>),
}

/// A set of secrets declared together. Derive it; don't implement it by hand.
pub trait Secrets: Sized + Send {
    /// Every secret name the struct declares, in field order.
    const NAMES: &'static [&'static str];

    /// Fetches every declared secret from `store`.
    fn declare(store: &SecretStore) -> impl Future<Output = Result<Self, Error>> + Send;
}

/// Parses a secret's raw bytes into a typed value. Parsing happens once per
/// version, so a malformed value fails at boot rather than on first use.
pub trait FromSecret: Sized + Send + Sync + 'static {
    type Error: std::error::Error + Send + Sync + 'static;

    fn from_secret(bytes: &[u8]) -> Result<Self, Self::Error>;
}

impl FromSecret for String {
    type Error = std::string::FromUtf8Error;

    fn from_secret(bytes: &[u8]) -> Result<Self, Self::Error> {
        String::from_utf8(bytes.to_vec())
    }
}

/// Raw bytes, zeroed on drop.
impl FromSecret for SecretBytes {
    type Error = std::convert::Infallible;

    fn from_secret(bytes: &[u8]) -> Result<Self, Self::Error> {
        Ok(SecretBytes::new(bytes.to_vec()))
    }
}

/// Where the store reads secrets from and how often it rechecks them.
#[derive(Debug, Clone)]
pub struct SetecConfig {
    /// The setec server, e.g. `https://setec.example.ts.net`.
    pub server: Url,
    /// How often the refresher checks for newly activated versions. Unchanged
    /// checks are conditional gets that setec does not audit-log.
    pub poll_interval: Duration,
    /// How long boot keeps retrying transient failures before giving up.
    /// Permanent failures (not found, access denied) fail immediately.
    pub boot_timeout: Duration,
}

impl SetecConfig {
    pub fn new(server: Url) -> Self {
        Self {
            server,
            poll_interval: DEFAULT_POLL_INTERVAL,
            boot_timeout: DEFAULT_BOOT_TIMEOUT,
        }
    }

    /// Reads `SETEC_SERVER` (required), plus `SETEC_POLL_INTERVAL_SECS` and
    /// `SETEC_BOOT_TIMEOUT_SECS` (optional).
    pub fn from_env() -> Result<Self, Error> {
        let server = std::env::var("SETEC_SERVER")
            .map_err(|_| Error::Config("SETEC_SERVER must be set".into()))?;
        let server = Url::parse(&server)
            .map_err(|e| Error::Config(format!("SETEC_SERVER is not a URL: {e}")))?;
        let mut config = Self::new(server);
        if let Some(secs) = env_secs("SETEC_POLL_INTERVAL_SECS")? {
            config.poll_interval = secs;
        }
        if let Some(secs) = env_secs("SETEC_BOOT_TIMEOUT_SECS")? {
            config.boot_timeout = secs;
        }
        Ok(config)
    }
}

fn env_secs(var: &str) -> Result<Option<Duration>, Error> {
    match std::env::var(var) {
        Ok(value) => value
            .parse()
            .map(|secs| Some(Duration::from_secs(secs)))
            .map_err(|e| Error::Config(format!("{var} is not a whole number of seconds: {e}"))),
        Err(_) => Ok(None),
    }
}

/// Fetches secrets and keeps declared handles current. Cheap to clone.
#[derive(Clone)]
pub struct SecretStore {
    inner: Arc<StoreInner>,
}

struct StoreInner {
    source: Source,
    poll_interval: Duration,
    boot_timeout: Duration,
    tracked: Mutex<Vec<Arc<dyn Tracked>>>,
}

enum Source {
    Setec(setec::Client),
    #[cfg(feature = "testing")]
    Fake(Mutex<HashMap<String, SecretValue>>),
}

impl Source {
    async fn get(&self, name: &str) -> Result<SecretValue, setec::Error> {
        match self {
            Self::Setec(client) => client.get(name).await,
            #[cfg(feature = "testing")]
            Self::Fake(values) => values
                .lock()
                .expect("fake secrets lock")
                .get(name)
                .cloned()
                .ok_or(setec::Error::NotFound),
        }
    }

    async fn get_if_changed(
        &self,
        name: &str,
        known: SecretVersion,
    ) -> Result<Option<SecretValue>, setec::Error> {
        match self {
            Self::Setec(client) => client.get_if_changed(name, known).await,
            #[cfg(feature = "testing")]
            Self::Fake(_) => {
                let current = self.get(name).await?;
                Ok((current.version != known).then_some(current))
            }
        }
    }
}

impl fmt::Debug for SecretStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretStore")
            .field("poll_interval", &self.inner.poll_interval)
            .field("boot_timeout", &self.inner.boot_timeout)
            .finish_non_exhaustive()
    }
}

impl SecretStore {
    /// Builds a store backed by setec. Makes no requests until secrets are
    /// declared.
    pub fn connect(config: SetecConfig) -> Result<Self, Error> {
        let client = setec::Client::new(config.server)
            .map_err(|e| Error::Config(format!("building HTTP client: {e}")))?;
        Ok(Self::from_source(
            Source::Setec(client),
            config.poll_interval,
            config.boot_timeout,
        ))
    }

    /// An in-memory store for tests. Values start at version 1; use
    /// [`SecretStore::rotate`] to simulate activating a new version.
    #[cfg(feature = "testing")]
    pub fn fake<'a>(secrets: impl IntoIterator<Item = (&'a str, &'a [u8])>) -> Self {
        let values = secrets
            .into_iter()
            .map(|(name, value)| {
                (
                    name.to_owned(),
                    SecretValue {
                        value: SecretBytes::new(value.to_vec()),
                        version: 1,
                    },
                )
            })
            .collect();
        Self::from_source(
            Source::Fake(Mutex::new(values)),
            DEFAULT_POLL_INTERVAL,
            DEFAULT_BOOT_TIMEOUT,
        )
    }

    /// Activates a new version of `name` in a fake store. The new value
    /// reaches handles on the next [`SecretStore::refresh`].
    #[cfg(feature = "testing")]
    pub fn rotate(&self, name: &str, value: &[u8]) {
        let Source::Fake(values) = &self.inner.source else {
            panic!("rotate is only supported on SecretStore::fake");
        };
        let mut values = values.lock().expect("fake secrets lock");
        let version = values.get(name).map_or(1, |v| v.version + 1);
        values.insert(
            name.to_owned(),
            SecretValue {
                value: SecretBytes::new(value.to_vec()),
                version,
            },
        );
    }

    fn from_source(source: Source, poll_interval: Duration, boot_timeout: Duration) -> Self {
        Self {
            inner: Arc::new(StoreInner {
                source,
                poll_interval,
                boot_timeout,
                tracked: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Fetches every secret declared by `S`. Fails if any secret is missing,
    /// denied, unparseable, or unreachable for longer than the boot timeout.
    pub async fn load<S: Secrets>(&self) -> Result<S, Error> {
        S::declare(self).await
    }

    /// Fetches one secret and tracks it for refresh. Prefer
    /// `#[derive(Secrets)]` + [`SecretStore::load`].
    pub async fn declare<T: FromSecret>(&self, name: &'static str) -> Result<Secret<T>, Error> {
        let fetched = self.fetch_at_boot(name).await?;
        let value = parse::<T>(name, &fetched)?;
        let entry = Arc::new(Entry {
            name,
            version: AtomicU32::new(fetched.version),
            checked_at: Mutex::new(Instant::now()),
            tx: watch::Sender::new(Arc::new(value)),
        });
        self.inner
            .tracked
            .lock()
            .expect("tracked secrets lock")
            .push(entry.clone());
        tracing::info!(secret = name, version = fetched.version, "Loaded secret");
        Ok(Secret { entry })
    }

    async fn fetch_at_boot(&self, name: &'static str) -> Result<SecretValue, Error> {
        let timeout = self.inner.boot_timeout;
        let deadline = Instant::now() + timeout;
        let mut wait = INITIAL_RETRY_WAIT;
        loop {
            let attempt = tokio::time::timeout_at(deadline, self.inner.source.get(name)).await;
            let error = match attempt {
                Ok(Ok(value)) => return Ok(value),
                Ok(Err(error)) if !error.is_retryable() => {
                    return Err(Error::Setec {
                        name,
                        source: error,
                    });
                }
                Ok(Err(error)) => error,
                Err(_elapsed) => {
                    return Err(Error::BootTimeout {
                        name,
                        timeout,
                        last: None,
                    });
                }
            };
            if Instant::now() + wait >= deadline {
                return Err(Error::BootTimeout {
                    name,
                    timeout,
                    last: Some(error),
                });
            }
            tracing::warn!(secret = name, error = ?error, retry_in = ?wait, "setec unavailable at boot; retrying");
            tokio::time::sleep(wait).await;
            wait = (wait * 2).min(MAX_RETRY_WAIT);
        }
    }

    /// Checks every tracked secret for a newly activated version once. Failures
    /// keep the last good value and are logged; they're also returned so a
    /// caller (or a test) can act on them.
    pub async fn refresh(&self) -> Result<(), Error> {
        let tracked = self
            .inner
            .tracked
            .lock()
            .expect("tracked secrets lock")
            .clone();
        let mut failures = Vec::new();
        for entry in tracked {
            if let Err(error) = self.refresh_one(entry.as_ref()).await {
                match &error {
                    Error::Setec {
                        source: setec::Error::NotFound | setec::Error::AccessDenied,
                        ..
                    }
                    | Error::Parse { .. } => {
                        tracing::error!(secret = entry.name(), error = ?error, "Secret refresh failed; keeping last good value");
                    }
                    _ => {
                        tracing::warn!(secret = entry.name(), error = ?error, "Secret refresh failed; keeping last good value");
                    }
                }
                failures.push(error);
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(Error::Refresh(failures))
        }
    }

    async fn refresh_one(&self, entry: &dyn Tracked) -> Result<(), Error> {
        let name = entry.name();
        let known = entry.version();
        let changed = self
            .inner
            .source
            .get_if_changed(name, known)
            .await
            .map_err(|source| Error::Setec { name, source })?;
        if let Some(value) = changed {
            entry.apply(&value)?;
            tracing::info!(
                secret = name,
                from = known,
                to = value.version,
                "Secret rotated"
            );
        }
        entry.mark_checked(Instant::now());
        Ok(())
    }

    /// Refreshes every `poll_interval` until `shutdown` is cancelled.
    pub async fn refresh_until(self, shutdown: CancellationToken) {
        let mut ticker = tokio::time::interval_at(
            Instant::now() + self.inner.poll_interval,
            self.inner.poll_interval,
        );
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = shutdown.cancelled() => return,
                _ = ticker.tick() => {
                    // Failures are logged inside refresh and never stop the loop.
                    let _ = self.refresh().await;
                }
            }
        }
    }

    /// Runs [`SecretStore::refresh_until`] as a supervised cja task.
    pub fn supervise(&self, supervisor: &mut Supervisor) {
        let store = self.clone();
        let shutdown = supervisor.shutdown_token();
        supervisor.spawn("secret-refresh", async move {
            store.refresh_until(shutdown).await;
            Ok(())
        });
    }

    /// The oldest successful check across all tracked secrets, or `None` if
    /// nothing is declared. Useful for a staleness health check.
    pub fn last_refreshed(&self) -> Option<Instant> {
        self.inner
            .tracked
            .lock()
            .expect("tracked secrets lock")
            .iter()
            .map(|entry| entry.checked_at())
            .min()
    }
}

fn parse<T: FromSecret>(name: &'static str, value: &SecretValue) -> Result<T, Error> {
    T::from_secret(value.value.expose()).map_err(|e| Error::Parse {
        name,
        version: value.version,
        source: Box::new(e),
    })
}

/// A typed handle to one secret's current value. Cheap to clone; every clone
/// sees rotations.
pub struct Secret<T> {
    entry: Arc<Entry<T>>,
}

impl<T> Clone for Secret<T> {
    fn clone(&self) -> Self {
        Self {
            entry: self.entry.clone(),
        }
    }
}

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Secret")
            .field("name", &self.entry.name)
            .field("version", &self.version())
            .finish_non_exhaustive()
    }
}

impl<T> Secret<T> {
    pub fn name(&self) -> &'static str {
        self.entry.name
    }

    /// The value of the active version as of the last refresh.
    pub fn current(&self) -> Arc<T> {
        self.entry.tx.borrow().clone()
    }

    pub fn version(&self) -> SecretVersion {
        self.entry.version.load(Ordering::Acquire)
    }

    /// A receiver that wakes on every rotation, for state derived from the
    /// secret (clients, signers) that must be rebuilt when it changes.
    pub fn watch(&self) -> watch::Receiver<Arc<T>> {
        self.entry.tx.subscribe()
    }
}

struct Entry<T> {
    name: &'static str,
    version: AtomicU32,
    checked_at: Mutex<Instant>,
    tx: watch::Sender<Arc<T>>,
}

/// Type-erased view of an [`Entry`] for the refresher.
trait Tracked: Send + Sync {
    fn name(&self) -> &'static str;
    fn version(&self) -> SecretVersion;
    fn apply(&self, value: &SecretValue) -> Result<(), Error>;
    fn mark_checked(&self, at: Instant);
    fn checked_at(&self) -> Instant;
}

impl<T: FromSecret> Tracked for Entry<T> {
    fn name(&self) -> &'static str {
        self.name
    }

    fn version(&self) -> SecretVersion {
        self.version.load(Ordering::Acquire)
    }

    fn apply(&self, value: &SecretValue) -> Result<(), Error> {
        let parsed = parse::<T>(self.name, value)?;
        self.tx.send_replace(Arc::new(parsed));
        self.version.store(value.version, Ordering::Release);
        Ok(())
    }

    fn mark_checked(&self, at: Instant) {
        *self.checked_at.lock().expect("checked_at lock") = at;
    }

    fn checked_at(&self) -> Instant {
        *self.checked_at.lock().expect("checked_at lock")
    }
}
