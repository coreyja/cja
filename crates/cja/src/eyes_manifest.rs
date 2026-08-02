//! Boot-time app manifest emission to Eyes.
//!
//! Telemetry shows what *happened*; the manifest tells Eyes what the app's
//! *shape* is: registered jobs, cron entries, build metadata, and optionally
//! HTTP monitors. Apps normally send one manifest at process start.
//!
//! Legacy emission is fire-and-forget: transport failures never fail boot, a
//! missing Tokio runtime produces a warning, and missing `EYES_ORG_ID` or
//! `EYES_APP_ID` makes sending a debug-logged no-op. State-aware builders add
//! synchronous declaration validation. Legacy manifests use `monitors: None`
//! (no monitor authority), while state-aware manifests use `Some`, including
//! `Some([])` to authoritatively remove all monitors. The server-only rejection
//! of a target equal to the Eyes origin remains an asynchronously logged error.
//!
//! # Intended call site
//!
//! Call once at boot after constructing job and cron registries:
//!
//! ```rust,ignore
//! let _eyes_handle = cja::setup::setup_tracing("my-app")?;
//! let app_state = AppState::from_env().await?;
//! let cron_registry = cron_registry();
//! cja::eyes_manifest::send_boot_manifest::<Jobs, AppState>(
//!     Some(env!("CARGO_PKG_VERSION")),
//!     option_env!("VERGEN_GIT_SHA"),
//!     Some(&cron_registry),
//! );
//! ```
//!
//! `app_version` and `git_sha` are explicit because cja cannot read the
//! application's compile-time environment. Pass `None` when unavailable.

use std::collections::HashSet;

use crate::{app_state::AppState, jobs::registry::JobRegistry};

pub use eyes_subscriber::{
    AppManifest, CronEntry, HttpMethod, HttpMonitor, ManifestError, MonitorTargetError,
};

/// A locally invalid HTTP monitor declaration.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BootManifestDeclarationError {
    #[error(transparent)]
    Target(#[from] MonitorTargetError),
    #[error("at most 100 HTTP monitors may be declared")]
    TooManyMonitors,
    #[error("monitor ID must not be empty")]
    EmptyId,
    #[error("monitor ID exceeds 128 bytes")]
    IdTooLong,
    #[error("monitor ID must already be trimmed")]
    UntrimmedId,
    #[error("monitor ID must be ASCII")]
    NonAsciiId,
    #[error("monitor ID contains an invalid character")]
    InvalidIdCharacter,
    #[error("duplicate monitor ID: {0}")]
    DuplicateId(String),
    #[error("monitor interval must be between 1 and i64::MAX seconds")]
    InvalidInterval,
    #[error("monitor timeout must be between 1 and i64::MAX seconds")]
    InvalidTimeout,
    #[error("monitor failure threshold must be between 1 and i32::MAX")]
    InvalidFailureThreshold,
    #[error("monitor timeout must not exceed its interval")]
    TimeoutExceedsInterval,
    #[error("monitor status bounds must be between 100 and 599")]
    InvalidStatusBounds,
    #[error("monitor minimum status must not exceed its maximum status")]
    ReversedStatusBounds,
}

fn legacy_manifest<J, S>(
    app_version: Option<&str>,
    git_sha: Option<&str>,
    crons: Vec<CronEntry>,
) -> AppManifest
where
    J: JobRegistry<S>,
    S: AppState,
{
    let manifest = AppManifest::default()
        .jobs(J::job_names().iter().map(ToString::to_string).collect())
        .crons(crons);
    let manifest = if let Some(value) = app_version {
        manifest.app_version(value)
    } else {
        manifest
    };
    if let Some(value) = git_sha {
        manifest.git_sha(value)
    } else {
        manifest
    }
}

#[cfg(feature = "cron")]
#[must_use]
/// Builds a legacy manifest from the job registry and optional cron registry.
///
/// Cron schedules come from [`crate::cron::CronRegistry::entries`]. This does
/// not participate in HTTP monitor authority; prefer this API when monitors
/// are managed elsewhere.
pub fn build_boot_manifest<J, S>(
    app_version: Option<&str>,
    git_sha: Option<&str>,
    cron_registry: Option<&crate::cron::CronRegistry<S>>,
) -> AppManifest
where
    J: JobRegistry<S>,
    S: AppState,
{
    let crons = cron_registry
        .map(|registry| {
            registry
                .entries()
                .into_iter()
                .map(|(name, schedule)| CronEntry {
                    name: name.to_string(),
                    schedule,
                })
                .collect()
        })
        .unwrap_or_default();
    legacy_manifest::<J, S>(app_version, git_sha, crons)
}

