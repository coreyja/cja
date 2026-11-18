//! Reconcile one application's declarations against the `PostHog` project API.
use super::{
    Alert, Comparison, Declarations, FilterOperator, FilterValue, MetricQuery, ProductEventMetric,
};
use reqwest::{Client, Method};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
    process::ExitCode,
    time::Duration,
};

/// CI credentials and stable application identity.
pub struct SyncConfig {
    pub host: String,
    pub api_key: String,
    pub project_id: i64,
    pub app: String,
}
fn slug(app: &str) -> Result<(), String> {
    if app.is_empty()
        || app.starts_with('-')
        || !app
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(format!("invalid app slug {app:?}"));
    }
    Ok(())
}
impl SyncConfig {
    /// Validate the app slug and parse configuration, returning `None` for an unconfigured repo.
    pub fn from_parts(
        host: Option<String>,
        api_key: Option<String>,
        project_id: Option<String>,
        app: impl Into<String>,
    ) -> Result<Option<Self>, String> {
        let app = app.into();
        slug(&app)?;
        let (Some(api_key), Some(id)) = (
            api_key.filter(|v| !v.is_empty()),
            project_id.filter(|v| !v.is_empty()),
        ) else {
            return Ok(None);
        };
        Ok(Some(Self {
            host: host
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "https://us.posthog.com".into())
                .trim_end_matches('/')
                .into(),
            api_key,
            project_id: id
                .parse()
                .map_err(|_| format!("invalid POSTHOG_PROJECT_ID {id:?}"))?,
            app,
        }))
    }
    /// Read only the three CI configuration variables from the environment.
    pub fn from_env(app: impl Into<String>) -> Result<Option<Self>, String> {
        Self::from_parts(
            std::env::var("POSTHOG_API_HOST").ok(),
            std::env::var("POSTHOG_PERSONAL_API_KEY").ok(),
            std::env::var("POSTHOG_PROJECT_ID").ok(),
            app,
        )
    }
}
/// Build the TLS-enabled client used by the CI binary.
pub fn http_client() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("initialize HTTP client")
}
/// Changes and unsupported declarations encountered during reconciliation.
#[derive(Debug, Default)]
pub struct SyncReport {
    pub created: Vec<String>,
    pub updated: Vec<String>,
    pub deleted: Vec<String>,
    pub skipped: Vec<String>,
}
impl fmt::Display for SyncReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "created {} updated {} deleted {}",
            self.created.len(),
            self.updated.len(),
            self.deleted.len()
        )?;
        for warning in &self.skipped {
            writeln!(f, "warning: {warning}")?;
        }
        Ok(())
    }
}
/// Error while validating declarations, reading remote state, or applying a change.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("{object}: {method} {url} returned {status}: {body}")]
    Api {
        object: String,
        method: String,
        url: String,
        status: u16,
        body: String,
    },
    #[error("{object}: {operation}")]
    Http {
        object: String,
        operation: &'static str,
        #[source]
        source: reqwest::Error,
    },
    #[error("{object}: decoding response")]
    Decode {
        object: String,
        source: serde_json::Error,
    },
    #[error("{object}: PostHog did not persist our identity {tag}; check project tagging support")]
    TagNotPersisted { object: String, tag: String },
    #[error("{0}")]
    Invalid(String),
}
struct Api<'a> {
    config: &'a SyncConfig,
    http: &'a Client,
    base: String,
}
impl Api<'_> {
    async fn call(
        &self,
        method: Method,
        url: &str,
        body: Option<&Value>,
        object: &str,
    ) -> Result<Value, SyncError> {
        let error = |source| SyncError::Http {
            object: object.into(),
            operation: "requesting PostHog",
            source,
        };
        let mut req = self
            .http
            .request(method.clone(), url)
            .bearer_auth(&self.config.api_key)
            .header("Content-Type", "application/json");
        if let Some(v) = body {
            req = req.json(v);
        }
        let res = req.send().await.map_err(error)?;
        let status = res.status();
        let bytes = res.bytes().await.map_err(error)?;
        if !status.is_success() {
            return Err(SyncError::Api {
                object: object.into(),
                method: method.to_string(),
                url: url.into(),
                status: status.as_u16(),
                body: String::from_utf8_lossy(&bytes).chars().take(2048).collect(),
            });
        }
        if bytes.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&bytes).map_err(|source| SyncError::Decode {
            object: object.into(),
            source,
        })
    }
    async fn list(&self, kind: &str) -> Result<Vec<Value>, SyncError> {
        let path = format!("{}{kind}/", self.base);
        let mut url = format!("{path}?limit=100");
        let mut rows = Vec::new();
        let mut seen = BTreeSet::new();
        loop {
            if !seen.insert(url.clone()) {
                return Err(SyncError::Invalid(format!("{kind}: pagination cycle")));
            }
            let page = self.call(Method::GET, &url, None, kind).await?;
            rows.extend(
                page["results"]
                    .as_array()
                    .ok_or_else(|| SyncError::Invalid(format!("{kind}: missing results")))?
                    .iter()
                    .filter(|r| r["deleted"] != true)
                    .cloned(),
            );
            let next = match page.get("next") {
                Some(Value::Null) => break,
                Some(Value::String(next)) => next,
                _ => {
                    return Err(SyncError::Invalid(format!(
                        "{kind}: missing or invalid next page"
                    )));
                }
            };
            if next.split('?').next() != Some(path.as_str()) {
                return Err(SyncError::Invalid(format!(
                    "{kind}: unexpected pagination URL"
                )));
            }
            url.clone_from(next);
        }
        Ok(rows)
    }
    async fn write(
        &self,
        method: Method,
        path: &str,
        body: &Value,
        key: &str,
    ) -> Result<Value, SyncError> {
        self.call(method, &format!("{}{path}", self.base), Some(body), key)
            .await
    }
}
fn subset(wanted: &Value, actual: &Value) -> bool {
    match (wanted, actual) {
        (Value::Object(w), Value::Object(a)) => w
            .iter()
            .all(|(k, v)| a.get(k).is_some_and(|x| subset(v, x))),
        (Value::Array(w), Value::Array(a)) => {
            w.len() == a.len() && w.iter().zip(a).all(|(v, x)| subset(v, x))
        }
        _ => wanted == actual,
    }
}
fn interval(n: u64) -> usize {
    match n {
        0..=3600 => 0,
        3601..=86400 => 1,
        86401..=604_800 => 2,
        _ => 3,
    }
}
fn query(p: &ProductEventMetric, app: &str) -> Value {
    let (_, math, property) = p.aggregate.parts();
    let mut properties =
        vec![json!({"type":"event","key":"service.name","operator":"exact","value":app})];
    for f in &p.property_filters {
        let value = match &f.value {
            FilterValue::Bool(x) => json!(x),
            FilterValue::Int(x) => json!(x),
            FilterValue::Float(x) => json!(x),
            FilterValue::Str(x) => json!(x),
        };
        let operator = match f.operator {
            FilterOperator::Eq => "exact",
            FilterOperator::Gte => "gte",
        };
        properties.push(
            json!({"type": "event", "key": f.property, "operator": operator, "value": value}),
        );
    }
    let mut series =
        json!({"kind":"EventsNode","event":p.event,"math":math,"properties":properties});
    if let Some(prop) = property {
        series["math_property"] = json!(prop);
    }
    let bucket = ["hour", "day", "week", "month"][p.bucket_seconds.map_or(1, interval)];
    let mut source = json!({
        "kind": "TrendsQuery",
        "series": [series],
        "interval": bucket,
        "dateRange": {"date_from": "-30d"},
        "filterTestAccounts": false
    });
    if let Some(prop) = &p.breakdown_property {
        source["breakdownFilter"] = json!({"breakdowns":[{"property":prop,"type":"event"}]});
    }
    json!({"kind":"InsightVizNode","source":source})
}
fn bounds(a: &Alert) -> Value {
    let mut b = json!({});
    for v in [&a.warning, &a.critical].into_iter().flatten() {
        b[match v.comparison {
            Comparison::Above => "upper",
            Comparison::Below => "lower",
        }] = json!(v.threshold);
    }
    b
}
type Index = BTreeMap<String, Value>;
fn index(rows: Vec<Value>, prefix: &str) -> Result<Index, SyncError> {
    let mut idx = Index::new();
    for row in rows {
        let tags = row["tags"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|t| t.starts_with("cja:"))
            .collect::<Vec<_>>();
        if tags.len() > 1 && tags.iter().any(|tag| tag.starts_with(prefix)) {
            return Err(SyncError::Invalid(format!(
                "ambiguous ownership tags: {tags:?}"
            )));
        }
        if let [tag] = tags.as_slice()
            && tag.starts_with(prefix)
            && idx.insert((*tag).into(), row.clone()).is_some()
        {
            return Err(SyncError::Invalid(format!("duplicate identity {tag}")));
        }
    }
    Ok(idx)
}
fn id(row: &Value, key: &str) -> Result<i64, SyncError> {
    row["id"]
        .as_i64()
        .ok_or_else(|| SyncError::Invalid(format!("{key}: missing numeric ID")))
}
fn insight_id(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v["id"].as_i64())
}
async fn upsert(
    api: &Api<'_>,
    kind: &str,
    key: &str,
    mut body: Value,
    idx: &mut Index,
    report: &mut SyncReport,
) -> Result<i64, SyncError> {
    let old = idx.get(key);
    if let Some(row) = old
        && subset(&body, row)
        && (kind != "insights"
            || body.pointer("/query/source/breakdownFilter").is_some()
            || row
                .pointer("/query/source/breakdownFilter/breakdowns")
                .and_then(Value::as_array)
                .is_none_or(Vec::is_empty))
    {
        return id(row, key);
    }
    let (method, path) = if let Some(row) = old {
        body["tags"] = row["tags"].clone();
        (Method::PATCH, format!("{kind}/{}/", id(row, key)?))
    } else {
        body["tags"] = json!([key]);
        (Method::POST, format!("{kind}/"))
    };
    let row = api.write(method, &path, &body, key).await?;
    if !row["tags"]
        .as_array()
        .is_some_and(|t| t.iter().any(|x| x == key))
    {
        return Err(SyncError::TagNotPersisted {
            object: key.into(),
            tag: key.into(),
        });
    }
    let n = id(&row, key)?;
    if old.is_some() {
        report.updated.push(key.into());
    } else {
        report.created.push(key.into());
    }
    idx.insert(key.into(), row);
    Ok(n)
}
async fn tiles(
    api: &Api<'_>,
    key: &str,
    n: i64,
    wanted: &[i64],
    owned: &BTreeSet<i64>,
    r: &mut SyncReport,
) -> Result<(), SyncError> {
    let path = format!("dashboards/{n}/");
    let dashboard = api
        .call(Method::GET, &format!("{}{path}", api.base), None, key)
        .await?;
    let mut present = BTreeSet::new();
    for tile in dashboard["tiles"]
        .as_array()
        .ok_or_else(|| SyncError::Invalid(format!("{key}: missing tiles")))?
    {
        if tile["insight"]["deleted"] == true {
            continue;
        }
        if let Some(i) = insight_id(&tile["insight"]) {
            present.insert(i);
            if owned.contains(&i) && !wanted.contains(&i) {
                let tid = id(tile, key)?;
                api.write(
                    Method::POST,
                    &format!("{path}delete_tile/"),
                    &json!({"tile_id":tid}),
                    key,
                )
                .await?;
                r.updated.push(format!("{key}: removed tile {tid}"));
            }
        }
    }
    for i in wanted {
        if present.insert(*i) {
            api.write(
                Method::POST,
                &format!("{path}add_insight/"),
                &json!({"insight_id":i}),
                key,
            )
            .await?;
            r.updated.push(format!("{key}: added insight {i}"));
        }
    }
    Ok(())
}
fn alert_key<'a>(name: &'a str, app: &str) -> Option<&'a str> {
    let (_, key) = name
        .strip_suffix(']')?
        .rsplit_once(&format!("[cja:{app}:alert:"))?;
    (!key.is_empty() && !key.contains([']', '['])).then_some(key)
}
async fn alerts(
    api: &Api<'_>,
    decl: &Declarations,
    rows: Vec<Value>,
    ids: &BTreeMap<&str, i64>,
    r: &mut SyncReport,
) -> Result<(), SyncError> {
    let app = &api.config.app;
    let mut existing = Index::new();
    for row in rows {
        if let Some(k) = row["name"].as_str().and_then(|s| alert_key(s, app))
            && existing.insert(k.into(), row.clone()).is_some()
        {
            return Err(SyncError::Invalid(format!("duplicate alert {k}")));
        }
    }
    for a in &decl.alerts {
        let tag = format!("cja:{app}:alert:{}", a.id);
        let Some(n) = ids.get(a.metric_id.as_str()) else {
            r.skipped.push(format!("{tag}: telemetry metric"));
            continue;
        };
        let cadence =
            ["hourly", "daily", "weekly", "monthly"][interval(a.evaluation_interval_seconds)];
        let mut body = json!({
            "name": format!("{} [{tag}]", a.description.as_ref().unwrap_or(&a.id)),
            "insight": n,
            "enabled": a.enabled,
            "condition": {"type": "absolute_value"},
            "config": {"type": "TrendsAlertConfig", "series_index": 0},
            "threshold": {"configuration": {"type": "absolute", "bounds": bounds(a)}},
            "calculation_interval": cadence
        });
        if let Some(row) = existing.remove(&a.id) {
            let desired = &body["threshold"]["configuration"];
            let stored = &row["threshold"]["configuration"];
            if row["name"] == body["name"]
                && insight_id(&row["insight"]) == Some(*n)
                && row["enabled"] == body["enabled"]
                && row["calculation_interval"] == cadence
                && subset(desired, stored)
                && ["lower", "upper"]
                    .iter()
                    .all(|k| desired["bounds"][*k] == stored["bounds"][*k])
            {
                continue;
            }
            api.write(Method::PATCH, &alert_path(&row)?, &body, &tag)
                .await?;
            r.updated.push(tag);
        } else {
            // Creation requires a subscriber list. Only an operator edits it afterwards.
            body["subscribed_users"] = json!([]);
            api.write(Method::POST, "alerts/", &body, &tag).await?;
            r.created.push(tag);
        }
    }
    for (k, row) in existing {
        let key = format!("cja:{app}:alert:{k}");
        api.call(
            Method::DELETE,
            &format!("{}{}", api.base, alert_path(&row)?),
            None,
            &key,
        )
        .await?;
        r.deleted.push(key);
    }
    Ok(())
}
fn alert_path(row: &Value) -> Result<String, SyncError> {
    let id = row["id"]
        .as_str()
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
        .ok_or_else(|| SyncError::Invalid("invalid alert UUID".into()))?;
    Ok(format!("alerts/{id}/"))
}
/// Apply a complete declaration set, including removal of stale owned objects.
pub async fn sync(
    config: &SyncConfig,
    declarations: &Declarations,
    http: &Client,
) -> Result<SyncReport, SyncError> {
    slug(&config.app)
        .and_then(|()| declarations.validate())
        .map_err(SyncError::Invalid)?;
    let api = Api {
        config,
        http,
        base: format!("{}/api/projects/{}/", config.host, config.project_id),
    };
    let tag = |kind: &str, key: &str| format!("cja:{}:{kind}:{key}", config.app);
    let mut insights = index(api.list("insights").await?, &tag("insight", ""))?;
    let mut dashboards = index(api.list("dashboards").await?, &tag("dashboard", ""))?;
    let alert_rows = api.list("alerts").await?;
    let mut report = SyncReport::default();
    let mut keep = BTreeSet::new();
    let mut ids = BTreeMap::new();
    for dash in &declarations.dashboards {
        let key = tag("dashboard", &dash.id);
        let body = json!({"name": dash.title, "description": dash.description.as_deref().unwrap_or_default()});
        upsert(&api, "dashboards", &key, body, &mut dashboards, &mut report).await?;
        keep.insert(key);
    }
    for metric in &declarations.metrics {
        let key = tag("insight", &metric.id);
        let MetricQuery::ProductEvent(product) = &metric.query else {
            report.skipped.push(format!("{key}: telemetry metric"));
            continue;
        };
        let insight_id = upsert(
            &api,
            "insights",
            &key,
            json!({"name":metric.display_name.as_ref().unwrap_or(&metric.id),"query":query(product,&config.app)}),
            &mut insights,
            &mut report,
        )
        .await?;
        ids.insert(metric.id.as_str(), insight_id);
        keep.insert(key);
    }
    let owned = insights
        .iter()
        .map(|(key, v)| id(v, key))
        .collect::<Result<_, _>>()?;
    for dash in &declarations.dashboards {
        let key = tag("dashboard", &dash.id);
        let mut wanted = vec![];
        for item in dash.sections.iter().flat_map(|s| &s.items) {
            if let Some((mid, _)) = item.reference() {
                if let Some(insight_id) = ids.get(mid) {
                    wanted.push(*insight_id);
                }
            } else {
                report.skipped.push(format!("{key}: unsupported {item:?}"));
            }
        }
        tiles(
            &api,
            &key,
            id(&dashboards[&key], &key)?,
            &wanted,
            &owned,
            &mut report,
        )
        .await?;
    }
    alerts(&api, declarations, alert_rows, &ids, &mut report).await?;
    for (kind, rows) in [("dashboards", dashboards), ("insights", insights)] {
        for (key, row) in rows {
            if !keep.contains(&key) {
                api.write(
                    Method::PATCH,
                    &format!("{kind}/{}/", id(&row, &key)?),
                    &json!({"deleted":true}),
                    &key,
                )
                .await?;
                report.deleted.push(key);
            }
        }
    }
    Ok(report)
}
fn message(error: &dyn Error) -> String {
    let mut line = error.to_string();
    let mut source = error.source();
    while let Some(e) = source {
        line.push_str(": ");
        line.push_str(&e.to_string());
        source = e.source();
    }
    line.replace(['\n', '\r'], " ")
}
/// Run the CI command and translate its result to a process exit status.
pub async fn run_from_env(app: &str, d: &Declarations) -> ExitCode {
    if let Err(e) = d.validate() {
        eprintln!("invalid declarations: {e}");
        return ExitCode::from(2);
    }
    match SyncConfig::from_env(app) {
        Ok(Some(c)) => match sync(&c, d, &http_client()).await {
            Ok(r) => {
                println!("{r}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{}", message(&e));
                ExitCode::FAILURE
            }
        },
        Ok(None) => {
            println!("::notice::PostHog sync skipped: credentials or project ID absent");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn configuration_and_ownership() {
        for app in ["", "ABC", "-bad", "demo:bad", "a]"] {
            assert!(SyncConfig::from_parts(None, None, None, app).is_err());
        }
        for key in [None, Some(String::new())] {
            assert!(
                SyncConfig::from_parts(None, key, Some("1".into()), "demo")
                    .unwrap()
                    .is_none()
            );
        }
        let config = |host, id: &str| {
            SyncConfig::from_parts(host, Some("key".into()), Some(id.into()), "demo-1")
        };
        assert_eq!(
            config(Some(String::new()), "17").unwrap().unwrap().host,
            "https://us.posthog.com"
        );
        assert_eq!(
            config(Some("https://example.org/".into()), "17")
                .unwrap()
                .unwrap()
                .host,
            "https://example.org"
        );
        assert!(config(None, "bad").is_err());
        assert_eq!(
            alert_key("Human [cja:demo:alert:jobs]", "demo"),
            Some("jobs")
        );
        assert_eq!(alert_key("Human [cja:demo:alert:jobs]", "other"), None);
    }
    #[test]
    fn subset_accounts_for_server_defaults() {
        let desired = json!({"series":[{"event":"e"}]});
        assert!(subset(
            &desired,
            &json!({"version":2,"series":[{"event":"e","id":1}]})
        ));
        for other in [
            json!({}),
            json!({"series":[]}),
            json!({"series":[{"event":"x"}]}),
        ] {
            assert!(!subset(&desired, &other));
        }
    }
    #[test]
    fn bucket_boundaries() {
        for (n, i) in [
            (300, 0),
            (3600, 0),
            (3601, 1),
            (86400, 1),
            (86401, 2),
            (604_800, 2),
            (604_801, 3),
        ] {
            assert_eq!(interval(n), i);
        }
    }
    #[tokio::test]
    async fn https_error_prints_os_cause() {
        let source = http_client()
            .get("https://127.0.0.1:1/")
            .send()
            .await
            .unwrap_err();
        let err = SyncError::Http {
            object: "test-app".into(),
            operation: "listing",
            source,
        };
        let text = message(&err);
        assert!(text.contains("test-app: listing"));
        assert!(text.to_lowercase().contains("connection refused"), "{text}");
        assert!(!text.contains("scheme is not http"));
        assert!(!text.contains('\n'));
    }
}

#[cfg(test)]
mod mapping_tests {
    use super::*;
    use crate::observability::{AlertBuilder, ProductMetricBuilder};
    #[test]
    fn product_trend_wire_contract() {
        let metric = ProductMetricBuilder::p95("time", "opened", "seconds")
            .filter("paid", FilterOperator::Eq, true)
            .filter("attempt", FilterOperator::Gte, 2_i64)
            .breakdown("tier")
            .bucket_seconds(300)
            .build()
            .unwrap();
        let MetricQuery::ProductEvent(spec) = metric.query else {
            panic!("expected product metric")
        };
        assert_eq!(
            query(&spec, "demo"),
            json!({
 "kind":"InsightVizNode","source":{"kind":"TrendsQuery",
 "series":[{"kind":"EventsNode","event":"opened","math":"p95","math_property":"seconds","properties":[
 {"type":"event","key":"service.name","operator":"exact","value":"demo"},
 {"type":"event","key":"paid","operator":"exact","value":true},
 {"type":"event","key":"attempt","operator":"gte","value":2}]}],
 "interval":"hour","dateRange":{"date_from":"-30d"},"filterTestAccounts":false,
 "breakdownFilter":{"breakdowns":[{"property":"tier","type":"event"}]}}})
        );
        let count = ProductMetricBuilder::count("sessions", "opened")
            .build()
            .unwrap();
        let MetricQuery::ProductEvent(spec) = count.query else {
            panic!("expected product metric")
        };
        let q = query(&spec, "demo");
        assert_eq!(q["source"]["interval"], "day");
        assert_eq!(q["source"]["series"][0]["math"], "total");
        assert!(q["source"].get("breakdownFilter").is_none());
        assert!(q["source"]["series"][0].get("math_property").is_none());
    }
    #[test]
    fn critical_overrides_only_its_own_direction() {
        let builder = || {
            AlertBuilder::new("count", "activity")
                .window_seconds(60)
                .evaluation_interval_seconds(60)
        };
        for (warn, crit, wanted) in [
            (Comparison::Above, Comparison::Above, json!({"upper":4.})),
            (Comparison::Below, Comparison::Below, json!({"lower":4.})),
            (
                Comparison::Below,
                Comparison::Above,
                json!({"lower":2.,"upper":4.}),
            ),
            (
                Comparison::Above,
                Comparison::Below,
                json!({"upper":2.,"lower":4.}),
            ),
        ] {
            assert_eq!(
                bounds(
                    &builder()
                        .warning(warn, 2.)
                        .critical(crit, 4.)
                        .build()
                        .unwrap()
                ),
                wanted
            );
        }
        assert_eq!(
            bounds(&builder().warning(Comparison::Below, 2.).build().unwrap()),
            json!({"lower":2.})
        );
    }
}
