//! Wire-level reconciliation using the same declarations as the example binary.
#[path = "../../cja.app/src/observability.rs"]
mod example;

use axum::{
    Json, Router,
    body::Bytes,
    extract::{OriginalUri, State},
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
};
use cja::observability::{
    AggregateFunction, AlertBuilder, Comparison, Dashboard, Declarations, FilterOperator, Item,
    Link, MetricQuery, ProductMetricBuilder, PropertyFilter, RatioMetricBuilder, Section,
    TelemetryMetricBuilder,
    posthog::{self, SyncConfig, SyncError, SyncReport},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Default)]
struct Store {
    rows: BTreeMap<(String, String), Value>,
    log: Vec<(Method, String, Value)>,
    next: u32,
    drop_tag: bool,
    fail: bool,
    fail_cleanup: bool,
    /// Kind whose next create comes back with its identity mangled.
    mangle: Option<&'static str>,
}
type Shared = Arc<Mutex<Store>>;
struct Mock {
    state: Shared,
    host: String,
}
impl Mock {
    async fn start() -> Self {
        let state = Shared::default();
        let router = Router::new().fallback(handle).with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self { state, host }
    }
    async fn sync(&self, app: &str, d: &Declarations) -> Result<SyncReport, SyncError> {
        let config = SyncConfig::from_parts(
            Some(self.host.clone()),
            Some("test".into()),
            Some("1".into()),
            app,
        )
        .unwrap()
        .unwrap();
        tokio::time::timeout(
            Duration::from_secs(5),
            posthog::sync(&config, d, &posthog::SyncClient::new().unwrap()),
        )
        .await
        .unwrap()
    }
    fn log(&self) -> Vec<(Method, String, Value)> {
        std::mem::take(&mut self.state.lock().unwrap().log)
    }
    /// Drain the request log, keeping only the writes.
    fn writes(&self) -> Vec<(Method, String, Value)> {
        self.log()
            .into_iter()
            .filter(|(method, _, _)| method != Method::GET)
            .collect()
    }
    fn row(&self, key: &str) -> Value {
        self.state
            .lock()
            .unwrap()
            .rows
            .values()
            .find(|r| r["tags"] == json!([key]) && r["deleted"] != true)
            .unwrap()
            .clone()
    }
}
fn one_metric(item: Option<Item>, default: Option<u64>) -> Declarations {
    let mut dashboard = Dashboard::new("activity", "Activity").unwrap();
    if let Some(seconds) = default {
        dashboard = dashboard.default_range_seconds(seconds);
    }
    if let Some(item) = item {
        dashboard = dashboard.section(Section::new().item(item));
    }
    Declarations::new()
        .metric(
            ProductMetricBuilder::count("views", "page_view")
                .build()
                .unwrap(),
        )
        .dashboard(dashboard)
}
fn trend(event: &str, display: Option<&str>, seconds: u64) -> Value {
    let mut source = json!({
        "kind":"TrendsQuery",
        "series":[{"kind":"EventsNode","event":event,"math":"total","properties":[
            {"type":"event","key":"service.name","operator":"exact","value":"demo"}
        ]}],
        "interval":"day","dateRange":{"date_from":"-30d"},"filterTestAccounts":false
    });
    if let Some(display) = display {
        let range = if seconds.is_multiple_of(86_400) {
            format!("-{}d", seconds / 86_400)
        } else {
            format!("-{seconds}s")
        };
        source["dateRange"] = json!({"date_from":range,"explicitDate":true});
        source["trendsFilter"] = json!({"display":display});
    }
    json!({"kind":"InsightVizNode","source":source})
}
fn page_query(display: Option<&str>) -> Value {
    let mut query = trend("page_view", display, 86_400);
    query["source"]["breakdownFilter"] = json!({"breakdowns":[{"property":"path","type":"event"}]});
    query
}
fn mock_legacy(event: &str) -> Value {
    let mut query = trend(event, None, 0);
    query["source"]["trendsFilter"] = json!({});
    query["source"]["version"] = json!(2);
    query
}
async fn assert_repeat_clean(mock: &Mock, d: &Declarations) {
    let report = mock.sync("demo", d).await.unwrap();
    assert_eq!(mock.writes(), []);
    assert!(report.created.is_empty() && report.updated.is_empty() && report.deleted.is_empty());
}
fn assert_presentation(row: &Value, display: &str, date_from: &str) {
    assert_eq!(row["query"]["source"]["trendsFilter"]["display"], display);
    assert_eq!(
        row["query"]["source"]["dateRange"],
        json!({"date_from":date_from,"explicitDate":true})
    );
}
/// Create one row the way `PostHog` does, including its server defaults.
fn create(s: &mut Store, kind: &str, mut body: Value) -> Result<Value, &'static str> {
    if s.mangle == Some(kind) {
        s.mangle = None;
        if kind == "alerts" {
            body["name"] = json!("renamed by the server");
        } else {
            body["tags"] = json!(["cja:mangled"]);
        }
    }
    if kind == "alerts" && !body["subscribed_users"].is_array() {
        return Err("subscribed_users is required");
    }
    s.next += 1;
    let n = s.next;
    let key = if kind == "alerts" {
        uuid::Uuid::from_u128(n.into()).to_string()
    } else {
        n.to_string()
    };
    body["id"] = if kind == "alerts" {
        json!(key)
    } else {
        json!(n)
    };
    if kind == "dashboards" {
        body["tiles"] = json!([]);
    }
    if kind == "insights" {
        body["query"]["source"]["version"] = json!(2);
        if body["query"]["source"].get("trendsFilter").is_none() {
            body["query"]["source"]["trendsFilter"] = json!({});
        }
    }
    s.rows.insert((kind.into(), key), body.clone());
    Ok(body)
}