#[cfg(not(feature = "cron"))]
#[must_use]
/// Builds a legacy manifest from the job registry (without cron support).
///
/// The resulting manifest does not participate in HTTP monitor authority.
pub fn build_boot_manifest<J, S>(app_version: Option<&str>, git_sha: Option<&str>) -> AppManifest
where
    J: JobRegistry<S>,
    S: AppState,
{
    legacy_manifest::<J, S>(app_version, git_sha, Vec::new())
}

fn monitor_declarations<S: AppState>(
    state: &S,
) -> Result<(Option<String>, Vec<HttpMonitor>), BootManifestDeclarationError> {
    let base = state.eyes_base_url().map(ToString::to_string);
    if let Some(base) = base.as_deref() {
        eyes_subscriber::resolve_monitor_target(None, base)?;
    }
    let mut monitors = state.eyes_http_monitors();
    if monitors.len() > 100 {
        return Err(BootManifestDeclarationError::TooManyMonitors);
    }
    let mut ids = HashSet::with_capacity(monitors.len());
    for monitor in &mut monitors {
        if monitor.id.is_empty() {
            return Err(BootManifestDeclarationError::EmptyId);
        }
        if monitor.id.len() > 128 {
            return Err(BootManifestDeclarationError::IdTooLong);
        }
        if monitor.id.trim() != monitor.id {
            return Err(BootManifestDeclarationError::UntrimmedId);
        }
        if !monitor.id.is_ascii() {
            return Err(BootManifestDeclarationError::NonAsciiId);
        }
        if !monitor
            .id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
        {
            return Err(BootManifestDeclarationError::InvalidIdCharacter);
        }
        if !ids.insert(monitor.id.clone()) {
            return Err(BootManifestDeclarationError::DuplicateId(
                monitor.id.clone(),
            ));
        }
        if monitor.interval_seconds == 0 || monitor.interval_seconds > i64::MAX as u64 {
            return Err(BootManifestDeclarationError::InvalidInterval);
        }
        if monitor.timeout_seconds == 0 || monitor.timeout_seconds > i64::MAX as u64 {
            return Err(BootManifestDeclarationError::InvalidTimeout);
        }
        if monitor.failure_threshold == 0 || monitor.failure_threshold > i32::MAX as u32 {
            return Err(BootManifestDeclarationError::InvalidFailureThreshold);
        }
        if monitor.timeout_seconds > monitor.interval_seconds {
            return Err(BootManifestDeclarationError::TimeoutExceedsInterval);
        }
        if !(100..=599).contains(&monitor.expected_status_min)
            || !(100..=599).contains(&monitor.expected_status_max)
        {
            return Err(BootManifestDeclarationError::InvalidStatusBounds);
        }
        if monitor.expected_status_min > monitor.expected_status_max {
            return Err(BootManifestDeclarationError::ReversedStatusBounds);
        }
        monitor.target =
            eyes_subscriber::resolve_monitor_target(base.as_deref(), &monitor.target)?.to_string();
    }
    Ok((base, monitors))
}

#[cfg(feature = "cron")]
/// Builds and validates an authoritative manifest from application state.
///
/// An empty declaration list becomes `Some([])`. Locally detectable invalid
/// declarations are returned synchronously. Rejection of the Eyes server's own
/// origin requires server context and is reported asynchronously when sent.
pub fn build_boot_manifest_from_state<J, S>(
    state: &S,
    app_version: Option<&str>,
    git_sha: Option<&str>,
    cron_registry: Option<&crate::cron::CronRegistry<S>>,
) -> Result<AppManifest, BootManifestDeclarationError>
where
    J: JobRegistry<S>,
    S: AppState,
{
    let manifest = build_boot_manifest::<J, S>(app_version, git_sha, cron_registry);
    finish_state_manifest(state, manifest)
}

