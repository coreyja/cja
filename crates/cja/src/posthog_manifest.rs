//! Complete boot declarations sent to `PostHog` for Blink ingestion.
use std::{collections::HashSet, time::Duration};

use serde::Serialize;
use uuid::Uuid;

use crate::{
    app_state::AppState,
    jobs::registry::JobRegistry,
    observability::declarations::{HogqlCheck, HogqlComparison, HogqlSeverity},
};

#[derive(Debug, thiserror::Error)]
#[error("invalid manifest declaration: {0}")]
pub struct ManifestDeclarationError(String);

pub struct BootManifest<'a, S: AppState> {
    pub app: &'a str,
    pub git_sha: Option<&'a str>,
    #[cfg(feature = "cron")]
    pub registry: &'a crate::cron::CronRegistry<S>,
    pub timezone: &'a jiff::tz::TimeZone,
    pub critical_jobs: &'a [&'a str],
    pub hogql_checks: &'a [HogqlCheck],
    #[cfg(not(feature = "cron"))]
    _state: std::marker::PhantomData<S>,
}

impl<'a, S: AppState> BootManifest<'a, S> {
    #[cfg(feature = "cron")]
    pub fn new(
        app: &'a str,
        registry: &'a crate::cron::CronRegistry<S>,
        timezone: &'a jiff::tz::TimeZone,
        critical_jobs: &'a [&'a str],
        hogql_checks: &'a [HogqlCheck],
    ) -> Self {
        Self {
            app,
            git_sha: None,
            registry,
            timezone,
            critical_jobs,
            hogql_checks,
        }
    }
    #[cfg(not(feature = "cron"))]
    pub fn new(
        app: &'a str,
        timezone: &'a jiff::tz::TimeZone,
        critical_jobs: &'a [&'a str],
        hogql_checks: &'a [HogqlCheck],
    ) -> Self {
        Self {
            app,
            git_sha: None,
            timezone,
            critical_jobs,
            hogql_checks,
            _state: std::marker::PhantomData,
        }
    }
    #[must_use]
    pub fn git_sha(mut self, sha: &'a str) -> Self {
        self.git_sha = Some(sha);
        self
    }
}

fn invalid(reason: impl Into<String>) -> ManifestDeclarationError {
    ManifestDeclarationError(reason.into())
}
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
}
fn validate_names<'a>(
    names: impl IntoIterator<Item = &'a str>,
) -> Result<(), ManifestDeclarationError> {
    let mut seen = HashSet::new();
    for name in names {
        if !valid_name(name) || !seen.insert(name) {
            return Err(invalid(format!("invalid or duplicate name: {name:?}")));
        }
    }
    if seen.len() > 100 {
        return Err(invalid("more than 100 declarations"));
    }
    Ok(())
}

#[derive(Serialize)]
struct CronDeclaration {
    name: String,
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    interval_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expression: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timezone: Option<String>,
    enabled: bool,
}
#[derive(Serialize)]
struct JobDeclaration {
    name: String,
    critical: bool,
    enabled: bool,
}
#[derive(Serialize)]
struct MonitorDeclaration {
    id: String,
    target: String,
    method: &'static str,
    interval_seconds: u64,
    timeout_seconds: u64,
    expected_status_min: u16,
    expected_status_max: u16,
    failure_threshold: u32,
    enabled: bool,
}
#[derive(Serialize)]
struct CheckDeclaration<'a> {
    name: &'a str,
    query: &'a str,
    comparison: HogqlComparison,
    threshold: f64,
    interval_seconds: u64,
    severity: HogqlSeverity,
    enabled: bool,
}
#[derive(Serialize)]
struct Properties<'a> {
    #[serde(rename = "$process_person_profile")]
    process_person_profile: bool,
    manifest_version: u8,
    event_id: Uuid,
    app: &'a str,
    booted_at: String,
    app_version: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_sha: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    base_url: Option<&'a str>,
    crons: Vec<CronDeclaration>,
    jobs: Vec<JobDeclaration>,
    monitors: Vec<MonitorDeclaration>,
    hogql_checks: Vec<CheckDeclaration<'a>>,
}
#[derive(Serialize)]
struct Envelope<'a> {
    uuid: Uuid,
    api_key: &'a str,
    event: &'static str,
    distinct_id: String,
    properties: Properties<'a>,
}