async fn handle(
    State(shared): State<Shared>,
    OriginalUri(uri): OriginalUri,
    method: Method,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    assert_eq!(headers["authorization"], "Bearer test");
    let path = uri
        .path()
        .strip_prefix("/api/projects/1/")
        .or_else(|| uri.path().strip_prefix("/api/environments/1/"))
        .unwrap();
    let parts = path.trim_end_matches('/').split('/').collect::<Vec<_>>();
    let mut body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    // PostHog normalizes every tag on write: `tag.strip().lower()`.
    if let Some(tags) = body.get_mut("tags").and_then(Value::as_array_mut) {
        for tag in tags {
            *tag = json!(tag.as_str().unwrap().trim().to_lowercase());
        }
    }
    let mut s = shared.lock().unwrap();
    s.log.push((method.clone(), path.into(), body.clone()));
    if method != Method::GET && std::mem::take(&mut s.fail) {
        return (StatusCode::BAD_REQUEST, Json(json!({"detail":"bad"}))).into_response();
    }
    if s.fail_cleanup
        && (method == Method::DELETE || (method == Method::PATCH && body["deleted"] == true))
    {
        s.fail_cleanup = false;
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"detail":"cleanup denied"})),
        )
            .into_response();
    }
    if method != Method::GET && std::mem::take(&mut s.drop_tag) {
        body["tags"] = json!([]);
    }
    let result = match (method, parts.as_slice()) {
        (Method::GET, [kind]) => {
            let results = s
                .rows
                .iter()
                .filter(|((k, _), _)| k == kind)
                .map(|(_, v)| v.clone())
                .collect::<Vec<_>>();
            list_page(&results, &uri, &headers, kind)
        }
        (Method::POST, [kind]) => match create(&mut s, kind, body) {
            Ok(row) => row,
            Err(detail) => {
                return (StatusCode::BAD_REQUEST, Json(json!({"detail":detail}))).into_response();
            }
        },
        (Method::GET, [kind, id]) => s.rows[&((*kind).into(), (*id).into())].clone(),
        (Method::PATCH, [kind, id]) => {
            let row = s.rows.get_mut(&((*kind).into(), (*id).into())).unwrap();
            row.as_object_mut()
                .unwrap()
                .extend(body.as_object().unwrap().clone());
            row.clone()
        }
        (Method::DELETE, ["alerts", id]) => {
            s.rows.remove(&("alerts".into(), (*id).into())).unwrap();
            return StatusCode::NO_CONTENT.into_response();
        }
        (Method::POST, ["dashboards", id, action]) => {
            s.next += 1;
            let tid = s.next;
            let tiles = s
                .rows
                .get_mut(&("dashboards".into(), (*id).into()))
                .unwrap()["tiles"]
                .as_array_mut()
                .unwrap();
            match *action {
                "add_insight" => {
                    if !tiles
                        .iter()
                        .any(|t| t["insight"]["id"] == body["insight_id"])
                    {
                        tiles.push(json!({"id":tid,"insight":{"id":body["insight_id"]}}));
                    }
                }
                "delete_tile" => tiles.retain(|t| t["id"] != body["tile_id"]),
                _ => panic!("unknown action"),
            }
            json!({})
        }
        _ => return StatusCode::METHOD_NOT_ALLOWED.into_response(),
    };
    Json(result).into_response()
}

#[tokio::test]
async fn reconcile_example_and_repeat() {
    let m = Mock::start().await;
    let d = example::declarations().unwrap();
    m.sync("demo", &d).await.unwrap();
    let writes = m
        .log()
        .into_iter()
        .filter(|(v, _, _)| v != Method::GET)
        .collect::<Vec<_>>();
    assert_eq!(writes.len(), 9);
    assert_eq!(writes[0].1, "dashboards/");
    assert_eq!(writes[1].1, "insights/");
    assert_eq!(writes[8].1, "alerts/");
    assert_eq!(
        writes[8].2["threshold"]["configuration"],
        json!({"type":"absolute","bounds":{"lower":1.}})
    );
    assert_eq!(writes[8].2["subscribed_users"], json!([]));
    assert_eq!(
        writes[1].2["query"]["source"]["breakdownFilter"]["breakdowns"][0]["property"],
        "path"
    );
    let report = m.sync("demo", &d).await.unwrap();
    assert!(m.log().iter().all(|(v, _, _)| v == Method::GET));
    assert!(report.created.is_empty() && report.updated.is_empty() && report.deleted.is_empty());
}

#[tokio::test]
async fn changed_declarations_patch_in_place_and_keep_foreign_tags() {
    let m = Mock::start().await;
    let mut d = example::declarations().unwrap();
    m.sync("demo", &d).await.unwrap();
    let insight = m.row("cja:demo:insight:pages")["id"].clone();
    let dashboard = m.row("cja:demo:dashboard:activity")["id"].clone();
    // Someone tagged both objects by hand in PostHog.
    for (kind, id) in [("insights", &insight), ("dashboards", &dashboard)] {
        let mut state = m.state.lock().unwrap();
        let row = state.rows.get_mut(&(kind.into(), id.to_string())).unwrap();
        row["tags"]
            .as_array_mut()
            .unwrap()
            .insert(0, json!("team-growth"));
    }
    m.log();
    let service = json!({"type":"event","key":"service.name","operator":"exact","value":"demo"});
    let query = |properties: Value| {
        json!({"kind":"InsightVizNode","source":{
            "kind":"TrendsQuery",
            "series":[{"kind":"EventsNode","event":"page_view","math":"total","properties":properties}],
            "interval":"day",
            "dateRange":{"date_from":"-1d","explicitDate":true},
            "trendsFilter":{"display":"ActionsTable"},
            "filterTestAccounts":false,
            "breakdownFilter":{"breakdowns":[{"property":"path","type":"event"}]}
        }})
    };

    d.metrics[0].display_name = Some("Views".into());
    d.dashboards[0].title = "Usage".into();
    let report = m.sync("demo", &d).await.unwrap();
    assert_eq!(
        report.updated,
        ["cja:demo:dashboard:activity", "cja:demo:insight:pages"]
    );
    assert!(report.created.is_empty() && report.deleted.is_empty());
    assert_eq!(
        m.writes(),
        [
            (
                Method::PATCH,
                format!("dashboards/{dashboard}/"),
                json!({
                    "name":"Usage",
                    "description":"",
                    "tags":["team-growth","cja:demo:dashboard:activity"]
                })
            ),
            (
                Method::PATCH,
                format!("insights/{insight}/"),
                json!({
                    "name":"Views",
                    "query":query(json!([service])),
                    "tags":["team-growth","cja:demo:insight:pages"]
                })
            ),
        ]
    );

    // A property-filter change alone also updates the insight in place.
    let MetricQuery::ProductEvent(product) = &mut d.metrics[0].query else {
        panic!("expected a product metric")
    };
    product.property_filters.push(PropertyFilter {
        property: "plan".into(),
        operator: FilterOperator::Eq,
        value: "pro".into(),
    });
    let report = m.sync("demo", &d).await.unwrap();
    assert_eq!(report.updated, ["cja:demo:insight:pages"]);
    let plan = json!({"type":"event","key":"plan","operator":"exact","value":"pro"});
    assert_eq!(
        m.writes(),
        [(
            Method::PATCH,
            format!("insights/{insight}/"),
            json!({
                "name":"Views",
                "query":query(json!([service, plan])),
                "tags":["team-growth","cja:demo:insight:pages"]
            })
        )]
    );
    m.sync("demo", &d).await.unwrap();
    assert_eq!(m.writes(), []);
}