#[cfg(not(feature = "cron"))]
/// Builds and validates an authoritative manifest from application state.
///
/// An empty declaration list becomes `Some([])`. Locally detectable invalid
/// declarations are returned synchronously. Rejection of the Eyes server's own
/// origin requires server context and is reported asynchronously when sent.
pub fn build_boot_manifest_from_state<J, S>(
    state: &S,
    app_version: Option<&str>,
    git_sha: Option<&str>,
) -> Result<AppManifest, BootManifestDeclarationError>
where
    J: JobRegistry<S>,
    S: AppState,
{
    let manifest = build_boot_manifest::<J, S>(app_version, git_sha);
    finish_state_manifest(state, manifest)
}

fn finish_state_manifest<S: AppState>(
    state: &S,
    manifest: AppManifest,
) -> Result<AppManifest, BootManifestDeclarationError> {
    let (base, monitors) = monitor_declarations(state)?;
    let manifest = if let Some(base) = base {
        manifest.base_url(base)
    } else {
        manifest
    };
    Ok(manifest.monitors(monitors))
}

fn eyes_configured(org_id: Option<&str>, app_id: Option<&str>) -> bool {
    org_id.is_some() && app_id.is_some()
}
fn eyes_configured_from_env() -> bool {
    eyes_configured(
        std::env::var("EYES_ORG_ID").ok().as_deref(),
        std::env::var("EYES_APP_ID").ok().as_deref(),
    )
}

#[cfg(feature = "cron")]
/// Sends a legacy boot manifest to Eyes, fire-and-forget.
///
/// Missing configuration, a missing Tokio runtime, and transport/server errors
/// are logged and never fail application boot. See the [module docs](self) for
/// the intended call site and build metadata guidance.
pub fn send_boot_manifest<J, S>(
    app_version: Option<&str>,
    git_sha: Option<&str>,
    cron_registry: Option<&crate::cron::CronRegistry<S>>,
) where
    J: JobRegistry<S>,
    S: AppState,
{
    spawn_send(build_boot_manifest::<J, S>(
        app_version,
        git_sha,
        cron_registry,
    ));
}

#[cfg(not(feature = "cron"))]
/// Sends a legacy boot manifest to Eyes, fire-and-forget (without cron support).
///
/// See the cron-enabled variant for configuration and failure behavior.
pub fn send_boot_manifest<J, S>(app_version: Option<&str>, git_sha: Option<&str>)
where
    J: JobRegistry<S>,
    S: AppState,
{
    spawn_send(build_boot_manifest::<J, S>(app_version, git_sha));
}

/// Sends an authoritative state-aware manifest. Use `::<Jobs, _>(&app_state, ...)`.
///
/// # Errors
/// Returns a declaration error only when Eyes is configured. Callers should
/// generally log and continue rather than use `?` during boot unless missing
/// monitoring should make the application unavailable.
#[cfg(feature = "cron")]
pub fn send_boot_manifest_from_state<J, S>(
    state: &S,
    app_version: Option<&str>,
    git_sha: Option<&str>,
    cron_registry: Option<&crate::cron::CronRegistry<S>>,
) -> Result<(), BootManifestDeclarationError>
where
    J: JobRegistry<S>,
    S: AppState,
{
    send_boot_manifest_from_state_inner::<J, S>(
        eyes_configured_from_env(),
        state,
        app_version,
        git_sha,
        cron_registry,
    )
}

#[cfg(feature = "cron")]
fn send_boot_manifest_from_state_inner<J, S>(
    configured: bool,
    state: &S,
    app_version: Option<&str>,
    git_sha: Option<&str>,
    cron_registry: Option<&crate::cron::CronRegistry<S>>,
) -> Result<(), BootManifestDeclarationError>
where
    J: JobRegistry<S>,
    S: AppState,
{
    if configured {
        spawn_send(build_boot_manifest_from_state::<J, S>(
            state,
            app_version,
            git_sha,
            cron_registry,
        )?);
    }
    Ok(())
}