fn build_monitors<S: AppState>(
    state: &S,
) -> Result<(Option<&str>, Vec<MonitorDeclaration>), ManifestDeclarationError> {
    let base_url = state.eyes_base_url();
    let base = base_url
        .map(|s| url::Url::parse(s).map_err(|_| invalid("invalid base URL")))
        .transpose()?;
    if base
        .as_ref()
        .is_some_and(|u| !matches!(u.scheme(), "http" | "https") || u.host().is_none())
    {
        return Err(invalid("invalid base URL"));
    }
    let mut monitors = Vec::new();
    for m in state.eyes_http_monitors() {
        if m.target.contains(['\\', '\t', '\r', '\n']) {
            return Err(invalid("invalid monitor target"));
        }
        let parsed = if m.target.starts_with('/') {
            base.as_ref()
                .ok_or_else(|| invalid("relative monitor requires base URL"))?
                .join(&m.target)
        } else {
            url::Url::parse(&m.target)
        }
        .map_err(|_| invalid("invalid monitor target"))?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.host().is_none()
            || (m.target.starts_with('/')
                && base.as_ref().is_some_and(|b| b.origin() != parsed.origin()))
        {
            return Err(invalid("invalid monitor origin"));
        }
        if m.interval_seconds == 0
            || m.interval_seconds > i64::MAX as u64
            || m.timeout_seconds == 0
            || m.timeout_seconds > m.interval_seconds
            || m.failure_threshold == 0
            || m.failure_threshold > i32::MAX as u32
            || !(100..=599).contains(&m.expected_status_min)
            || !(100..=599).contains(&m.expected_status_max)
            || m.expected_status_min > m.expected_status_max
        {
            return Err(invalid("invalid monitor bounds"));
        }
        monitors.push(MonitorDeclaration {
            id: m.id,
            target: m.target,
            method: match m.method {
                crate::app_state::HttpMethod::Get => "GET",
                crate::app_state::HttpMethod::Head => "HEAD",
            },
            interval_seconds: m.interval_seconds,
            timeout_seconds: m.timeout_seconds,
            expected_status_min: m.expected_status_min,
            expected_status_max: m.expected_status_max,
            failure_threshold: m.failure_threshold,
            enabled: m.enabled,
        });
    }
    validate_names(monitors.iter().map(|m| m.id.as_str()))?;
    monitors.sort_by(|a, b| a.id.cmp(&b.id));
    Ok((base_url, monitors))
}

fn build_checks(
    checks: &[HogqlCheck],
) -> Result<Vec<CheckDeclaration<'_>>, ManifestDeclarationError> {
    validate_names(checks.iter().map(|c| c.name.as_str()))?;
    let mut declarations = Vec::new();
    for c in checks {
        if c.query.len() > 16 * 1024
            || c.query.is_empty()
            || !c.threshold.is_finite()
            || !(300..=i64::MAX as u64).contains(&c.interval_seconds)
        {
            return Err(invalid("invalid HogQL check"));
        }
        declarations.push(CheckDeclaration {
            name: &c.name,
            query: &c.query,
            comparison: c.comparison,
            threshold: c.threshold,
            interval_seconds: c.interval_seconds,
            severity: c.severity,
            enabled: true,
        });
    }
    declarations.sort_by(|a, b| a.name.cmp(b.name));
    Ok(declarations)
}