#[tokio::test]
async fn unchanged_alert_with_an_embedded_insight_object_makes_no_writes() {
    let m = Mock::start().await;
    let d = example::declarations().unwrap();
    m.sync("demo", &d).await.unwrap();
    {
        // PostHog returns the alert's insight as an object, not a bare id.
        let mut state = m.state.lock().unwrap();
        let (_, row) = state
            .rows
            .iter_mut()
            .find(|((kind, _), _)| kind == "alerts")
            .unwrap();
        row["insight"] = json!({"id":row["insight"],"short_id":"aBcD1234","name":"daily"});
    }
    m.log();
    let report = m.sync("demo", &d).await.unwrap();
    assert_eq!(m.writes(), []);
    assert!(report.created.is_empty() && report.updated.is_empty() && report.deleted.is_empty());
}

#[tokio::test]
async fn removing_a_breakdown_updates_the_insight() {
    let m = Mock::start().await;
    let mut d = example::declarations().unwrap();
    m.sync("demo", &d).await.unwrap();
    let insight = m.row("cja:demo:insight:pages")["id"].clone();
    m.log();
    let MetricQuery::ProductEvent(product) = &mut d.metrics[0].query else {
        panic!("expected a product metric")
    };
    assert_eq!(product.breakdown_property.take().as_deref(), Some("path"));
    // Without its breakdown the metric is a scalar, so it needs a stat tile.
    d.dashboards[0].sections[0].items[0] = Item::stat("pages");
    let report = m.sync("demo", &d).await.unwrap();
    assert_eq!(report.updated, ["cja:demo:insight:pages"]);
    let writes = m.writes();
    assert_eq!(writes.len(), 1, "{writes:?}");
    let (method, path, body) = &writes[0];
    assert_eq!(
        (method, path),
        (&Method::PATCH, &format!("insights/{insight}/"))
    );
    assert_eq!(body["query"]["source"]["kind"], "TrendsQuery");
    assert!(body["query"]["source"].get("breakdownFilter").is_none());
    assert!(
        m.row("cja:demo:insight:pages")["query"]["source"]
            .get("breakdownFilter")
            .is_none()
    );
    m.sync("demo", &d).await.unwrap();
    assert_eq!(m.writes(), []);
}

#[tokio::test]
async fn prune_preserves_other_apps_and_hand_tiles() {
    let m = Mock::start().await;
    let mut d = example::declarations().unwrap();
    m.sync("demo", &d).await.unwrap();
    m.sync("demo-b", &d).await.unwrap();
    let other = m.row("cja:demo-b:dashboard:activity");
    let dashboard = m.row("cja:demo:dashboard:activity");
    let old = m.row("cja:demo:insight:pages")["id"].clone();
    {
        let mut s = m.state.lock().unwrap();
        s.rows.insert(
            ("insights".into(), "999".into()),
            json!({"id":999,"name":"Handmade","tags":[]}),
        );
        s.rows.insert(
            ("alerts".into(), "custom".into()),
            json!({"id":"custom","name":"Handmade","insight":old}),
        );
        let tiles = s
            .rows
            .get_mut(&("dashboards".into(), dashboard["id"].to_string()))
            .unwrap()["tiles"]
            .as_array_mut()
            .unwrap();
        tiles.extend([
            json!({"id":12345,"text":"keep"}),
            json!({"id":12346,"insight":999}),
        ]);
    }
    d.metrics.remove(0);
    assert!(matches!(
        d.dashboards[0].sections[0].items.remove(0),
        Item::Table { .. }
    ));
    m.sync("demo", &d).await.unwrap();
    let tiles = m.row("cja:demo:dashboard:activity")["tiles"].clone();
    assert_eq!(tiles.as_array().unwrap().len(), 4);
    assert!(
        tiles
            .as_array()
            .unwrap()
            .contains(&json!({"id":12346,"insight":999}))
    );
    let report = m.sync("demo", &Declarations::new()).await.unwrap();
    assert!(
        report
            .deleted
            .contains(&"cja:demo:dashboard:activity".into())
    );
    assert!(
        report
            .deleted
            .iter()
            .any(|k| k.starts_with("cja:demo:alert:"))
    );
    let writes = m.log();
    assert!(
        writes
            .iter()
            .any(|(method, path, body)| method == Method::PATCH
                && path.starts_with("dashboards/")
                && body == &json!({"deleted":true}))
    );
    assert!(
        writes
            .iter()
            .any(|(method, path, _)| method == Method::DELETE
                && path.starts_with("alerts/")
                && !path.contains("custom"))
    );
    assert!(
        !writes
            .iter()
            .any(|(method, path, _)| method == Method::DELETE
                && (path.starts_with("insights/") || path.starts_with("dashboards/")))
    );
    assert!(!writes.iter().any(|(method, path, _)| method != Method::GET
        && (path == "insights/999/" || path == "alerts/custom/")));

    let d = example::declarations().unwrap();
    m.sync("demo", &d).await.unwrap();
    assert_ne!(old, m.row("cja:demo:insight:pages")["id"]);
    assert_eq!(other, m.row("cja:demo-b:dashboard:activity"));
    m.log();
    m.sync("demo-b", &d).await.unwrap();
    m.sync("demo", &d).await.unwrap();
    assert!(m.log().iter().all(|(v, _, _)| v == Method::GET));
    let s = m.state.lock().unwrap();
    assert_eq!(
        s.rows[&("insights".into(), old.to_string())]["deleted"],
        true
    );
    assert_eq!(
        s.rows[&("insights".into(), "999".into())]["name"],
        "Handmade"
    );
    assert_eq!(s.rows[&("alerts".into(), "custom".into())]["insight"], old);
}