/// Sends an authoritative state-aware manifest. Use `::<Jobs, _>(&app_state, ...)`.
///
/// # Errors
/// Returns a declaration error only when Eyes is configured. Callers should
/// generally log and continue rather than use `?` during boot unless missing
/// monitoring should make the application unavailable.
#[cfg(not(feature = "cron"))]
pub fn send_boot_manifest_from_state<J, S>(
    state: &S,
    app_version: Option<&str>,
    git_sha: Option<&str>,
) -> Result<(), BootManifestDeclarationError>
where
    J: JobRegistry<S>,
    S: AppState,
{
    send_boot_manifest_from_state_inner::<J, S>(
        eyes_configured_from_env(),
        state,
        app_version,
        git_sha,
    )
}

#[cfg(not(feature = "cron"))]
fn send_boot_manifest_from_state_inner<J, S>(
    configured: bool,
    state: &S,
    app_version: Option<&str>,
    git_sha: Option<&str>,
) -> Result<(), BootManifestDeclarationError>
where
    J: JobRegistry<S>,
    S: AppState,
{
    if configured {
        spawn_send(build_boot_manifest_from_state::<J, S>(
            state,
            app_version,
            git_sha,
        )?);
    }
    Ok(())
}

fn spawn_send(manifest: AppManifest) {
    if !eyes_configured_from_env() {
        tracing::debug!("Skipping Eyes boot manifest: EYES_ORG_ID and/or EYES_APP_ID not set");
        return;
    }
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            if let Err(error) = eyes_subscriber::send_manifest_from_env(&manifest).await {
                tracing::warn!(%error, "Failed to send Eyes boot manifest");
            }
        });
    } else {
        tracing::warn!("Skipping Eyes boot manifest: no Tokio runtime available");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{impl_job_registry, jobs::Job, server::cookies::CookieKey};

    #[derive(Clone)]
    struct TestAppState {
        db: sqlx::PgPool,
        key: CookieKey,
        base: Option<String>,
        monitors: Vec<HttpMonitor>,
        panic_on_monitor_hooks: bool,
    }
    impl AppState for TestAppState {
        fn version(&self) -> &'static str {
            "test"
        }
        fn db(&self) -> &sqlx::PgPool {
            &self.db
        }
        fn cookie_key(&self) -> &CookieKey {
            &self.key
        }
        fn eyes_base_url(&self) -> Option<&str> {
            assert!(!self.panic_on_monitor_hooks, "base hook must not be called");
            self.base.as_deref()
        }
        fn eyes_http_monitors(&self) -> Vec<HttpMonitor> {
            assert!(
                !self.panic_on_monitor_hooks,
                "monitor hook must not be called"
            );
            self.monitors.clone()
        }
    }
    #[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
    struct ManifestJobA;
    #[async_trait::async_trait]
    impl Job<TestAppState> for ManifestJobA {
        const NAME: &'static str = "ManifestJobA";
        async fn run(&self, _app_state: TestAppState) -> color_eyre::Result<()> {
            Ok(())
        }
    }

    #[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
    struct ManifestJobB;

    #[async_trait::async_trait]
    impl Job<TestAppState> for ManifestJobB {
        const NAME: &'static str = "ManifestJobB";

        async fn run(&self, _app_state: TestAppState) -> color_eyre::Result<()> {
            Ok(())
        }
    }

    impl_job_registry!(TestAppState, ManifestJobA, ManifestJobB);
    fn test_state(base: Option<&str>, monitors: Vec<HttpMonitor>) -> TestAppState {
        TestAppState {
            db: sqlx::PgPool::connect_lazy("postgres://localhost/cja_test").unwrap(),
            key: CookieKey::generate(),
            base: base.map(str::to_string),
            monitors,
            panic_on_monitor_hooks: false,
        }
    }
    #[cfg(feature = "cron")]
    fn build_from_state(s: &TestAppState) -> Result<AppManifest, BootManifestDeclarationError> {
        build_boot_manifest_from_state::<Jobs, TestAppState>(s, None, None, None)
    }
    #[cfg(not(feature = "cron"))]
    fn build_from_state(s: &TestAppState) -> Result<AppManifest, BootManifestDeclarationError> {
        build_boot_manifest_from_state::<Jobs, TestAppState>(s, None, None)
    }

    #[tokio::test]
    async fn resolves_and_preserves_authority() {
        let manifest = build_from_state(&test_state(
            Some("https://example.com/app/"),
            vec![HttpMonitor::new("health", "/health")],
        ))
        .unwrap();
        assert_eq!(
            manifest.base_url.as_deref(),
            Some("https://example.com/app/")
        );
        assert_eq!(
            manifest.monitors.unwrap()[0].target,
            "https://example.com/health"
        );
        let without_trailing_slash = build_from_state(&test_state(
            Some("https://example.com/app"),
            vec![HttpMonitor::new("health", "/health")],
        ))
        .unwrap();
        assert_eq!(
            without_trailing_slash.monitors.unwrap()[0].target,
            "https://example.com/health"
        );
        let empty = build_from_state(&test_state(None, vec![])).unwrap();
        assert_eq!(empty.monitors, Some(vec![]));
    }
    #[test]
    fn legacy_manifest_does_not_participate_in_monitor_authority() {
        #[cfg(feature = "cron")]
        let manifest = build_boot_manifest::<Jobs, TestAppState>(Some("1.2.3"), Some("abc"), None);
        #[cfg(not(feature = "cron"))]
        let manifest = build_boot_manifest::<Jobs, TestAppState>(Some("1.2.3"), Some("abc"));
        assert_eq!(manifest.app_version.as_deref(), Some("1.2.3"));
        assert_eq!(manifest.git_sha.as_deref(), Some("abc"));
        assert_eq!(manifest.jobs, vec!["ManifestJobA", "ManifestJobB"]);
        assert_eq!(manifest.base_url, None);
        assert_eq!(manifest.monitors, None);
    }

    #[cfg(feature = "cron")]
    #[test]
    fn cron_manifest_preserves_schedules_and_build_metadata() {
        use std::time::Duration;

        let mut registry = crate::cron::CronRegistry::new();
        registry.register_job(ManifestJobA, None, Duration::from_secs(300));
        registry
            .register_job_with_cron(ManifestJobB, None, "0 0 9 * * * *")
            .unwrap();

        let manifest = build_boot_manifest::<Jobs, TestAppState>(
            Some("1.2.3"),
            Some("abc123"),
            Some(&registry),
        );
        assert_eq!(manifest.app_version.as_deref(), Some("1.2.3"));
        assert_eq!(manifest.git_sha.as_deref(), Some("abc123"));
        assert_eq!(manifest.jobs, vec!["ManifestJobA", "ManifestJobB"]);
        assert_eq!(
            manifest.crons,
            vec![
                CronEntry {
                    name: "ManifestJobA".into(),
                    schedule: "300s".into(),
                },
                CronEntry {
                    name: "ManifestJobB".into(),
                    schedule: "0 0 9 * * * *".into(),
                },
            ]
        );
    }
    #[tokio::test]
    async fn absolute_head_needs_no_base() {
        let monitor = HttpMonitor::new("external", "https://status.example.net/ping")
            .method(HttpMethod::Head)
            .enabled(false);
        assert_eq!(
            build_from_state(&test_state(None, vec![monitor.clone()]))
                .unwrap()
                .monitors,
            Some(vec![monitor])
        );
    }
    #[tokio::test]
    async fn validates_base_and_targets() {
        for base in ["not a url", "ftp://example.com", "http://localhost:3000"] {
            assert!(matches!(
                build_from_state(&test_state(Some(base), vec![])),
                Err(BootManifestDeclarationError::Target(_))
            ));
        }
        for target in [
            "/health",
            "health",
            "//evil.example",
            "https://user@example.com",
            "https://example.com/#x",
            "ftp://example.com",
            "http://127.0.0.1",
        ] {
            assert!(
                matches!(
                    build_from_state(&test_state(None, vec![HttpMonitor::new("x", target)])),
                    Err(BootManifestDeclarationError::Target(_))
                ),
                "{target}"
            );
        }
    }
    #[tokio::test]
    async fn validates_structure() {
        let bad = [
            (
                HttpMonitor::new("", "https://example.com"),
                BootManifestDeclarationError::EmptyId,
            ),
            (
                HttpMonitor::new(&"x".repeat(129), "https://example.com"),
                BootManifestDeclarationError::IdTooLong,
            ),
            (
                HttpMonitor::new(" x", "https://example.com"),
                BootManifestDeclarationError::UntrimmedId,
            ),
            (
                HttpMonitor::new("x!", "https://example.com"),
                BootManifestDeclarationError::InvalidIdCharacter,
            ),
            (
                HttpMonitor::new("é", "https://example.com"),
                BootManifestDeclarationError::NonAsciiId,
            ),
        ];
        for (monitor, expected) in bad {
            assert_eq!(
                build_from_state(&test_state(None, vec![monitor])),
                Err(expected)
            );
        }
        let mut monitors = vec![HttpMonitor::new("x", "https://example.com"); 2];
        assert!(matches!(
            build_from_state(&test_state(None, monitors)),
            Err(BootManifestDeclarationError::DuplicateId(_))
        ));
        monitors = (0..101)
            .map(|i| HttpMonitor::new(i.to_string(), "https://example.com"))
            .collect();
        assert_eq!(
            build_from_state(&test_state(None, monitors)),
            Err(BootManifestDeclarationError::TooManyMonitors)
        );
        let variants = [
            (
                HttpMonitor::new("x", "https://example.com").interval_seconds(0),
                BootManifestDeclarationError::InvalidInterval,
            ),
            (
                HttpMonitor::new("x", "https://example.com").interval_seconds(i64::MAX as u64 + 1),
                BootManifestDeclarationError::InvalidInterval,
            ),
            (
                HttpMonitor::new("x", "https://example.com").timeout_seconds(0),
                BootManifestDeclarationError::InvalidTimeout,
            ),
            (
                HttpMonitor::new("x", "https://example.com").timeout_seconds(i64::MAX as u64 + 1),
                BootManifestDeclarationError::InvalidTimeout,
            ),
            (
                HttpMonitor::new("x", "https://example.com").failure_threshold(0),
                BootManifestDeclarationError::InvalidFailureThreshold,
            ),
            (
                HttpMonitor::new("x", "https://example.com").failure_threshold(i32::MAX as u32 + 1),
                BootManifestDeclarationError::InvalidFailureThreshold,
            ),
            (
                HttpMonitor::new("x", "https://example.com")
                    .interval_seconds(1)
                    .timeout_seconds(2),
                BootManifestDeclarationError::TimeoutExceedsInterval,
            ),
            (
                HttpMonitor::new("x", "https://example.com").expected_status(99, 200),
                BootManifestDeclarationError::InvalidStatusBounds,
            ),
            (
                HttpMonitor::new("x", "https://example.com").expected_status(200, 600),
                BootManifestDeclarationError::InvalidStatusBounds,
            ),
            (
                HttpMonitor::new("x", "https://example.com").expected_status(300, 200),
                BootManifestDeclarationError::ReversedStatusBounds,
            ),
        ];
        for (monitor, expected) in variants {
            assert_eq!(
                build_from_state(&test_state(None, vec![monitor])),
                Err(expected)
            );
        }
    }

    #[tokio::test]
    async fn state_send_skips_hooks_when_unconfigured_and_validates_when_configured() {
        let mut panicking = test_state(None, vec![]);
        panicking.panic_on_monitor_hooks = true;
        #[cfg(feature = "cron")]
        let unconfigured = send_boot_manifest_from_state_inner::<Jobs, TestAppState>(
            false, &panicking, None, None, None,
        );
        #[cfg(not(feature = "cron"))]
        let unconfigured = send_boot_manifest_from_state_inner::<Jobs, TestAppState>(
            false, &panicking, None, None,
        );
        assert_eq!(unconfigured, Ok(()));

        let invalid = test_state(None, vec![HttpMonitor::new("", "/health")]);
        #[cfg(feature = "cron")]
        let configured = send_boot_manifest_from_state_inner::<Jobs, TestAppState>(
            true, &invalid, None, None, None,
        );
        #[cfg(not(feature = "cron"))]
        let configured =
            send_boot_manifest_from_state_inner::<Jobs, TestAppState>(true, &invalid, None, None);
        assert_eq!(configured, Err(BootManifestDeclarationError::EmptyId));
    }
    #[test]
    fn configuration_presence() {
        assert!(eyes_configured(Some(""), Some("")));
        assert!(!eyes_configured(None, Some("x")));
        assert!(!eyes_configured(Some("x"), None));
    }
}
