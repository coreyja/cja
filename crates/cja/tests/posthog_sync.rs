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
    AggregateFunction, AlertBuilder, Comparison, Declarations, Item, TelemetryMetricBuilder,
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
            posthog::sync(&config, d, &posthog::http_client()),
        )
        .await
        .unwrap()
    }
    fn log(&self) -> Vec<(Method, String, Value)> {
        std::mem::take(&mut self.state.lock().unwrap().log)
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
    let mut s = shared.lock().unwrap();
    s.log.push((method.clone(), path.into(), body.clone()));
    if method != Method::GET && std::mem::take(&mut s.fail) {
        return (StatusCode::BAD_REQUEST, Json(json!({"detail":"bad"}))).into_response();
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
        (Method::POST, [kind]) => {
            if *kind == "alerts" && !body["subscribed_users"].is_array() {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"detail": "subscribed_users is required"})),
                )
                    .into_response();
            }
            s.next += 1;
            let n = s.next;
            let key = if *kind == "alerts" {
                uuid::Uuid::from_u128(n.into()).to_string()
            } else {
                n.to_string()
            };
            body["id"] = if *kind == "alerts" {
                json!(key)
            } else {
                json!(n)
            };
            if *kind == "dashboards" {
                body["tiles"] = json!([]);
            }
            if *kind == "insights" {
                body["query"]["source"]["version"] = json!(2);
                body["query"]["source"]["trendsFilter"] = json!({});
            }
            s.rows.insert(((*kind).into(), key), body.clone());
            body
        }
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
    let mut d = example::declarations().unwrap();
    m.sync("demo", &d).await.unwrap();
    let writes = m
        .log()
        .into_iter()
        .filter(|(v, _, _)| v != Method::GET)
        .collect::<Vec<_>>();
    assert_eq!(writes.len(), 8);
    assert_eq!(writes[0].1, "dashboards/");
    assert_eq!(writes[1].1, "insights/");
    assert_eq!(writes[7].1, "alerts/");
    assert_eq!(
        writes[7].2["threshold"]["configuration"],
        json!({"type":"absolute","bounds":{"lower":1.}})
    );
    assert_eq!(writes[7].2["subscribed_users"], json!([]));
    assert_eq!(
        writes[1].2["query"]["source"]["breakdownFilter"]["breakdowns"][0]["property"],
        "path"
    );
    let report = m.sync("demo", &d).await.unwrap();
    assert!(m.log().iter().all(|(v, _, _)| v == Method::GET));
    assert!(report.created.is_empty() && report.updated.is_empty() && report.deleted.is_empty());
    d.metrics[0].display_name = Some("Views".into());
    d.dashboards[0].title = "Usage".into();
    let report = m.sync("demo", &d).await.unwrap();
    assert_eq!(report.updated.len(), 2);
    assert_eq!(
        m.log()
            .iter()
            .filter(|(v, _, _)| v != Method::GET)
            .map(|(v, _, _)| v)
            .collect::<Vec<_>>(),
        [Method::PATCH, Method::PATCH]
    );
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
async fn telemetry_and_health_are_skipped() {
    let m = Mock::start().await;
    let mut d = example::declarations().unwrap();
    d.dashboards[0].sections[0]
        .items
        .push(Item::health_summary());
    let d = d
        .metric(
            TelemetryMetricBuilder::new("jobs", AggregateFunction::Count, None)
                .unwrap()
                .build()
                .unwrap(),
        )
        .alert(
            AlertBuilder::new("failed", "jobs")
                .window_seconds(60)
                .evaluation_interval_seconds(60)
                .critical(Comparison::Above, 1.)
                .build()
                .unwrap(),
        );
    let r = m.sync("demo", &d).await.unwrap();
    assert_eq!(r.skipped.len(), 3);
    assert_eq!(r.created.len(), 5);
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
        [(
            Method::POST,
            format!("dashboards/{}/delete_tile/", dashboard["id"]),
            json!({"tile_id":tile_id})
        )]
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
        [(
            Method::POST,
            format!("dashboards/{}/add_insight/", dashboard["id"]),
            json!({"insight_id":page_id})
        )]
    );
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