#[tokio::test]
async fn surfaces_rejected_write_and_missing_tag() {
    let m = Mock::start().await;
    let d = example::declarations().unwrap();
    m.state.lock().unwrap().fail = true;
    let e = m.sync("demo", &d).await.unwrap_err();
    assert!(matches!(e, SyncError::Api { status: 400, .. }));
    for s in ["cja:demo:dashboard:activity", "POST", "bad"] {
        assert!(e.to_string().contains(s));
    }
    m.state.lock().unwrap().drop_tag = true;
    assert!(
        matches!(m.sync("demo",&d).await,Err(SyncError::TagNotPersisted{tag,..}) if tag=="cja:demo:dashboard:activity")
    );
}
#[tokio::test]
async fn eyes_only_declarations_are_skipped_with_named_warnings() {
    let m = Mock::start().await;
    let mut d = example::declarations().unwrap();
    d.dashboards[0].sections[0].items.extend([
        Item::health_summary(),
        Item::links(vec![Link::new("Runbook", "/runbook").unwrap()]),
        Item::stat("jobs"),
        Item::stat("share"),
    ]);
    let telemetry = |id: &str| {
        TelemetryMetricBuilder::new(id, AggregateFunction::Count, None)
            .unwrap()
            .build()
            .unwrap()
    };
    let alert = |id: &str, metric: &str| {
        AlertBuilder::new(id, metric)
            .window_seconds(60)
            .evaluation_interval_seconds(60)
            .critical(Comparison::Above, 1.)
            .build()
            .unwrap()
    };
    let d = d
        .metric(telemetry("jobs"))
        .metric(telemetry("all_jobs"))
        .metric(
            RatioMetricBuilder::new("share", "jobs", "all_jobs")
                .as_percentage()
                .build()
                .unwrap(),
        )
        .alert(alert("failed", "jobs"))
        .alert(alert("low-share", "share"));
    let r = m.sync("demo", &d).await.unwrap();
    assert_eq!(
        r.skipped,
        [
            "cja:demo:insight:jobs: telemetry metric (Eyes-only)",
            "cja:demo:insight:all_jobs: telemetry metric (Eyes-only)",
            "cja:demo:insight:share: ratio metric (Eyes-only)",
            "cja:demo:dashboard:activity: health summary item (Eyes-only)",
            "cja:demo:dashboard:activity: links item (Eyes-only)",
            "cja:demo:dashboard:activity: tile for telemetry metric jobs (Eyes-only)",
            "cja:demo:dashboard:activity: tile for ratio metric share (Eyes-only)",
            "cja:demo:alert:failed: telemetry metric jobs (Eyes-only)",
            "cja:demo:alert:low-share: ratio metric share (Eyes-only)",
        ]
    );
    assert_eq!(
        r.created,
        [
            "cja:demo:dashboard:activity",
            "cja:demo:insight:pages",
            "cja:demo:insight:sessions",
            "cja:demo:insight:daily",
            "cja:demo:insight:daily@86400s",
            "cja:demo:alert:quiet",
        ]
    );
    let writes = m
        .log()
        .into_iter()
        .filter(|(method, _, _)| method != Method::GET)
        .collect::<Vec<_>>();
    // Only the example's own alert is created: none for the telemetry or
    // ratio alert, and no insight or tile for an Eyes-only metric.
    let posts = |path: &str| {
        writes
            .iter()
            .filter(|(method, p, _)| method == Method::POST && p == path)
            .map(|(_, _, body)| body.clone())
            .collect::<Vec<_>>()
    };
    let alerts = posts("alerts/");
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0]["name"], "quiet [cja:demo:alert:quiet]");
    assert_eq!(posts("insights/").len(), 4);
    assert_eq!(
        writes
            .iter()
            .filter(|(_, path, _)| path.ends_with("/add_insight/"))
            .count(),
        3
    );
}

#[tokio::test]
async fn updates_alert_bounds_in_place_and_preserves_subscribers() {
    let mock = Mock::start().await;
    let mut declarations = example::declarations().unwrap();
    mock.sync("demo", &declarations).await.unwrap();
    mock.log();
    let key = {
        let mut state = mock.state.lock().unwrap();
        let (key, row) = state
            .rows
            .iter_mut()
            .find(|((kind, _), _)| kind == "alerts")
            .unwrap();
        row["subscribed_users"] = json!([42]);
        row["insight"] = json!({"id":row["insight"]});
        key.clone()
    };
    declarations.alerts[0].critical = Some(cja::observability::Severity {
        comparison: Comparison::Above,
        threshold: 50.,
    });
    let report = mock.sync("demo", &declarations).await.unwrap();
    assert_eq!(report.updated, ["cja:demo:alert:quiet"]);
    let writes = mock
        .log()
        .into_iter()
        .filter(|(method, _, _)| method != Method::GET)
        .collect::<Vec<_>>();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].0, Method::PATCH);
    assert_eq!(
        writes[0].2["threshold"]["configuration"]["bounds"],
        json!({"upper":50.,"lower":1.})
    );
    assert!(writes[0].2.get("subscribed_users").is_none());
    assert_eq!(
        mock.state.lock().unwrap().rows[&key]["subscribed_users"],
        json!([42])
    );
}

#[tokio::test]
async fn membership_changes_do_not_replace_insights() {
    let mock = Mock::start().await;
    let mut declarations = example::declarations().unwrap();
    mock.sync("demo", &declarations).await.unwrap();
    let page_id = mock.row("cja:demo:insight:pages")["id"].clone();
    let dashboard = mock.row("cja:demo:dashboard:activity");
    let tile_id = dashboard["tiles"][0]["id"].clone();
    mock.log();
    let item = declarations.dashboards[0].sections[0].items.remove(0);
    mock.sync("demo", &declarations).await.unwrap();
    let changes = mock
        .log()
        .into_iter()
        .filter(|(method, _, _)| method != Method::GET)
        .collect::<Vec<_>>();
    assert_eq!(
        changes,
        [
            (
                Method::PATCH,
                format!("insights/{page_id}/"),
                json!({"name":"pages","query":page_query(None),"tags":["cja:demo:insight:pages"]})
            ),
            (
                Method::POST,
                format!("dashboards/{}/delete_tile/", dashboard["id"]),
                json!({"tile_id":tile_id})
            )
        ]
    );
    declarations.dashboards[0].sections[0].items.push(item);
    mock.sync("demo", &declarations).await.unwrap();
    let changes = mock
        .log()
        .into_iter()
        .filter(|(method, _, _)| method != Method::GET)
        .collect::<Vec<_>>();
    assert_eq!(
        changes,
        [
            (
                Method::PATCH,
                format!("insights/{page_id}/"),
                json!({"name":"pages","query":page_query(Some("ActionsTable")),"tags":["cja:demo:insight:pages"]})
            ),
            (
                Method::POST,
                format!("dashboards/{}/add_insight/", dashboard["id"]),
                json!({"insight_id":page_id})
            )
        ]
    );
    assert_eq!(mock.row("cja:demo:insight:pages")["id"], page_id);
    assert_repeat_clean(&mock, &declarations).await;
}

