//! Boot-time application manifest emission to Eyes.
//!
//! Legacy builders emit monitor authority as `None` and transport failures never
//! fail application boot. State-aware builders emit `Some`, where `Some([])`
//! authoritatively removes all declarations. Their local declaration errors are
//! synchronous and may fail boot when propagated. The server-only check which
//! rejects the Eyes origin itself remains an asynchronously logged send failure.

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
pub fn send_boot_manifest<J, S>(app_version: Option<&str>, git_sha: Option<&str>)
where
    J: JobRegistry<S>,
    S: AppState,
{
    spawn_send(build_boot_manifest::<J, S>(app_version, git_sha));
}

/// Sends an authoritative state-aware manifest. Use `::<Jobs, _>(&app_state, ...)`.
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
    if !eyes_configured_from_env() {
        return Ok(());
    }
    spawn_send(build_boot_manifest_from_state::<J, S>(
        state,
        app_version,
        git_sha,
        cron_registry,
    )?);
    Ok(())
}

/// Sends an authoritative state-aware manifest. Use `::<Jobs, _>(&app_state, ...)`.
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
    if !eyes_configured_from_env() {
        return Ok(());
    }
    spawn_send(build_boot_manifest_from_state::<J, S>(
        state,
        app_version,
        git_sha,
    )?);
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
    struct State {
        db: sqlx::PgPool,
        key: CookieKey,
        base: Option<String>,
        monitors: Vec<HttpMonitor>,
    }
    impl AppState for State {
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
            self.base.as_deref()
        }
        fn eyes_http_monitors(&self) -> Vec<HttpMonitor> {
            self.monitors.clone()
        }
    }
    #[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
    struct TestJob;
    #[async_trait::async_trait]
    impl Job<State> for TestJob {
        const NAME: &'static str = "TestJob";
        async fn run(&self, _app_state: State) -> color_eyre::Result<()> {
            Ok(())
        }
    }
    impl_job_registry!(State, TestJob);
    fn state(base: Option<&str>, monitors: Vec<HttpMonitor>) -> State {
        State {
            db: sqlx::PgPool::connect_lazy("postgres://localhost/cja_test").unwrap(),
            key: CookieKey::generate(),
            base: base.map(str::to_string),
            monitors,
        }
    }
    #[cfg(feature = "cron")]
    fn build(s: &State) -> Result<AppManifest, BootManifestDeclarationError> {
        build_boot_manifest_from_state::<Jobs, State>(s, None, None, None)
    }
    #[cfg(not(feature = "cron"))]
    fn build(s: &State) -> Result<AppManifest, BootManifestDeclarationError> {
        build_boot_manifest_from_state::<Jobs, State>(s, None, None)
    }

    #[tokio::test]
    async fn resolves_and_preserves_authority() {
        let manifest = build(&state(
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
        let empty = build(&state(None, vec![])).unwrap();
        assert_eq!(empty.monitors, Some(vec![]));
    }
    #[tokio::test]
    async fn legacy_manifest_does_not_participate_in_monitor_authority() {
        #[cfg(feature = "cron")]
        let manifest = build_boot_manifest::<Jobs, State>(Some("1.2.3"), Some("abc"), None);
        #[cfg(not(feature = "cron"))]
        let manifest = build_boot_manifest::<Jobs, State>(Some("1.2.3"), Some("abc"));
        assert_eq!(manifest.jobs, vec!["TestJob"]);
        assert_eq!(manifest.base_url, None);
        assert_eq!(manifest.monitors, None);
    }
    #[tokio::test]
    async fn absolute_head_needs_no_base() {
        let monitor = HttpMonitor::new("external", "https://status.example.net/ping")
            .method(HttpMethod::Head)
            .enabled(false);
        assert_eq!(
            build(&state(None, vec![monitor.clone()])).unwrap().monitors,
            Some(vec![monitor])
        );
    }
    #[tokio::test]
    async fn validates_base_and_targets() {
        for base in ["not a url", "ftp://example.com", "http://localhost:3000"] {
            assert!(matches!(
                build(&state(Some(base), vec![])),
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
                    build(&state(None, vec![HttpMonitor::new("x", target)])),
                    Err(BootManifestDeclarationError::Target(_))
                ),
                "{target}"
            );
        }
    }
    #[tokio::test]
    async fn validates_structure() {
        let bad = [
            HttpMonitor::new("", "https://example.com"),
            HttpMonitor::new(" x", "https://example.com"),
            HttpMonitor::new("x!", "https://example.com"),
            HttpMonitor::new("é", "https://example.com"),
        ];
        for monitor in bad {
            assert!(build(&state(None, vec![monitor])).is_err());
        }
        let mut monitors = vec![HttpMonitor::new("x", "https://example.com"); 2];
        assert!(matches!(
            build(&state(None, monitors)),
            Err(BootManifestDeclarationError::DuplicateId(_))
        ));
        monitors = (0..101)
            .map(|i| HttpMonitor::new(i.to_string(), "https://example.com"))
            .collect();
        assert_eq!(
            build(&state(None, monitors)),
            Err(BootManifestDeclarationError::TooManyMonitors)
        );
        let variants = [
            HttpMonitor::new("x", "https://example.com").interval_seconds(0),
            HttpMonitor::new("x", "https://example.com").timeout_seconds(0),
            HttpMonitor::new("x", "https://example.com").failure_threshold(0),
            HttpMonitor::new("x", "https://example.com")
                .interval_seconds(1)
                .timeout_seconds(2),
            HttpMonitor::new("x", "https://example.com").expected_status(99, 200),
            HttpMonitor::new("x", "https://example.com").expected_status(300, 200),
        ];
        for monitor in variants {
            assert!(build(&state(None, vec![monitor])).is_err());
        }
    }
    #[test]
    fn configuration_presence() {
        assert!(eyes_configured(Some(""), Some("")));
        assert!(!eyes_configured(None, Some("x")));
        assert!(!eyes_configured(Some("x"), None));
    }
}