fn snapshot<'a, J, S>(
    state: &'a S,
    input: &'a BootManifest<'_, S>,
    token: &'a str,
    id: Uuid,
    booted_at: &str,
) -> Result<Vec<u8>, ManifestDeclarationError>
where
    J: JobRegistry<S>,
    S: AppState,
{
    crate::observability::posthog::slug(input.app).map_err(invalid)?;
    let zone = input
        .timezone
        .iana_name()
        .ok_or_else(|| invalid("timezone must be IANA"))?;
    let mut crons = Vec::new();
    #[cfg(not(feature = "cron"))]
    let _ = zone;
    #[cfg(feature = "cron")]
    for job in input.registry.jobs().values() {
        let (kind, interval_ms, expression, timezone) = match &job.schedule {
            crate::cron::Schedule::Interval(interval) => {
                let ms = interval.0.as_millis();
                let ms =
                    i64::try_from(ms).map_err(|_| invalid("interval exceeds i64 milliseconds"))?;
                if ms == 0 {
                    return Err(invalid("interval must be positive"));
                }
                ("interval", Some(ms), None, None)
            }
            crate::cron::Schedule::Cron(cron) => {
                let expr = cron.0.to_string();
                crate::cron::parse_cron(&expr)
                    .map_err(|_| invalid("unparseable cron expression"))?;
                ("cron", None, Some(expr), Some(zone.to_owned()))
            }
        };
        crons.push(CronDeclaration {
            name: job.name.to_owned(),
            kind,
            interval_ms,
            expression,
            timezone,
            enabled: true,
        });
    }
    validate_names(crons.iter().map(|c| c.name.as_str()))?;
    crons.sort_by(|a, b| a.name.cmp(&b.name));
    validate_names(J::job_names().iter().copied())?;
    validate_names(input.critical_jobs.iter().copied())?;
    let critical: HashSet<_> = input.critical_jobs.iter().copied().collect();
    if !critical.iter().all(|name| J::job_names().contains(name)) {
        return Err(invalid("unknown critical job"));
    }
    let mut jobs: Vec<_> = J::job_names()
        .iter()
        .map(|name| JobDeclaration {
            name: (*name).to_owned(),
            critical: critical.contains(name),
            enabled: true,
        })
        .collect();
    jobs.sort_by(|a, b| a.name.cmp(&b.name));
    let (base_url, monitors) = build_monitors(state)?;
    let checks = build_checks(input.hogql_checks)?;
    let properties = Properties {
        process_person_profile: false,
        manifest_version: 1,
        event_id: id,
        app: input.app,
        booted_at: booted_at.to_owned(),
        app_version: state.version(),
        git_sha: input.git_sha,
        base_url,
        crons,
        jobs,
        monitors,
        hogql_checks: checks,
    };
    let size = serde_json::to_vec(&properties)
        .map_err(|e| invalid(e.to_string()))?
        .len();
    if size > 256 * 1024 {
        return Err(invalid("manifest properties exceed 256 KiB"));
    }
    serde_json::to_vec(&Envelope {
        uuid: id,
        api_key: token,
        event: "cja_manifest",
        distinct_id: format!("cja:{}", input.app),
        properties,
    })
    .map_err(|e| invalid(e.to_string()))
}

/// Validate locally, then enqueue an immutable capture snapshot without delaying boot.
pub fn emit_boot_manifest<J, S>(
    state: &S,
    input: &BootManifest<'_, S>,
) -> Result<(), ManifestDeclarationError>
where
    J: JobRegistry<S>,
    S: AppState,
{
    // Validate even without capture credentials.
    let id = Uuid::new_v4();
    let booted_at = jiff::Timestamp::now()
        .round(jiff::Unit::Microsecond)
        .map_err(|_| invalid("invalid boot time"))?
        .to_string();
    let validation = snapshot::<J, S>(state, input, "", id, &booted_at)?;
    drop(validation);
    let credentials = match crate::setup::posthog::PostHogCredentials::from_env() {
        Ok(Some(c)) => c,
        Ok(None) => return Ok(()),
        Err(error) => {
            tracing::warn!(%error, "skipping PostHog manifest capture");
            return Ok(());
        }
    };
    let bytes = snapshot::<J, S>(state, input, &credentials.token, id, &booted_at)?;
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        tracing::warn!("PostHog manifest capture requires a Tokio runtime");
        return Ok(());
    };
    runtime.spawn(send(bytes, credentials.host));
    Ok(())
}

async fn send(bytes: Vec<u8>, host: String) {
    let tls = match crate::tls::ring_client_config() {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(%e, "manifest TLS unavailable");
            return;
        }
    };
    let client = match reqwest_012::Client::builder()
        .use_preconfigured_tls(tls)
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(%e, "manifest HTTP client unavailable");
            return;
        }
    };
    // /i/v0/e/ accepts a single capture; top-level UUID and immutable bytes make retries idempotent to Blink.
    let endpoint = format!("{host}/i/v0/e/");
    let started = tokio::time::Instant::now();
    let mut delay = Duration::from_secs(1);
    let mut last_class = None;
    while started.elapsed() < Duration::from_hours(24) {
        let response = client
            .post(&endpoint)
            .header("content-type", "application/json")
            .body(bytes.clone())
            .send()
            .await;
        let (class, retry_after) = match response {
            Ok(r) if r.status().is_success() => {
                if last_class.is_some() {
                    tracing::info!("manifest capture recovered");
                }
                return;
            }
            Ok(r) => {
                let code = r.status().as_u16();
                if code != 408 && code != 429 && code < 500 {
                    tracing::warn!(code, "manifest capture rejected");
                    return;
                }
                let retry = retry_after(r.headers());
                (format!("HTTP {code}"), retry)
            }
            Err(e) => (
                if e.is_timeout() { "timeout" } else { "network" }.to_owned(),
                None,
            ),
        };
        if last_class.as_ref() != Some(&class) {
            tracing::warn!(%class, "manifest capture retrying");
            last_class = Some(class);
        }
        tokio::time::sleep(retry_after.unwrap_or(delay)).await;
        delay = (delay * 2).min(Duration::from_secs(300));
    }
    tracing::warn!("manifest capture expired after 24 hours");
}