#[tokio::test]
async fn kinds_and_ranges_persist_with_dashboard_defaults_and_item_override() {
    let mock = Mock::start().await;
    let weekly = ProductMetricBuilder::count("weekly", "week_view")
        .bucket_weeks(1)
        .build()
        .unwrap();
    let d = example::declarations()
        .unwrap()
        .metric(weekly)
        .dashboard(
            Dashboard::new("history", "History")
                .unwrap()
                .default_range_seconds(364 * 86_400)
                .section(Section::new().item(Item::time_series("weekly"))),
        )
        .dashboard(
            Dashboard::new("history-copy", "History copy")
                .unwrap()
                .default_range_seconds(364 * 86_400)
                .section(Section::new().item(Item::time_series("weekly"))),
        )
        .dashboard(
            Dashboard::new("week", "Week")
                .unwrap()
                .default_range_seconds(364 * 86_400)
                .section(Section::new().item(Item::time_series("weekly").window_seconds(604_800))),
        );
    mock.sync("demo", &d).await.unwrap();
    assert_presentation(&mock.row("cja:demo:insight:pages"), "ActionsTable", "-1d");
    assert_presentation(&mock.row("cja:demo:insight:sessions"), "BoldNumber", "-1d");
    assert_presentation(
        &mock.row("cja:demo:insight:weekly"),
        "ActionsLineGraph",
        "-364d",
    );
    assert_presentation(
        &mock.row("cja:demo:insight:weekly@604800s"),
        "ActionsLineGraph",
        "-7d",
    );
    assert_eq!(
        mock.row("cja:demo:insight:weekly")["query"]["source"]["interval"],
        "week"
    );
    assert_eq!(
        mock.row("cja:demo:dashboard:history")["tiles"][0]["insight"]["id"],
        mock.row("cja:demo:dashboard:history-copy")["tiles"][0]["insight"]["id"]
    );
    assert_eq!(
        mock.row("cja:demo:insight:daily")["query"],
        mock_legacy("session_created")
    );
    mock.log();
    assert_repeat_clean(&mock, &d).await;
}

#[tokio::test]
async fn invalid_windows_fail_before_any_api_request() {
    for seconds in [0, 59, 31_622_401] {
        let mock = Mock::start().await;
        let d = one_metric(Some(Item::stat("views")), Some(seconds));
        assert!(matches!(
            mock.sync("demo", &d).await,
            Err(SyncError::Invalid(_))
        ));
        assert_eq!(mock.log(), Vec::new());
    }
    for seconds in [0, 31_622_401] {
        let mock = Mock::start().await;
        let d = one_metric(Some(Item::stat("views").window_seconds(seconds)), None);
        assert!(matches!(
            mock.sync("demo", &d).await,
            Err(SyncError::Invalid(_))
        ));
        assert_eq!(mock.log(), Vec::new());
    }
    for seconds in [60, 31_622_400] {
        let mock = Mock::start().await;
        mock.sync("demo", &one_metric(None, Some(seconds)))
            .await
            .unwrap();
    }
    for seconds in [1, 31_622_400] {
        let mock = Mock::start().await;
        mock.sync(
            "demo",
            &one_metric(Some(Item::stat("views").window_seconds(seconds)), None),
        )
        .await
        .unwrap();
    }
    let mock = Mock::start().await;
    let d = one_metric(None, None);
    mock.sync("demo", &d).await.unwrap();
    assert_eq!(
        mock.row("cja:demo:insight:views")["query"],
        mock_legacy("page_view")
    );
    mock.log();
    assert_repeat_clean(&mock, &d).await;

    let mock = Mock::start().await;
    let mut d = example::declarations().unwrap();
    d.metrics.push(
        TelemetryMetricBuilder::new("jobs", AggregateFunction::Count, None)
            .unwrap()
            .build()
            .unwrap(),
    );
    d.dashboards[0].sections[0]
        .items
        .push(Item::stat("jobs").window_seconds(0));
    assert!(matches!(
        mock.sync("demo", &d).await,
        Err(SyncError::Invalid(_))
    ));
    assert_eq!(mock.log(), Vec::new());
}

#[tokio::test]
async fn overlong_variant_tag_fails_before_any_api_request() {
    let mock = Mock::start().await;
    let d = one_metric(Some(Item::stat("views").window_seconds(604_800)), None).dashboard(
        Dashboard::new("other", "Other")
            .unwrap()
            .section(Section::new().item(Item::stat("views").window_seconds(2_592_000))),
    );
    let error = mock.sync(&"a".repeat(240), &d).await.unwrap_err();
    assert!(matches!(error, SyncError::Invalid(message) if message.contains("variant tag")));
    assert_eq!(mock.log(), Vec::new());
}

fn views_alert() -> cja::observability::Alert {
    AlertBuilder::new("high", "views")
        .window_seconds(60)
        .evaluation_interval_seconds(60)
        .critical(Comparison::Above, 1.)
        .build()
        .unwrap()
}

#[tokio::test]
async fn adding_alert_rewrites_unalerted_30_day_stat_before_alert_creation() {
    let mock = Mock::start().await;
    let mut d = one_metric(Some(Item::stat("views")), None);
    mock.sync("demo", &d).await.unwrap();
    let canonical = mock.row("cja:demo:insight:views")["id"].clone();
    assert_presentation(&mock.row("cja:demo:insight:views"), "BoldNumber", "-30d");
    mock.log();
    d.alerts.push(views_alert());
    mock.sync("demo", &d).await.unwrap();
    let writes = mock.writes();
    assert_eq!(
        writes[0],
        (
            Method::PATCH,
            format!("insights/{canonical}/"),
            json!({"name":"views","query":trend("page_view", None, 0),"tags":["cja:demo:insight:views"]})
        )
    );
    assert_eq!(writes[1].0, Method::POST);
    assert_eq!(writes[1].1, "insights/");
    assert_eq!(
        writes[1].2["tags"],
        json!(["cja:demo:insight:views@2592000s"])
    );
    assert_eq!(
        writes
            .iter()
            .map(|(_, path, _)| path.as_str())
            .collect::<Vec<_>>(),
        [
            format!("insights/{canonical}/"),
            "insights/".into(),
            format!(
                "dashboards/{}/delete_tile/",
                mock.row("cja:demo:dashboard:activity")["id"]
            ),
            format!(
                "dashboards/{}/add_insight/",
                mock.row("cja:demo:dashboard:activity")["id"]
            ),
            "alerts/".into()
        ]
    );
    assert_eq!(mock.row("cja:demo:insight:views")["id"], canonical);
    assert_eq!(
        mock.row("cja:demo:insight:views")["query"],
        trend("page_view", None, 0)
    );
    assert_presentation(
        &mock.row("cja:demo:insight:views@2592000s"),
        "BoldNumber",
        "-30d",
    );
    assert_eq!(
        mock.state
            .lock()
            .unwrap()
            .rows
            .values()
            .find(|v| v["name"] == "high [cja:demo:alert:high]")
            .unwrap()["insight"],
        canonical
    );
    assert_repeat_clean(&mock, &d).await;
}

#[tokio::test]
async fn removing_only_item_rewrites_unalerted_30_day_stat() {
    let mock = Mock::start().await;
    let mut d = one_metric(Some(Item::stat("views")), None);
    mock.sync("demo", &d).await.unwrap();
    let canonical = mock.row("cja:demo:insight:views")["id"].clone();
    let dashboard = mock.row("cja:demo:dashboard:activity");
    mock.log();
    d.dashboards[0].sections[0].items.clear();
    mock.sync("demo", &d).await.unwrap();
    assert_eq!(
        mock.writes(),
        [
            (
                Method::PATCH,
                format!("insights/{canonical}/"),
                json!({"name":"views","query":trend("page_view", None, 0),"tags":["cja:demo:insight:views"]})
            ),
            (
                Method::POST,
                format!("dashboards/{}/delete_tile/", dashboard["id"]),
                json!({"tile_id":dashboard["tiles"][0]["id"]})
            )
        ]
    );
    assert_eq!(mock.row("cja:demo:insight:views")["id"], canonical);
    assert_repeat_clean(&mock, &d).await;
}

#[tokio::test]
async fn alerted_metric_keeps_canonical_query_and_alert_while_variants_change() {
    let mock = Mock::start().await;
    let mut d = one_metric(None, None).alert(views_alert());
    mock.sync("demo", &d).await.unwrap();
    let canonical = mock.row("cja:demo:insight:views")["id"].clone();
    mock.log();
    d.dashboards[0]
        .sections
        .push(Section::new().item(Item::stat("views").window_seconds(604_800)));
    mock.sync("demo", &d).await.unwrap();
    let variant = mock.row("cja:demo:insight:views@604800s")["id"].clone();
    assert_presentation(
        &mock.row("cja:demo:insight:views@604800s"),
        "BoldNumber",
        "-7d",
    );
    assert_eq!(
        mock.row("cja:demo:dashboard:activity")["tiles"][0]["insight"]["id"],
        variant
    );
    assert_eq!(
        mock.row("cja:demo:insight:views")["query"],
        mock_legacy("page_view")
    );
    assert!(
        !mock
            .writes()
            .iter()
            .any(|(_, path, _)| path == &format!("insights/{canonical}/") || path == "alerts/")
    );
    d.dashboards[0].sections[0].items[0] = Item::stat("views").window_seconds(2_592_000);
    mock.sync("demo", &d).await.unwrap();
    let writes = mock.writes();
    assert_eq!(writes.first().unwrap().1, "insights/");
    assert_eq!(
        writes.last().unwrap(),
        &(
            Method::PATCH,
            format!("insights/{variant}/"),
            json!({"deleted":true})
        )
    );
    assert_presentation(
        &mock.row("cja:demo:insight:views@2592000s"),
        "BoldNumber",
        "-30d",
    );
    assert_eq!(mock.row("cja:demo:insight:views")["id"], canonical);
    assert_repeat_clean(&mock, &d).await;
    let current_variant = mock.row("cja:demo:insight:views@2592000s")["id"].clone();
    d.dashboards[0].sections[0].items.clear();
    mock.sync("demo", &d).await.unwrap();
    let writes = mock.writes();
    assert_eq!(
        writes[0].1,
        format!(
            "dashboards/{}/delete_tile/",
            mock.row("cja:demo:dashboard:activity")["id"]
        )
    );
    assert_eq!(
        writes.last().unwrap(),
        &(
            Method::PATCH,
            format!("insights/{current_variant}/"),
            json!({"deleted":true})
        )
    );
    assert_eq!(mock.row("cja:demo:insight:views")["id"], canonical);
    assert_repeat_clean(&mock, &d).await;
}