fn retry_after(headers: &reqwest_012::header::HeaderMap) -> Option<Duration> {
    headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .map(|s| Duration::from_secs(s.clamp(1, 300)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{app_state::HttpMonitor, server::cookies::CookieKey};

    #[derive(Clone)]
    struct State {
        db: sqlx::PgPool,
        key: CookieKey,
        target: &'static str,
    }
    impl AppState for State {
        fn version(&self) -> &'static str {
            "1.0.0"
        }
        fn db(&self) -> &sqlx::PgPool {
            &self.db
        }
        fn cookie_key(&self) -> &CookieKey {
            &self.key
        }
        fn eyes_base_url(&self) -> Option<&str> {
            Some("https://demo.example.com")
        }
        fn eyes_http_monitors(&self) -> Vec<HttpMonitor> {
            vec![HttpMonitor::new("home", self.target)]
        }
    }
    fn state() -> State {
        State {
            db: sqlx::postgres::PgPoolOptions::new()
                .connect_lazy("postgres:///test")
                .unwrap(),
            key: CookieKey::from_env_or_generate().unwrap(),
            target: "/health",
        }
    }
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    struct Work;
    #[async_trait::async_trait]
    impl crate::jobs::Job<State> for Work {
        const NAME: &'static str = "work";
        async fn run(&self, _: State) -> crate::Result<()> {
            Ok(())
        }
    }
    crate::impl_job_registry!(State, Work);
    #[test]
    fn declaration_name_limits_and_duplicates() {
        assert!(validate_names(["A".repeat(128).as_str()]).is_ok());
        assert!(validate_names(["A".repeat(129).as_str()]).is_err());
        assert!(validate_names(["duplicate", "duplicate"]).is_err());
        let names: Vec<_> = (0..100).map(|n| format!("item-{n}")).collect();
        assert!(validate_names(names.iter().map(String::as_str)).is_ok());
        let names: Vec<_> = (0..101).map(|n| format!("item-{n}")).collect();
        assert!(validate_names(names.iter().map(String::as_str)).is_err());
    }
    #[test]
    fn retry_after_is_bounded() {
        let mut headers = reqwest_012::header::HeaderMap::new();
        assert_eq!(retry_after(&headers), None);
        headers.insert("retry-after", "0".parse().unwrap());
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(1)));
        headers.insert("retry-after", "300".parse().unwrap());
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(300)));
        headers.insert("retry-after", "301".parse().unwrap());
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(300)));
        headers.insert("retry-after", "bogus".parse().unwrap());
        assert_eq!(retry_after(&headers), None);
    }
    #[tokio::test]
    async fn exact_fixture_shape_and_validation() {
        let state = state();
        let mut registry = crate::cron::CronRegistry::new();
        registry.register_job(Work, None, Duration::from_secs(300));
        let zone = jiff::tz::TimeZone::get("America/New_York").unwrap();
        let check = HogqlCheck::new(
            "errors",
            "SELECT count() FROM events",
            HogqlComparison::Gt,
            0.0,
            300,
            HogqlSeverity::Warning,
        );
        let checks = [check];
        let input =
            BootManifest::new("demo", &registry, &zone, &["work"], &checks).git_sha("abc123");
        let id = Uuid::parse_str("a0000000-0000-0000-0000-000000000001").unwrap();
        let bytes =
            snapshot::<Jobs, State>(&state, &input, "token", id, "2026-11-18T18:00:00Z").unwrap();
        let event: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let mut fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/cja_manifest_v1.json")).unwrap();
        // Blink main d3b03b4 fixture predates DEV-2499 severity.
        fixture["properties"]["hogql_checks"][0]["severity"] = "warning".into();
        fixture["properties"]["crons"][0]["name"] = "work".into();
        fixture["properties"]["app_version"] = "1.0.0".into();
        fixture["properties"]["hogql_checks"][0]["threshold"] = serde_json::json!(0.0);
        fixture["uuid"] = id.to_string().into();
        fixture["api_key"] = "token".into();
        assert_eq!(event, fixture);
        assert!(event.get("timestamp").is_none());
        assert!(event.get("sent_at").is_none());
        assert!(event["properties"].get("process_role").is_none());
        assert!(crate::cron::parse_cron("0 */5 * * * *").is_ok());
        let mut cron_registry = crate::cron::CronRegistry::new();
        cron_registry
            .register_job_with_cron(Work, None, "0 */5 * * * *")
            .unwrap();
        let cron_input = BootManifest::new("demo", &cron_registry, &zone, &[], &[]);
        let cron_event: serde_json::Value = serde_json::from_slice(
            &snapshot::<Jobs, State>(&state, &cron_input, "", id, "2026-11-18T18:00:00Z").unwrap(),
        )
        .unwrap();
        let cron = &cron_event["properties"]["crons"][0];
        assert_eq!(cron["kind"], "cron");
        assert_eq!(cron["timezone"], "America/New_York");
        assert!(cron.get("interval_ms").is_none());
        assert!(crate::cron::parse_cron(cron["expression"].as_str().unwrap()).is_ok());
        let invalid = BootManifest::new("demo", &registry, &zone, &["missing"], &checks);
        assert!(snapshot::<Jobs, State>(&state, &invalid, "", id, "2026-11-18T18:00:00Z").is_err());
        let escaped = State {
            target: "//evil.example/health",
            ..state.clone()
        };
        assert!(snapshot::<Jobs, State>(&escaped, &input, "", id, "2026-11-18T18:00:00Z").is_err());
        let comparisons = [
            HogqlComparison::Gt,
            HogqlComparison::Gte,
            HogqlComparison::Lt,
            HogqlComparison::Lte,
            HogqlComparison::Eq,
            HogqlComparison::Neq,
        ];
        let all_checks: Vec<_> = comparisons
            .into_iter()
            .enumerate()
            .map(|(n, comparison)| {
                HogqlCheck::new(
                    format!("check-{n}"),
                    "SELECT 1",
                    comparison,
                    1.0,
                    300,
                    HogqlSeverity::Critical,
                )
            })
            .collect();
        let input = BootManifest::new("demo", &registry, &zone, &[], &all_checks);
        let event: serde_json::Value = serde_json::from_slice(
            &snapshot::<Jobs, State>(&state, &input, "", id, "2026-11-18T18:00:00Z").unwrap(),
        )
        .unwrap();
        let actual: Vec<_> = event["properties"]["hogql_checks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["comparison"].as_str().unwrap())
            .collect();
        assert_eq!(actual, ["gt", "gte", "lt", "lte", "eq", "neq"]);
        assert_eq!(event["properties"]["jobs"][0]["critical"], false);
        let oversized = [HogqlCheck::new(
            "bad",
            "x".repeat(16 * 1024 + 1),
            HogqlComparison::Gt,
            0.0,
            300,
            HogqlSeverity::Info,
        )];
        let invalid = BootManifest::new("demo", &registry, &zone, &[], &oversized);
        assert!(snapshot::<Jobs, State>(&state, &invalid, "", id, "2026-11-18T18:00:00Z").is_err());
    }

    /// Pinned by PR review round 1: Blink's `resolve` joins any target that is
    /// not an absolute URL onto `base_url` (blink main d3b03b4 src/manifest.rs),
    /// so a same-origin relative target without a leading `/` is a valid
    /// declaration and must not fail boot locally.
    #[tokio::test]
    async fn same_origin_relative_target_without_leading_slash_is_accepted() {
        let registry = crate::cron::CronRegistry::new();
        let zone = jiff::tz::TimeZone::UTC;
        let input = BootManifest::new("demo", &registry, &zone, &[], &[]);
        let id = Uuid::parse_str("a0000000-0000-0000-0000-000000000001").unwrap();
        let relative = State {
            target: "health",
            ..state()
        };
        let bytes = snapshot::<Jobs, State>(&relative, &input, "", id, "2026-11-18T18:00:00Z")
            .expect("same-origin relative target accepted by Blink must be accepted by cja");
        let event: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(event["properties"]["monitors"][0]["target"], "health");
    }

    #[tokio::test]
    async fn child_boot_marker() {
        if std::env::var_os("CJA_MANIFEST_CHILD").is_none() {
            return;
        }
        let state = state();
        let registry = crate::cron::CronRegistry::new();
        let zone = jiff::tz::TimeZone::UTC;
        let input = BootManifest::new("demo", &registry, &zone, &[], &[]);
        emit_boot_manifest::<Jobs, State>(&state, &input).unwrap();
        println!("BOOT_READY");
    }

    #[tokio::test]
    async fn retry_reuses_identical_capture_bytes_and_uuid() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let mut bodies = Vec::new();
            for status in ["503 Service Unavailable", "200 OK"] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = Vec::new();
                let header_end;
                loop {
                    let mut chunk = [0u8; 4096];
                    let count = stream.read(&mut chunk).await.unwrap();
                    assert!(count > 0);
                    buf.extend_from_slice(&chunk[..count]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        header_end = pos + 4;
                        break;
                    }
                }
                let headers = String::from_utf8_lossy(&buf[..header_end]).to_ascii_lowercase();
                assert!(headers.starts_with("post /i/v0/e/ http/1.1\r\n"));
                let len: usize = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .unwrap()
                    .parse()
                    .unwrap();
                while buf.len() < header_end + len {
                    let mut chunk = [0u8; 4096];
                    let count = stream.read(&mut chunk).await.unwrap();
                    assert!(count > 0);
                    buf.extend_from_slice(&chunk[..count]);
                }
                bodies.push(buf[header_end..header_end + len].to_vec());
                stream
                    .write_all(
                        format!(
                            "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            }
            bodies
        });
        let id = Uuid::new_v4();
        let bytes =
            serde_json::to_vec(&serde_json::json!({"uuid":id,"event":"cja_manifest"})).unwrap();
        tokio::time::timeout(Duration::from_secs(4), send(bytes.clone(), host))
            .await
            .unwrap();
        let received = server.await.unwrap();
        assert_eq!(received, [bytes.clone(), bytes]);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&received[0]).unwrap()["uuid"],
            id.to_string()
        );
    }

    #[tokio::test]
    async fn permanent_capture_rejections_stop() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        for status in ["400 Bad Request", "413 Payload Too Large"] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let host = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = [0u8; 4096];
                let n = stream.read(&mut bytes).await.unwrap();
                assert!(bytes[..n].starts_with(b"POST /i/v0/e/ HTTP/1.1"));
                stream
                    .write_all(
                        format!(
                            "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
                tokio::time::timeout(Duration::from_millis(1200), listener.accept())
                    .await
                    .is_err()
            });
            tokio::time::timeout(Duration::from_secs(2), send(b"{}".to_vec(), host))
                .await
                .unwrap();
            assert!(server.await.unwrap(), "{status} was retried");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn retry_queue_expires_after_twenty_four_hours() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        let (sent, received) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = [0u8; 4096];
            let count = stream.read(&mut bytes).await.unwrap();
            assert!(bytes[..count].starts_with(b"POST /i/v0/e/ HTTP/1.1"));
            stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
            sent.send(()).unwrap();
            tokio::time::timeout(Duration::from_secs(5), listener.accept())
                .await
                .is_err()
        });
        let sender = tokio::spawn(send(b"{}".to_vec(), host));
        received.await.unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_hours(24)).await;
        sender.await.unwrap();
        assert!(server.await.unwrap(), "capture retried after queue expiry");
    }

    #[test]
    fn unreachable_capture_does_not_delay_boot_over_one_second() {
        use std::{
            process::{Command, Stdio},
            time::Instant,
        };
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let stalled = format!("http://{}", listener.local_addr().unwrap());
        let _server = std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                std::thread::sleep(Duration::from_secs(3));
                drop(stream);
            }
        });
        for (host, token) in [
            ("http://127.0.0.1:1".to_owned(), false),
            ("http://127.0.0.1:1".to_owned(), true),
            (stalled, true),
        ] {
            let start = Instant::now();
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "posthog_manifest::tests::child_boot_marker",
                    "--nocapture",
                ])
                .env("CJA_MANIFEST_CHILD", "1")
                .env("POSTHOG_HOST", &host)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if token {
                command.env("POSTHOG_PROJECT_TOKEN", "test-token");
            } else {
                command.env_remove("POSTHOG_PROJECT_TOKEN");
            }
            let mut child = command.spawn().unwrap();
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    let output = child.wait_with_output().unwrap();
                    assert!(
                        status.success(),
                        "child failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                    assert!(output.stdout.windows(10).any(|w| w == b"BOOT_READY"));
                    assert!(
                        start.elapsed() < Duration::from_secs(1),
                        "boot latency exceeded one second for {host}"
                    );
                    break;
                }
                if start.elapsed() >= Duration::from_secs(1) {
                    child.kill().unwrap();
                    let output = child.wait_with_output().unwrap();
                    panic!(
                        "boot latency exceeded one second for {host}: {}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}