async fn seed_two_windows(mock: &Mock, d: &Declarations) -> (Value, Value) {
    mock.sync("demo", d).await.unwrap();
    let canonical = mock.row("cja:demo:insight:views")["id"].clone();
    let old_variant = mock.row("cja:demo:insight:views@2592000s")["id"].clone();
    assert_presentation(&mock.row("cja:demo:insight:views"), "BoldNumber", "-7d");
    assert_presentation(
        &mock.row("cja:demo:insight:views@2592000s"),
        "BoldNumber",
        "-30d",
    );
    {
        let dashboard = mock.row("cja:demo:dashboard:activity");
        let mut state = mock.state.lock().unwrap();
        state
            .rows
            .get_mut(&(String::from("dashboards"), dashboard["id"].to_string()))
            .unwrap()["tiles"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id":9999,"text":"handmade"}));
    }
    mock.log();
    (canonical, old_variant)
}

#[tokio::test]
async fn two_windows_reorder_canonical_and_prune_obsolete_variant() {
    let mock = Mock::start().await;
    let mut d = one_metric(Some(Item::stat("views")), Some(604_800)).dashboard(
        Dashboard::new("month", "Month")
            .unwrap()
            .default_range_seconds(2_592_000)
            .section(Section::new().item(Item::stat("views"))),
    );
    let (canonical, old_variant) = seed_two_windows(&mock, &d).await;
    d.dashboards.swap(0, 1);
    mock.sync("demo", &d).await.unwrap();
    let new_variant = mock.row("cja:demo:insight:views@604800s")["id"].clone();
    let writes = mock.writes();
    assert_eq!(
        writes[0],
        (
            Method::PATCH,
            format!("insights/{canonical}/"),
            json!({"name":"views","query":trend("page_view", Some("BoldNumber"), 2_592_000),"tags":["cja:demo:insight:views"]})
        )
    );
    assert_eq!(writes[1].1, "insights/");
    assert_eq!(
        writes[1].2["tags"],
        json!(["cja:demo:insight:views@604800s"])
    );
    assert_eq!(
        writes.last().unwrap(),
        &(
            Method::PATCH,
            format!("insights/{old_variant}/"),
            json!({"deleted":true})
        )
    );
    assert_eq!(writes.len(), 7);
    let month = mock.row("cja:demo:dashboard:month");
    let activity = mock.row("cja:demo:dashboard:activity");
    assert_eq!(
        writes[2].1,
        format!("dashboards/{}/delete_tile/", month["id"])
    );
    assert_eq!(
        writes[3],
        (
            Method::POST,
            format!("dashboards/{}/add_insight/", month["id"]),
            json!({"insight_id":canonical})
        )
    );
    assert_eq!(
        writes[4].1,
        format!("dashboards/{}/delete_tile/", activity["id"])
    );
    assert_eq!(
        writes[5],
        (
            Method::POST,
            format!("dashboards/{}/add_insight/", activity["id"]),
            json!({"insight_id":new_variant})
        )
    );
    assert_eq!(month["tiles"][0]["insight"]["id"], canonical);
    assert!(
        activity["tiles"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tile| tile["insight"]["id"] == new_variant)
    );
    assert!(
        activity["tiles"]
            .as_array()
            .unwrap()
            .contains(&json!({"id":9999,"text":"handmade"}))
    );
    assert_repeat_clean(&mock, &d).await;
    d.dashboards[1].sections[0].items.clear();
    mock.sync("demo", &d).await.unwrap();
    let writes = mock.writes();
    assert_eq!(
        writes[0].1,
        format!("dashboards/{}/delete_tile/", activity["id"])
    );
    assert_eq!(
        writes.last().unwrap(),
        &(
            Method::PATCH,
            format!("insights/{new_variant}/"),
            json!({"deleted":true})
        )
    );
    assert_eq!(mock.row("cja:demo:insight:views")["id"], canonical);
    assert_repeat_clean(&mock, &d).await;
}

fn list_page(results: &[Value], uri: &axum::http::Uri, headers: &HeaderMap, kind: &str) -> Value {
    let offset = uri
        .query()
        .unwrap_or_default()
        .split('&')
        .find_map(|p| p.strip_prefix("offset="))
        .map_or(0, |n| n.parse().unwrap());
    let next = (offset + 2 < results.len()).then(|| {
        format!(
            "http://{}/api/environments/1/{kind}/?limit=2&offset={}",
            headers["host"].to_str().unwrap(),
            offset + 2
        )
    });
    let page: Vec<_> = results.iter().skip(offset).take(2).collect();
    json!({"count":results.len(),"results":page,"next":next})
}

#[tokio::test]
async fn invalid_declarations_fail_before_any_api_request() {
    let mock = Mock::start().await;
    let mut declarations = example::declarations().unwrap();
    let dashboard = declarations.dashboards[0].clone();
    assert!(
        mock.sync("demo", &declarations.clone().dashboard(dashboard))
            .await
            .is_err()
    );
    let duplicate = declarations.alerts[0].clone();
    assert!(
        mock.sync("demo", &declarations.clone().alert(duplicate))
            .await
            .is_err()
    );
    declarations.alerts[0].metric_id = "missing".into();
    assert!(mock.sync("demo", &declarations).await.is_err());
    declarations.alerts.clear();
    declarations.dashboards[0].sections[0]
        .items
        .push(Item::stat("missing"));
    assert!(mock.sync("demo", &declarations).await.is_err());
    assert_eq!(mock.log(), Vec::new());
}

#[test]
fn dashboard_limits_accept_the_exact_maximum() {
    use cja::observability::{Dashboard, Section};
    let mut declarations = Declarations::new();
    for i in 0..20 {
        declarations
            .dashboards
            .push(Dashboard::new(format!("dashboard-{i}"), "Activity").unwrap());
    }
    let mut section = Section::new();
    section.items = vec![Item::health_summary(); 24];
    declarations.dashboards[0].sections = vec![section.clone(); 20];
    declarations.validate().unwrap();
    let dashboard = Dashboard::new("extra", "Extra").unwrap();
    assert!(
        declarations
            .clone()
            .dashboard(dashboard)
            .validate()
            .is_err()
    );
    declarations.dashboards[0].sections.push(section);
    assert!(declarations.validate().is_err());
    declarations.dashboards[0].sections.pop();
    declarations.dashboards[0].sections[0]
        .items
        .push(Item::health_summary());
    assert!(declarations.validate().is_err());
}

#[tokio::test]
async fn unidentified_creates_are_removed_before_the_error_returns() {
    for (kind, tag) in [
        ("dashboards", "cja:demo:dashboard:activity"),
        ("insights", "cja:demo:insight:pages"),
        ("alerts", "cja:demo:alert:quiet"),
    ] {
        let mock = Mock::start().await;
        let declarations = example::declarations().unwrap();
        mock.state.lock().unwrap().mangle = Some(kind);
        let error = mock.sync("demo", &declarations).await.unwrap_err();
        let SyncError::TagNotPersisted {
            tag: reported,
            cleanup,
            ..
        } = &error
        else {
            panic!("{kind}: expected TagNotPersisted, got {error}")
        };
        assert_eq!(reported, tag);
        assert_eq!(cleanup, "the object just created was removed");
        let writes = mock
            .log()
            .into_iter()
            .filter(|(method, _, _)| method != Method::GET)
            .collect::<Vec<_>>();
        // The cleanup is the last write, straight after the mangled create.
        let [.., (create, create_path, _), (method, path, body)] = writes.as_slice() else {
            panic!("{kind}: expected a create and a cleanup, got {writes:?}")
        };
        assert_eq!(
            (create, create_path.as_str()),
            (&Method::POST, &*format!("{kind}/"))
        );
        let state = mock.state.lock().unwrap();
        if kind == "alerts" {
            assert_eq!(method, Method::DELETE);
            assert!(path.starts_with("alerts/"), "{path}");
            assert!(!state.rows.keys().any(|(k, _)| k == "alerts"));
        } else {
            assert_eq!(method, Method::PATCH);
            assert_eq!(body, &json!({"deleted":true}));
            let mangled = state
                .rows
                .iter()
                .find(|((k, _), row)| k == kind && row["tags"] == json!(["cja:mangled"]))
                .map(|((_, id), row)| (id.clone(), row.clone()))
                .unwrap();
            assert_eq!(path, &format!("{kind}/{}/", mangled.0));
            assert_eq!(mangled.1["deleted"], true);
        }
    }
}

#[tokio::test]
async fn unidentified_update_keeps_the_existing_object() {
    let mock = Mock::start().await;
    let mut declarations = example::declarations().unwrap();
    mock.sync("demo", &declarations).await.unwrap();
    let old = mock.row("cja:demo:dashboard:activity");
    let id = old["id"].as_i64().unwrap().to_string();
    mock.log();
    declarations.dashboards[0].title.push_str(" updated");
    mock.state.lock().unwrap().drop_tag = true;
    let error = mock.sync("demo", &declarations).await.unwrap_err();
    let SyncError::TagNotPersisted { cleanup, .. } = &error else {
        panic!("{error}")
    };
    assert_eq!(cleanup, "the existing object was left in place");
    let writes = mock.writes();
    assert_eq!(writes.len(), 1, "unexpected cleanup: {writes:?}");
    assert_eq!(writes[0].0, Method::PATCH);
    assert_eq!(writes[0].1, format!("dashboards/{id}/"));
    let state = mock.state.lock().unwrap();
    let row = &state.rows[&("dashboards".into(), id)];
    assert_ne!(row["deleted"], true);
    assert_eq!(row["name"], "Product activity updated");
}

#[tokio::test]
async fn failed_cleanup_reports_manual_removal_and_keeps_mangled_row() {
    let mock = Mock::start().await;
    {
        let mut state = mock.state.lock().unwrap();
        state.mangle = Some("dashboards");
        state.fail_cleanup = true;
    }
    let error = mock
        .sync("demo", &example::declarations().unwrap())
        .await
        .unwrap_err();
    let SyncError::TagNotPersisted { cleanup, .. } = &error else {
        panic!("{error}")
    };
    assert!(
        cleanup.contains("remove it by hand") && cleanup.contains("cleanup denied"),
        "{cleanup}"
    );
    let writes = mock.writes();
    assert_eq!(
        writes.len(),
        2,
        "create and cleanup attempt required: {writes:?}"
    );
    assert_eq!(writes[0].0, Method::POST);
    assert_eq!(writes[1].0, Method::PATCH);
    assert_eq!(writes[1].2, json!({"deleted":true}));
    let state = mock.state.lock().unwrap();
    assert!(
        state
            .rows
            .values()
            .any(|row| row["tags"] == json!(["cja:mangled"]) && row["deleted"] != true)
    );
}

#[tokio::test]
async fn alert_names_over_the_posthog_limit_fail_before_any_request() {
    let mock = Mock::start().await;
    let mut declarations = example::declarations().unwrap();
    // "<description> [cja:demo:alert:quiet]": the suffix is 23 characters.
    declarations.alerts[0].description = Some("d".repeat(232));
    mock.sync("demo", &declarations).await.unwrap();
    assert_eq!(
        mock.state
            .lock()
            .unwrap()
            .rows
            .iter()
            .find(|((kind, _), _)| kind == "alerts")
            .unwrap()
            .1["name"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        255
    );
    mock.log();
    declarations.alerts[0].description = Some("d".repeat(233));
    let error = mock.sync("demo", &declarations).await.unwrap_err();
    assert!(
        matches!(&error, SyncError::AlertNameTooLong { alert, length: 256, .. } if alert == "cja:demo:alert:quiet"),
        "{error}"
    );
    assert!(error.to_string().contains("at most 255"), "{error}");
    assert!(
        error.to_string().contains("shorten the alert description"),
        "{error}"
    );
    assert!(
        !error.to_string().contains("shorten the alert ID"),
        "{error}"
    );
    // The same description fits a shorter app slug: the bound is on the name.
    assert_eq!(mock.log(), Vec::new());
    mock.sync("dem", &declarations).await.unwrap();

    mock.log();
    declarations.alerts[0].id = "a".repeat(128);
    declarations.alerts[0].description = None;
    let error = mock.sync("demo", &declarations).await.unwrap_err();
    assert!(matches!(error, SyncError::AlertNameTooLong { .. }));
    assert!(
        error.to_string().contains("shorten the alert ID"),
        "{error}"
    );
    assert!(
        !error.to_string().contains("shorten the alert description"),
        "{error}"
    );
    assert_eq!(mock.log(), Vec::new());
}

#[tokio::test]
async fn unicode_alert_length_counts_characters_and_eyes_only_alerts_skip() {
    let mock = Mock::start().await;
    let mut declarations = example::declarations().unwrap();
    declarations.alerts[0].description = Some("é".repeat(232));
    mock.sync("demo", &declarations).await.unwrap();
    let row = mock
        .state
        .lock()
        .unwrap()
        .rows
        .iter()
        .find(|((kind, _), _)| kind == "alerts")
        .unwrap()
        .1
        .clone();
    assert_eq!(row["name"].as_str().unwrap().chars().count(), 255);
    assert!(row["name"].as_str().unwrap().len() > 255);
    mock.log();
    declarations.alerts[0].description = Some("é".repeat(233));
    let error = mock.sync("demo", &declarations).await.unwrap_err();
    assert!(matches!(
        error,
        SyncError::AlertNameTooLong { length: 256, .. }
    ));
    assert_eq!(mock.log(), Vec::new());

    let ops = TelemetryMetricBuilder::new("ops", AggregateFunction::Count, None)
        .unwrap()
        .build()
        .unwrap();
    let alert = AlertBuilder::new("ops_quiet", "ops")
        .description("é".repeat(300))
        .window_seconds(300)
        .evaluation_interval_seconds(60)
        .warning(Comparison::Below, 1.)
        .build()
        .unwrap();
    let report = mock
        .sync("demo", &Declarations::default().metric(ops).alert(alert))
        .await
        .unwrap();
    assert!(
        report
            .skipped
            .iter()
            .any(|entry| entry.contains("ops_quiet")),
        "{:?}",
        report.skipped
    );
    assert!(!mock.writes().iter().any(|(_, path, _)| path == "alerts/"));
}

/// Insert a copy of the live row matching `pick` under a fresh id, the way
/// duplicating an object in the `PostHog` UI does (tags and name come along).
fn duplicate(m: &Mock, kind: &str, pick: impl Fn(&Value) -> bool) -> (String, String) {
    let mut s = m.state.lock().unwrap();
    let mut copy = s
        .rows
        .iter()
        .find(|((k, _), row)| k == kind && pick(row))
        .map(|(_, row)| row.clone())
        .unwrap();
    s.next += 1;
    let key = if kind == "alerts" {
        uuid::Uuid::from_u128(s.next.into()).to_string()
    } else {
        s.next.to_string()
    };
    copy["id"] = if kind == "alerts" {
        json!(key)
    } else {
        json!(s.next)
    };
    s.rows.insert((kind.into(), key.clone()), copy);
    (kind.into(), key)
}

/// The error text and recovery steps quoted in `docs/posthog-sync.md`
/// ("Duplicating a managed object in `PostHog`").
#[tokio::test]
async fn a_duplicated_managed_object_fails_until_the_copy_loses_its_identity() {
    let m = Mock::start().await;
    let d = example::declarations().unwrap();
    m.sync("demo", &d).await.unwrap();
    m.log();

    let copy = duplicate(&m, "insights", |r| {
        r["tags"] == json!(["cja:demo:insight:pages"])
    });
    let error = m.sync("demo", &d).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "duplicate identity cja:demo:insight:pages"
    );
    assert!(m.writes().is_empty(), "fails before any write");
    m.state.lock().unwrap().rows.get_mut(&copy).unwrap()["tags"] = json!([]);
    let report = m.sync("demo", &d).await.unwrap();
    assert_eq!(m.writes(), vec![]);
    assert!(report.created.is_empty() && report.updated.is_empty() && report.deleted.is_empty());

    let copy = duplicate(&m, "alerts", |r| {
        r["name"]
            .as_str()
            .is_some_and(|n| n.ends_with("[cja:demo:alert:quiet]"))
    });
    let error = m.sync("demo", &d).await.unwrap_err();
    assert_eq!(error.to_string(), "duplicate alert quiet");
    m.state.lock().unwrap().rows.get_mut(&copy).unwrap()["name"] = json!("Quiet (Copy)");
    m.log();
    let report = m.sync("demo", &d).await.unwrap();
    assert_eq!(m.writes(), vec![]);
    assert!(report.created.is_empty() && report.updated.is_empty() && report.deleted.is_empty());
}
