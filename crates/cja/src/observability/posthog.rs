//! Reconcile one application's declarations against the `PostHog` project API.
use super::{
    Alert, Bucket, Comparison, Dashboard, Declarations, FilterOperator, FilterValue, Item,
    MetricQuery, ProductAggregate, ProductEventMetric, RatioMetric, TelemetryMetric,
};
use eyes_subscriber::eyes_query::{
    AggregateFunction as EyesAggregate, BinaryOp, Expr, FieldPath, Function, Literal,
    MONDAY_ORIGIN_US, PathSegment, Stage,
};
use http::Method;
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

#[cfg(test)]
mod sql_tests {
    use super::*;
    use crate::observability::{ProductMetricBuilder, RatioMetricBuilder, TelemetryMetricBuilder};

    fn telemetry(metric: &super::super::Metric) -> &TelemetryMetric {
        let MetricQuery::Telemetry(metric) = &metric.query else {
            panic!("expected telemetry")
        };
        metric
    }

    #[test]
    fn log_count_has_exact_predicate_coverage_and_outer_limit() {
        let metric = TelemetryMetricBuilder::new("games", EyesAggregate::Count, None)
            .unwrap()
            .filter_eq("fields.event_type", "game_completed")
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(
            telemetry_sql(telemetry(&metric), "arena", SqlWindow::trailing(3600)).unwrap(),
            "SELECT if((SELECT count() FROM (SELECT 1 FROM logs WHERE service_name = 'arena' AND timestamp <= now() - INTERVAL 3600 SECOND LIMIT 1)) > 0, count(), NULL) AS value FROM (SELECT NULL AS _metric_value FROM logs WHERE service_name = 'arena' AND timestamp >= now() - INTERVAL 3600 SECOND AND attributes['event_type'] = 'game_completed') AS _source LIMIT 1"
        );
    }

    #[test]
    fn span_http_percentile_maps_semantics_and_duration_units() {
        let metric = TelemetryMetricBuilder::new("http.p95", EyesAggregate::P95, Some("duration"))
            .unwrap()
            .filter_eq("semantic_kind", "http.request")
            .unwrap()
            .build()
            .unwrap();
        let sql = telemetry_sql(telemetry(&metric), "arena", SqlWindow::trailing(300)).unwrap();
        assert_eq!(
            sql,
            concat!(
                "SELECT if((SELECT count() FROM (SELECT 1 FROM posthog.trace_spans WHERE service_name = 'arena' AND timestamp <= now() - INTERVAL 300 SECOND LIMIT 1)) > 0, ",
                "if(count(_metric_value) = 0, NULL, quantile(0.95)(_metric_value)), NULL) AS value FROM ",
                "(SELECT duration_nano / 1000 AS _metric_value FROM posthog.trace_spans WHERE service_name = 'arena' AND timestamp >= now() - INTERVAL 300 SECOND AND ",
                "(attributes['target'] = 'cja::server::trace' AND attributes['kind'] = 'server')) AS _source LIMIT 1"
            )
        );
        assert!(sql.contains("FROM posthog.trace_spans"));
        assert!(!sql.contains("FROM logs"));
        assert!(sql.contains("duration_nano / 1000 AS _metric_value"));
        assert!(sql.contains(
            "attributes['target'] = 'cja::server::trace' AND attributes['kind'] = 'server'"
        ));
        assert!(sql.contains("quantile(0.95)(_metric_value)"));
        assert!(sql.contains("timestamp <= now() - INTERVAL 300 SECOND"));
    }

    #[test]
    fn every_aggregate_and_typed_where_operator_compiles() {
        let functions = [
            EyesAggregate::Count,
            EyesAggregate::CountDistinct,
            EyesAggregate::Sum,
            EyesAggregate::Avg,
            EyesAggregate::Min,
            EyesAggregate::Max,
            EyesAggregate::P50,
            EyesAggregate::P90,
            EyesAggregate::P95,
            EyesAggregate::P99,
        ];
        let expected = [
            "count()",
            "count(DISTINCT _metric_value)",
            "sum(_metric_value)",
            "avg(_metric_value)",
            "min(_metric_value)",
            "max(_metric_value)",
            "quantile(0.50)(_metric_value)",
            "quantile(0.90)(_metric_value)",
            "quantile(0.95)(_metric_value)",
            "quantile(0.99)(_metric_value)",
        ];
        for (function, aggregate) in functions.into_iter().zip(expected) {
            let argument = if function == EyesAggregate::Count {
                None
            } else if function == EyesAggregate::CountDistinct {
                Some("fields.user_id")
            } else {
                Some("fields.duration_ms")
            };
            let metric = TelemetryMetricBuilder::new("sample", function, argument)
                .unwrap()
                .filter_eq("fields.event_type", "game_completed")
                .unwrap()
                .filter_eq("fields.enabled", true)
                .unwrap()
                .filter_eq("fields.attempt", 2_i64)
                .unwrap()
                .filter_gte("fields.duration_ms", 1.5_f64)
                .unwrap()
                .build()
                .unwrap();
            let sql = telemetry_sql(telemetry(&metric), "arena", SqlWindow::trailing(60)).unwrap();
            assert!(sql.contains(aggregate), "{sql}");
            assert!(sql.contains("attributes['enabled'] = 'true'"), "{sql}");
            assert!(
                sql.contains("toFloatOrNull(attributes['attempt']) = 2"),
                "{sql}"
            );
            assert!(
                sql.contains("toFloatOrNull(attributes['duration_ms']) >= 1.5e0"),
                "{sql}"
            );
            assert!(!sql.contains("posthog.trace_spans"), "{sql}");
        }
    }

    #[test]
    fn numeric_guard_preserves_f64_min_without_a_huge_integer_literal() {
        let metric =
            TelemetryMetricBuilder::new("latency", EyesAggregate::P95, Some("fields.duration_ms"))
                .unwrap()
                .filter_eq("fields.metric_type", "queue_wait")
                .unwrap()
                .filter_numeric("fields.duration_ms")
                .unwrap()
                .build()
                .unwrap();
        let sql = telemetry_sql(telemetry(&metric), "arena", SqlWindow::trailing(300)).unwrap();
        assert!(
            sql.contains("toFloatOrNull(attributes['duration_ms']) >= -1.7976931348623157e308"),
            "{sql}"
        );
        assert!(!sql.contains("posthog.trace_spans"));
        assert!(sql.contains("if(count(_metric_value) = 0, NULL, quantile(0.95)(_metric_value))"));
    }

    #[test]
    fn covered_empty_sum_is_null() {
        let metric = TelemetryMetricBuilder::new(
            "suppressed",
            EyesAggregate::Sum,
            Some("fields.suppressed"),
        )
        .unwrap()
        .filter_eq("fields.event_type", "shout_screening")
        .unwrap()
        .build()
        .unwrap();
        let sql = telemetry_sql(telemetry(&metric), "arena", SqlWindow::trailing(3600)).unwrap();
        assert!(
            sql.contains("if((SELECT count() FROM (SELECT 1 FROM logs"),
            "{sql}"
        );
        assert!(
            sql.contains("if(count(_metric_value) = 0, NULL, sum(_metric_value))"),
            "{sql}"
        );
    }

    #[test]
    fn covered_empty_product_sum_is_null() {
        let metric = ProductMetricBuilder::sum("sales", "purchased", "amount")
            .build()
            .unwrap();
        let MetricQuery::ProductEvent(product) = &metric.query else {
            panic!("expected product metric")
        };
        let sql = product_plan(product, "arena", SqlWindow::trailing(3600))
            .unwrap()
            .sql;
        assert!(
            sql.contains("if(count(_metric_value) = 0, NULL, sum(_metric_value))"),
            "{sql}"
        );
    }

    #[test]
    fn buckets_groups_escaping_and_dual_source_coverage() {
        let metric = TelemetryMetricBuilder::new("route", EyesAggregate::Count, None)
            .unwrap()
            .filter_eq("fields[\"o'key\"]", "it's ok")
            .unwrap()
            .group_by("fields[\"http.route\"]")
            .unwrap()
            .time_bucket_weeks(1)
            .build()
            .unwrap();
        let sql = telemetry_sql(telemetry(&metric), "arena", SqlWindow::trailing(604_800)).unwrap();
        assert!(sql.contains("attributes['o''key'] = 'it''s ok'"), "{sql}");
        assert!(
            sql.contains("toDateTime64('1970-01-05 00:00:00', 6, 'UTC')"),
            "{sql}"
        );
        assert!(sql.contains("AS bucket"));
        assert!(sql.contains("AS _dim_0"));
        assert!(sql.contains("ORDER BY bucket DESC, _dim_0 LIMIT 50000"));
        assert!(sql.ends_with("ORDER BY bucket, _dim_0 LIMIT 50000"));
        assert_eq!(
            sql.matches("timestamp <= now() - INTERVAL 604800 SECOND")
                .count(),
            2
        );
        let node = sql_query(&sql, SqlDisplay::Line, 1);
        assert_eq!(node["kind"], "DataVisualizationNode");
        assert_eq!(node["source"]["kind"], "HogQLQuery");
        assert_eq!(node["display"], "ActionsLineGraph");
    }

    #[test]
    fn ratio_uses_each_operand_window_and_null_safe_division() {
        let numerator = TelemetryMetricBuilder::new("daily", EyesAggregate::Count, None)
            .unwrap()
            .filter_eq("fields.event_type", "game_completed")
            .unwrap()
            .build()
            .unwrap();
        let denominator = TelemetryMetricBuilder::new("monthly", EyesAggregate::Count, None)
            .unwrap()
            .filter_eq("fields.event_type", "game_started")
            .unwrap()
            .build()
            .unwrap();
        let ratio = RatioMetricBuilder::new("share", "daily", "monthly")
            .numerator_window_seconds(86_400)
            .denominator_window_seconds(2_419_200)
            .as_percentage()
            .build()
            .unwrap();
        let MetricQuery::Ratio(spec) = &ratio.query else {
            panic!("expected ratio")
        };
        let declarations = Declarations::new()
            .metric(numerator)
            .metric(denominator)
            .metric(ratio.clone());
        let sql = ratio_sql(spec, &declarations, "arena", SqlWindow::trailing(3600)).unwrap();
        assert_eq!(
            sql,
            concat!(
                "SELECT if(isNull(_num_value) OR isNull(_den_value) OR _den_value = 0, NULL, _num_value * 100 / _den_value) AS value ",
                "FROM (SELECT value AS _num_value FROM (SELECT if((SELECT count() FROM (SELECT 1 FROM logs WHERE service_name = 'arena' AND timestamp <= now() - INTERVAL 86400 SECOND LIMIT 1)) > 0, ",
                "count(), NULL) AS value FROM (SELECT NULL AS _metric_value FROM logs WHERE service_name = 'arena' AND timestamp >= now() - INTERVAL 86400 SECOND AND attributes['event_type'] = 'game_completed') AS _source LIMIT 1) LIMIT 1) AS _num ",
                "CROSS JOIN (SELECT value AS _den_value FROM (SELECT if((SELECT count() FROM (SELECT 1 FROM logs WHERE service_name = 'arena' AND timestamp <= now() - INTERVAL 2419200 SECOND LIMIT 1)) > 0, ",
                "count(), NULL) AS value FROM (SELECT NULL AS _metric_value FROM logs WHERE service_name = 'arena' AND timestamp >= now() - INTERVAL 2419200 SECOND AND attributes['event_type'] = 'game_started') AS _source LIMIT 1) LIMIT 1) AS _den LIMIT 1"
            )
        );
        assert!(sql.contains("FROM logs"));
        assert!(!sql.contains("FROM events"));
        assert!(sql.contains("timestamp <= now() - INTERVAL 86400 SECOND"));
        assert!(sql.contains("timestamp <= now() - INTERVAL 2419200 SECOND"));
        assert!(sql.contains("isNull(_num_value) OR isNull(_den_value) OR _den_value = 0"));
        assert!(sql.contains("_num_value * 100 / _den_value"));
    }

    #[test]
    fn mixed_cluster_ratio_is_rejected_before_sql_generation() {
        let numerator = TelemetryMetricBuilder::new("completed", EyesAggregate::Count, None)
            .unwrap()
            .filter_eq("fields.event_type", "game_completed")
            .unwrap()
            .build()
            .unwrap();
        let denominator = ProductMetricBuilder::count("played", "game_played")
            .build()
            .unwrap();
        let ratio = RatioMetricBuilder::new("share", "completed", "played")
            .build()
            .unwrap();
        let MetricQuery::Ratio(spec) = &ratio.query else {
            panic!("expected ratio")
        };
        let declarations = Declarations::new()
            .metric(numerator)
            .metric(denominator)
            .metric(ratio.clone());
        assert!(
            matches!(ratio_sql(spec, &declarations, "arena", SqlWindow::trailing(3600)), Err(SyncError::Invalid(message)) if message.contains("PostHog cannot query both clusters"))
        );
    }

    #[test]
    fn absent_source_field_cannot_be_a_group_dimension() {
        let metric = TelemetryMetricBuilder::new("kinds", EyesAggregate::Count, None)
            .unwrap()
            .group_by("semantic_kind")
            .unwrap()
            .build()
            .unwrap();
        assert!(matches!(
            telemetry_sql(telemetry(&metric), "arena", SqlWindow::trailing(3600)),
            Err(SyncError::Invalid(message)) if message.contains("cannot group by") && message.contains("semantic_kind")
        ));
    }

    #[test]
    fn product_product_ratio_uses_each_operand_window() {
        let numerator = ProductMetricBuilder::count("played", "game_played")
            .build()
            .unwrap();
        let denominator = ProductMetricBuilder::count("started", "game_started")
            .build()
            .unwrap();
        let ratio = RatioMetricBuilder::new("share", "played", "started")
            .numerator_window_seconds(86_400)
            .denominator_window_seconds(2_419_200)
            .build()
            .unwrap();
        let MetricQuery::Ratio(spec) = &ratio.query else {
            panic!("expected ratio")
        };
        let declarations = Declarations::new()
            .metric(numerator)
            .metric(denominator)
            .metric(ratio.clone());
        let sql = ratio_sql(spec, &declarations, "arena", SqlWindow::trailing(3600)).unwrap();
        assert_eq!(
            sql,
            include_str!("../../tests/fixtures/posthog_product_ratio.sql").trim_end()
        );
        assert!(sql.contains("event = 'game_played'"));
        assert!(sql.contains("event = 'game_started'"));
    }

    #[test]
    fn table_numerator_scalar_denominator_uses_cross_join() {
        let numerator = TelemetryMetricBuilder::new("routes", EyesAggregate::Count, None)
            .unwrap()
            .filter_eq("semantic_kind", "http.request")
            .unwrap()
            .group_by("fields.route")
            .unwrap()
            .build()
            .unwrap();
        let denominator = TelemetryMetricBuilder::new("requests", EyesAggregate::Count, None)
            .unwrap()
            .filter_eq("semantic_kind", "http.request")
            .unwrap()
            .build()
            .unwrap();
        let ratio = RatioMetricBuilder::new("route-share", "routes", "requests")
            .build()
            .unwrap();
        let MetricQuery::Ratio(spec) = &ratio.query else {
            panic!("expected ratio")
        };
        let declarations = Declarations::new()
            .metric(numerator)
            .metric(denominator)
            .metric(ratio.clone());
        let sql = ratio_sql(spec, &declarations, "arena", SqlWindow::trailing(3600)).unwrap();
        assert_eq!(
            sql,
            include_str!("../../tests/fixtures/posthog_table_scalar_ratio.sql").trim_end()
        );
        assert!(sql.contains("SELECT value AS _num_value, _dim_0"), "{sql}");
        assert!(
            sql.contains("CROSS JOIN (SELECT value AS _den_value"),
            "{sql}"
        );
        assert!(sql.contains("GROUP BY _dim_0"), "{sql}");
        assert!(sql.ends_with("ORDER BY _dim_0 LIMIT 50000"), "{sql}");
        assert_eq!(
            sql.matches("timestamp <= now() - INTERVAL 3600 SECOND")
                .count(),
            2
        );
    }

    #[test]
    fn grouped_ratio_regroups_every_key_after_union() {
        let numerator = TelemetryMetricBuilder::new("num", EyesAggregate::Count, None)
            .unwrap()
            .filter_eq("fields.event_type", "game_completed")
            .unwrap()
            .group_by("fields.source")
            .unwrap()
            .time_bucket(300)
            .build()
            .unwrap();
        let denominator = TelemetryMetricBuilder::new("den", EyesAggregate::Count, None)
            .unwrap()
            .filter_eq("fields.event_type", "game_started")
            .unwrap()
            .group_by("fields.source")
            .unwrap()
            .time_bucket(300)
            .build()
            .unwrap();
        let ratio = RatioMetricBuilder::new("ratio", "num", "den")
            .build()
            .unwrap();
        let MetricQuery::Ratio(spec) = &ratio.query else {
            panic!("expected ratio")
        };
        let declarations = Declarations::new()
            .metric(numerator)
            .metric(denominator)
            .metric(ratio.clone());
        let sql = ratio_sql(spec, &declarations, "arena", SqlWindow::trailing(3600)).unwrap();
        assert_eq!(
            sql,
            include_str!("../../tests/fixtures/posthog_grouped_ratio.sql").trim_end()
        );
        assert!(sql.contains(
            "toNullable(value) AS numerator, NULL AS denominator, 1 AS num_present, bucket, _dim_0"
        ));
        assert!(
            sql.contains("NULL AS numerator, toNullable(value) AS denominator, NULL AS num_present, bucket, _dim_0")
        );
        assert!(sql.contains("if(isNull(max(num_present)), 0, max(numerator))"));
        assert!(sql.contains("isNull(_den_value) OR _den_value = 0"));
        assert!(sql.contains("GROUP BY bucket, _dim_0 ORDER BY bucket DESC, _dim_0 LIMIT 50000"));
        assert!(sql.ends_with("ORDER BY bucket, _dim_0 LIMIT 50000"));
    }

    #[test]
    fn unbucketed_grouped_ratio_orders_before_truncation() {
        let numerator = TelemetryMetricBuilder::new("num", EyesAggregate::Count, None)
            .unwrap()
            .group_by("fields.source")
            .unwrap()
            .build()
            .unwrap();
        let denominator = TelemetryMetricBuilder::new("den", EyesAggregate::Count, None)
            .unwrap()
            .group_by("fields.source")
            .unwrap()
            .build()
            .unwrap();
        let ratio = RatioMetricBuilder::new("ratio", "num", "den")
            .build()
            .unwrap();
        let MetricQuery::Ratio(spec) = &ratio.query else {
            panic!("expected ratio")
        };
        let declarations = Declarations::new()
            .metric(numerator)
            .metric(denominator)
            .metric(ratio.clone());
        let sql = ratio_sql(spec, &declarations, "arena", SqlWindow::trailing(3600)).unwrap();
        assert!(
            sql.contains("GROUP BY _dim_0 ORDER BY _dim_0 LIMIT 50000"),
            "{sql}"
        );
    }

    #[test]
    fn unsupported_ir_stages_and_missing_query_name_the_metric() {
        let base = TelemetryMetricBuilder::new("sample", EyesAggregate::Count, None)
            .unwrap()
            .build()
            .unwrap();
        let path = FieldPath::bare("message");
        let stages = [
            Stage::Regex {
                pattern: "x".into(),
                source: path.clone(),
                alias: "match".into(),
            },
            Stage::ParseJson {
                source: path,
                alias: "json".into(),
            },
            Stage::Let {
                alias: "x".into(),
                expr: Expr::Literal {
                    value: Literal::Int(1),
                },
            },
            Stage::Project { outputs: vec![] },
            Stage::Sort { keys: vec![] },
            Stage::Limit { count: 1 },
        ];
        for stage in stages {
            let mut metric = base.clone();
            let MetricQuery::Telemetry(ref mut telemetry) = metric.query else {
                panic!("expected telemetry")
            };
            telemetry
                .named_metric_mut()
                .query
                .as_mut()
                .unwrap()
                .query
                .stages
                .insert(0, stage.clone());
            let error = telemetry_sql(telemetry, "arena", SqlWindow::trailing(60)).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("metric sample: unsupported stage"),
                "{error}"
            );
            assert!(error.to_string().contains(&format!("{stage:?}")), "{error}");
        }
        let mut metric = base;
        let MetricQuery::Telemetry(ref mut telemetry) = metric.query else {
            panic!("expected telemetry")
        };
        telemetry.named_metric_mut().query = None;
        assert!(
            telemetry_sql(telemetry, "arena", SqlWindow::trailing(60))
                .unwrap_err()
                .to_string()
                .contains("metric sample: missing query")
        );
    }

    #[test]
    fn delayed_window_bounds_and_overflow() {
        let window = SqlWindow {
            seconds: 300,
            delay: 30,
        };
        assert_eq!(
            window.time_predicate().unwrap(),
            "timestamp >= now() - INTERVAL 330 SECOND AND timestamp < now() - INTERVAL 30 SECOND"
        );
        assert!(
            SqlWindow {
                seconds: u64::MAX,
                delay: 1
            }
            .lower()
            .is_err()
        );
    }

    #[test]
    fn bucket_row_cap_accepts_maximum_and_rejects_one_more() {
        assert!(
            ensure_bucket_rows(600, SqlWindow::trailing((MAX_SQL_ROWS - 1) * 600), "series")
                .is_ok()
        );
        assert!(
            ensure_bucket_rows(600, SqlWindow::trailing(MAX_SQL_ROWS * 600), "series").is_err()
        );
    }

    /// cja's `Tracer::make_span` sets `otel.name = "<METHOD> <route>"`, so the
    /// exported `PostHog` span is never named `server.request`. The plan maps
    /// `semantic_kind = http.request` by target and kind only, not by name.
    #[test]
    fn http_semantic_kind_matches_renamed_server_spans_by_target_and_kind_only() {
        let metric = TelemetryMetricBuilder::new("http.requests", EyesAggregate::Count, None)
            .unwrap()
            .filter_eq("semantic_kind", "http.request")
            .unwrap()
            .build()
            .unwrap();
        let sql = telemetry_sql(telemetry(&metric), "arena", SqlWindow::trailing(3600)).unwrap();
        assert!(sql.contains("FROM posthog.trace_spans"), "{sql}");
        assert!(
            sql.contains(
                "attributes['target'] = 'cja::server::trace' AND attributes['kind'] = 'server'"
            ),
            "{sql}"
        );
        assert!(
            !sql.contains("name = 'server.request'"),
            "renamed HTTP spans never match a server.request name filter: {sql}"
        );
    }
}
pub(crate) fn slug(app: &str) -> Result<(), String> {
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
/// The sync engine's HTTPS client: rustls on ring with the platform verifier
/// (system roots) and a 30 second request timeout.
///
/// It carries its own TLS configuration, so building it installs no
/// process-wide rustls `CryptoProvider`.
#[derive(Debug, Clone)]
pub struct SyncClient(reqwest_012::Client);
impl SyncClient {
    /// Build the client. Fails when the platform certificate verifier cannot
    /// load (for example an empty system root store).
    pub fn new() -> Result<Self, SyncClientError> {
        let tls = crate::tls::ring_client_config().map_err(|e| SyncClientError(Box::new(e)))?;
        reqwest_012::Client::builder()
            .use_preconfigured_tls(tls)
            .timeout(Duration::from_secs(30))
            .build()
            .map(Self)
            .map_err(|e| SyncClientError(Box::new(e)))
    }
}
/// [`SyncClient::new`] could not build the client; the cause is its `source`.
#[derive(Debug, thiserror::Error)]
#[error("building the PostHog sync HTTP client")]
pub struct SyncClientError(#[source] Box<dyn Error + Send + Sync>);
/// A request that never produced an HTTP response (connect, TLS, timeout or
/// body read). Displays the transport's message; the OS cause is its `source`.
#[derive(Debug)]
pub struct TransportError(reqwest_012::Error);
impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl Error for TransportError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.0.source()
    }
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
        for (action, keys) in [
            ("created", &self.created),
            ("updated", &self.updated),
            ("deleted", &self.deleted),
        ] {
            for key in keys {
                writeln!(f, "{action}: {key}")?;
            }
        }
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
        source: TransportError,
    },
    #[error("{object}: decoding response")]
    Decode {
        object: String,
        source: serde_json::Error,
    },
    #[error(
        "{object}: PostHog did not persist our identity {tag} (a tag for dashboards/insights, a name suffix for alerts); {cleanup}"
    )]
    TagNotPersisted {
        object: String,
        tag: String,
        /// What happened to the object the failed write touched.
        cleanup: String,
    },
    #[error(
        "{alert}: alert name is {length} characters and PostHog allows at most {ALERT_NAME_LIMIT}; shorten the {hint}"
    )]
    AlertNameTooLong {
        alert: String,
        length: usize,
        hint: &'static str,
    },
    #[error("{0}")]
    Invalid(String),
}
struct Api<'a> {
    config: &'a SyncConfig,
    http: &'a SyncClient,
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
            source: TransportError(source),
        };
        let mut req = self
            .http
            .0
            .request(method.clone(), url)
            .bearer_auth(&self.config.api_key)
            .header("Content-Type", "application/json");
        if let Some(v) = body {
            req = req.body(v.to_string());
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
            let origin = url::Url::parse(&self.config.host)
                .map_err(|e| SyncError::Invalid(format!("{kind}: invalid API host: {e}")))?;
            let next_url = url::Url::parse(next)
                .map_err(|e| SyncError::Invalid(format!("{kind}: invalid pagination URL: {e}")))?;
            if origin.origin() != next_url.origin() {
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
fn insight_absences_match(wanted: &Value, actual: &Value) -> bool {
    if wanted.pointer("/query/source/kind") != Some(&json!("TrendsQuery")) {
        return true;
    }
    (wanted.pointer("/query/source/breakdownFilter").is_some()
        || actual
            .pointer("/query/source/breakdownFilter/breakdowns")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty))
        && (wanted
            .pointer("/query/source/trendsFilter/display")
            .is_some()
            || actual
                .pointer("/query/source/trendsFilter/display")
                .is_none_or(|display| display == "ActionsLineGraph"))
        && (wanted
            .pointer("/query/source/dateRange/explicitDate")
            .is_some()
            || actual.pointer("/query/source/dateRange/explicitDate") != Some(&json!(true)))
}
const DEFAULT_RANGE_SECONDS: u64 = 30 * 86_400;
const MAX_RANGE_SECONDS: u64 = 31_622_400;
const TAG_LIMIT: usize = 255;
// PostHog's HogQL QUERY/SQL_ALERT contexts cap SELECT results at 50,000 rows.
const MAX_SQL_ROWS: u64 = 50_000;
const SQL_COVERAGE_DESCRIPTION: &str =
    "Shows no value until PostHog holds data back to the start of this window.";
fn date_from(seconds: u64) -> String {
    for (unit, suffix) in [(86_400, "d"), (3_600, "h"), (60, "M")] {
        if seconds.is_multiple_of(unit) {
            return format!("-{}{suffix}", seconds / unit);
        }
    }
    format!("-{seconds}s")
}
fn resolve_presentation<'a>(
    item: &'a Item,
    dash: &Dashboard,
) -> Option<(&'a str, &'static str, u64)> {
    let (metric, display) = match item {
        Item::Stat { metric_id, .. } => (metric_id, "BoldNumber"),
        Item::Table { metric_id, .. } => (metric_id, "ActionsTable"),
        Item::TimeSeries { metric_id, .. } => (metric_id, "ActionsLineGraph"),
        Item::HealthSummary { .. } | Item::Links { .. } => return None,
    };
    Some((
        metric,
        display,
        item.declared_window_seconds()
            .or(dash.default_range_seconds)
            .unwrap_or(DEFAULT_RANGE_SECONDS),
    ))
}
fn interval(n: u64) -> usize {
    match n {
        0..=3600 => 0,
        3601..=86400 => 1,
        86401..=604_800 => 2,
        _ => 3,
    }
}
fn query(p: &ProductEventMetric, app: &str, presentation: Option<(&str, u64)>) -> Value {
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
    // PostHog has no multi-week interval, so every week-aligned bucket charts
    // at `week`: its alignment matters more than its exact width.
    let bucket = ["hour", "day", "week", "month"][match p.bucket {
        None => 1,
        Some(Bucket::Seconds(n)) => interval(n),
        Some(Bucket::Weeks(_)) => 2,
    }];
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
    if let Some((display, seconds)) = presentation {
        source["dateRange"] = if display == "ActionsLineGraph" {
            json!({"date_from":date_from(seconds)})
        } else {
            json!({"date_from":date_from(seconds),"explicitDate":true})
        };
        source["trendsFilter"] = json!({"display":display});
    }
    json!({"kind":"InsightVizNode","source":source})
}

#[derive(Clone, Copy)]
struct SqlWindow {
    seconds: u64,
    delay: u64,
}
impl SqlWindow {
    fn trailing(seconds: u64) -> Self {
        Self { seconds, delay: 0 }
    }
    fn lower(self) -> Result<String, SyncError> {
        let age = self
            .seconds
            .checked_add(self.delay)
            .ok_or_else(|| SyncError::Invalid("SQL window and evaluation delay overflow".into()))?;
        Ok(format!("now() - INTERVAL {age} SECOND"))
    }
    fn time_predicate(self) -> Result<String, SyncError> {
        let predicate = format!("timestamp >= {}", self.lower()?);
        Ok(if self.delay == 0 {
            predicate
        } else {
            format!(
                "{predicate} AND timestamp < now() - INTERVAL {} SECOND",
                self.delay
            )
        })
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum SqlDisplay {
    Number,
    Table,
    Line,
}
impl SqlDisplay {
    fn from_name(name: &str) -> Result<Self, SyncError> {
        Ok(match name {
            "BoldNumber" => Self::Number,
            "ActionsTable" => Self::Table,
            "ActionsLineGraph" => Self::Line,
            _ => {
                return Err(SyncError::Invalid(format!(
                    "unsupported SQL display {name:?}"
                )));
            }
        })
    }
    fn name(self) -> &'static str {
        match self {
            Self::Number => "BoldNumber",
            Self::Table => "ActionsTable",
            Self::Line => "ActionsLineGraph",
        }
    }
}
fn sql_query(sql: &str, display: SqlDisplay, dimensions: usize) -> Value {
    let mut node = json!({"kind":"DataVisualizationNode", "source":{"kind":"HogQLQuery","query":sql}, "display":display.name()});
    if display == SqlDisplay::Line && dimensions == 1 {
        node["chartSettings"] = json!({"seriesBreakdownColumn":"_dim_0"});
    }
    node
}
fn sql_quote(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "''"))
}
fn map_value(map: &str, key: &str) -> String {
    format!("{map}[{}]", sql_quote(key))
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum SqlSource {
    Logs,
    Spans,
    Events,
}
impl SqlSource {
    fn table(self) -> &'static str {
        match self {
            Self::Logs => "logs",
            Self::Spans => "posthog.trace_spans",
            Self::Events => "events",
        }
    }
    fn app_column(self, app: &str) -> String {
        match self {
            Self::Events => format!(
                "{} = {}",
                map_value("properties", "service.name"),
                sql_quote(app)
            ),
            Self::Logs | Self::Spans => format!("service_name = {}", sql_quote(app)),
        }
    }
    fn coverage(self, app: &str, lower: &str) -> String {
        format!(
            "(SELECT count() FROM (SELECT 1 FROM {} WHERE {} AND timestamp <= {lower} LIMIT 1)) > 0",
            self.table(),
            self.app_column(app)
        )
    }
}
fn path_sql(path: &FieldPath, source: SqlSource, numeric: bool) -> Result<String, SyncError> {
    let raw = match (path.root.as_str(), path.segments.as_slice(), source) {
        ("timestamp", [], _) => "timestamp".into(),
        ("fields", [PathSegment::Key(key)], SqlSource::Events) => map_value("properties", key),
        ("fields", [PathSegment::Key(key)], _) => map_value("attributes", key),
        ("message", [], SqlSource::Logs) => "body".into(),
        ("level", [], SqlSource::Logs) => "upper(level)".into(),
        ("target", [], SqlSource::Logs) => "splitByChar('@', instrumentation_scope)[1]".into(),
        ("target", [], SqlSource::Spans) => map_value("attributes", "target"),
        ("name", [], SqlSource::Spans) => "name".into(),
        ("duration", [], SqlSource::Spans) => "duration_nano / 1000".into(),
        ("semantic_kind" | "message" | "level" | "target" | "name" | "duration", [], _) => {
            "NULL".into()
        }
        _ => {
            return Err(SyncError::Invalid(format!(
                "unsupported field path {path:?}"
            )));
        }
    };
    if numeric && (raw.starts_with("attributes[") || raw.starts_with("properties[")) {
        Ok(format!("toFloatOrNull({raw})"))
    } else {
        Ok(raw)
    }
}
fn expr_path(expr: &Expr) -> Result<&FieldPath, SyncError> {
    match expr {
        Expr::Path { path } => Ok(path),
        _ => Err(SyncError::Invalid(format!("expected path, got {expr:?}"))),
    }
}
fn literal_sql(literal: &Literal, attribute: bool) -> Result<String, SyncError> {
    Ok(match literal {
        Literal::String(v) => sql_quote(v),
        Literal::Bool(v) if attribute => sql_quote(if *v { "true" } else { "false" }),
        Literal::Bool(v) => v.to_string(),
        Literal::Int(v) => v.to_string(),
        Literal::Float(v) => format!("{:e}", v.get()),
        _ => {
            return Err(SyncError::Invalid(format!(
                "unsupported filter literal {literal:?}"
            )));
        }
    })
}
fn semantic_predicate(value: &str, source: SqlSource) -> &'static str {
    match (value, source) {
        ("http.request", SqlSource::Spans) => {
            "attributes['target'] = 'cja::server::trace' AND attributes['kind'] = 'server'"
        }
        ("job.run", SqlSource::Spans) => "name = 'worker.run_job'",
        ("job.enqueue", SqlSource::Spans) => "name = 'jobs.enqueue'",
        ("cron.fire", SqlSource::Logs) => {
            "instrumentation_scope LIKE 'cja::cron::registry@%' AND body LIKE 'Enqueuing Task%' AND upper(level) = 'INFO'"
        }
        _ => "false",
    }
}
fn predicate_sql(expr: &Expr, source: SqlSource) -> Result<String, SyncError> {
    let Expr::Binary { op, left, right } = expr else {
        return Err(SyncError::Invalid(format!(
            "unsupported Where predicate {expr:?}"
        )));
    };
    let operator = match op {
        BinaryOp::Eq => "=",
        BinaryOp::Gte => ">=",
        _ => {
            return Err(SyncError::Invalid(format!(
                "unsupported Where operator {op:?}"
            )));
        }
    };
    let path = expr_path(left)?;
    let Expr::Literal { value } = right.as_ref() else {
        return Err(SyncError::Invalid(format!(
            "unsupported Where right side {right:?}"
        )));
    };
    if path.root == "semantic_kind" {
        if let (BinaryOp::Eq, Literal::String(value)) = (op, value) {
            if !matches!(
                value.as_str(),
                "http.request" | "job.run" | "job.enqueue" | "cron.fire"
            ) {
                return Err(SyncError::Invalid(format!(
                    "unsupported semantic_kind {value:?}"
                )));
            }
            return Ok(format!("({})", semantic_predicate(value, source)));
        }
        return Err(SyncError::Invalid(
            "semantic_kind requires string Eq".into(),
        ));
    }
    let numeric = matches!(value, Literal::Int(_) | Literal::Float(_));
    let field = path_sql(path, source, numeric)?;
    let attribute = field.contains("attributes[") || field.contains("properties[");
    Ok(format!(
        "{field} {operator} {}",
        literal_sql(value, attribute)?
    ))
}
fn aggregate_sql(function: EyesAggregate) -> &'static str {
    match function {
        EyesAggregate::Count => "count()",
        EyesAggregate::CountDistinct => "count(DISTINCT _metric_value)",
        EyesAggregate::Sum => "sum(_metric_value)",
        EyesAggregate::Avg => "avg(_metric_value)",
        EyesAggregate::Min => "min(_metric_value)",
        EyesAggregate::Max => "max(_metric_value)",
        EyesAggregate::P50 => "quantile(0.50)(_metric_value)",
        EyesAggregate::P90 => "quantile(0.90)(_metric_value)",
        EyesAggregate::P95 => "quantile(0.95)(_metric_value)",
        EyesAggregate::P99 => "quantile(0.99)(_metric_value)",
    }
}
#[derive(Clone)]
struct SqlPlan {
    sql: String,
    groups: Vec<String>,
    coverage: String,
}
fn group_sql(expr: &Expr, source: SqlSource) -> Result<String, SyncError> {
    if let Expr::Path { path } = expr {
        let field = path_sql(path, source, false)?;
        if field == "NULL" {
            return Err(SyncError::Invalid(format!(
                "cannot group by {path:?} on {}",
                source.table()
            )));
        }
        return Ok(field);
    }
    let Expr::Call {
        function: Function::TimeBucket,
        args,
    } = expr
    else {
        return Err(SyncError::Invalid(format!("unsupported group {expr:?}")));
    };
    let [
        Expr::Path { path },
        Expr::Literal {
            value: Literal::Duration(width),
        },
        rest @ ..,
    ] = args.as_slice()
    else {
        return Err(SyncError::Invalid(format!("invalid time bucket {expr:?}")));
    };
    if path.root != "timestamp"
        || !path.segments.is_empty()
        || *width <= 0
        || width % 1_000_000 != 0
    {
        return Err(SyncError::Invalid(format!(
            "unsupported time bucket {expr:?}"
        )));
    }
    let origin = match rest {
        [] => String::new(),
        [
            Expr::Literal {
                value: Literal::Duration(MONDAY_ORIGIN_US),
            },
        ] => ", toDateTime64('1970-01-05 00:00:00', 6, 'UTC')".into(),
        _ => {
            return Err(SyncError::Invalid(format!(
                "unsupported time bucket origin {expr:?}"
            )));
        }
    };
    Ok(format!(
        "toStartOfInterval(timestamp, INTERVAL {} SECOND{origin})",
        width / 1_000_000
    ))
}
fn group_aliases(
    dimensions: &[eyes_subscriber::eyes_query::NamedExpr],
    metric_id: &str,
) -> Result<Vec<String>, SyncError> {
    let has_bucket = dimensions.first().is_some_and(|g| {
        matches!(
            g.expr,
            Expr::Call {
                function: Function::TimeBucket,
                ..
            }
        )
    });
    for (i, group) in dimensions.iter().enumerate() {
        let expected = if has_bucket && i == 0 {
            "bucket".into()
        } else {
            format!("_dim_{}", i - usize::from(has_bucket))
        };
        if group.alias != expected {
            return Err(SyncError::Invalid(format!(
                "metric {metric_id}: unsupported group alias {}",
                group.alias
            )));
        }
    }
    Ok(dimensions.iter().map(|g| g.alias.clone()).collect())
}
fn ensure_bucket_rows(
    width_seconds: u64,
    window: SqlWindow,
    metric_id: &str,
) -> Result<(), SyncError> {
    if width_seconds == 0 || window.seconds / width_seconds >= MAX_SQL_ROWS {
        return Err(SyncError::Invalid(format!(
            "metric {metric_id}: window would exceed PostHog's {MAX_SQL_ROWS}-row SQL limit"
        )));
    }
    Ok(())
}
fn source_selection(predicates: &[&Expr], argument: Option<&Expr>) -> Vec<SqlSource> {
    let mut logs = false;
    let mut spans = false;
    for expr in predicates {
        if let Expr::Binary { left, right, .. } = expr
            && let Expr::Path { path } = left.as_ref()
        {
            match path.root.as_str() {
                "level" | "message" => logs = true,
                "name" | "duration" => spans = true,
                "fields" if matches!(path.segments.as_slice(), [PathSegment::Key(key)] if key == "event_type" || key == "metric_type") =>
                {
                    logs = true;
                }
                "semantic_kind" => {
                    if let Expr::Literal {
                        value: Literal::String(s),
                    } = right.as_ref()
                    {
                        if s == "cron.fire" {
                            logs = true;
                        } else {
                            spans = true;
                        }
                    }
                }
                _ => {}
            }
        }
    }
    if let Some(Expr::Path { path }) = argument {
        match path.root.as_str() {
            "message" | "level" => logs = true,
            "name" | "duration" => spans = true,
            _ => {}
        }
    }
    match (logs, spans) {
        (true, false) => vec![SqlSource::Logs],
        (false, true) => vec![SqlSource::Spans],
        _ => vec![SqlSource::Logs, SqlSource::Spans],
    }
}
fn render_plan(
    branches: &[String],
    sources: &[SqlSource],
    groups: &[String],
    aggregate: &str,
    empty_is_null: bool,
    app: &str,
    window: SqlWindow,
) -> Result<SqlPlan, SyncError> {
    let lower = window.lower()?;
    let coverage = sources
        .iter()
        .map(|s| s.coverage(app, &lower))
        .collect::<Vec<_>>()
        .join(" AND ");
    let rows = branches.join(" UNION ALL ");
    let grouped = !groups.is_empty();
    let aggregate = if empty_is_null {
        format!("if(count(_metric_value) = 0, NULL, {aggregate})")
    } else {
        aggregate.to_owned()
    };
    let projection = if grouped {
        format!("{aggregate} AS value, {}", groups.join(", "))
    } else {
        format!("if({coverage}, {aggregate}, NULL) AS value")
    };
    let group_by = if grouped {
        format!(" WHERE {coverage} GROUP BY {}", groups.join(", "))
    } else {
        String::new()
    };
    let limit = if grouped { MAX_SQL_ROWS } else { 1 };
    let sql = format!("SELECT {projection} FROM ({rows}) AS _source{group_by}");
    let sql = if grouped && groups.first().is_some_and(|g| g == "bucket") {
        let recent_order = descending_bucket_order(groups);
        format!(
            "SELECT * FROM (SELECT * FROM ({sql}) AS _grouped ORDER BY {recent_order} LIMIT {limit}) AS _recent ORDER BY {} LIMIT {limit}",
            groups.join(", ")
        )
    } else if grouped {
        format!("{sql} ORDER BY {} LIMIT {limit}", groups.join(", "))
    } else {
        format!("{sql} LIMIT {limit}")
    };
    Ok(SqlPlan {
        sql,
        groups: groups.to_vec(),
        coverage,
    })
}
fn descending_bucket_order(groups: &[String]) -> String {
    let mut order = String::from("bucket DESC");
    for group in groups.iter().skip(1) {
        order.push_str(", ");
        order.push_str(group);
    }
    order
}
fn telemetry_plan(
    metric: &TelemetryMetric,
    app: &str,
    window: SqlWindow,
) -> Result<SqlPlan, SyncError> {
    let named = metric.named_metric();
    let stages = &named
        .query
        .as_ref()
        .ok_or_else(|| SyncError::Invalid(format!("metric {}: missing query", named.id)))?
        .query
        .stages;
    let mut predicates = Vec::new();
    let mut aggregate = None;
    for (index, stage) in stages.iter().enumerate() {
        match stage {
            Stage::Where { predicate } if aggregate.is_none() => predicates.push(predicate),
            Stage::Aggregate { aggregates, groups }
                if aggregate.is_none() && aggregates.len() == 1 && index + 1 == stages.len() =>
            {
                aggregate = Some((&aggregates[0], groups));
            }
            _ => {
                return Err(SyncError::Invalid(format!(
                    "metric {}: unsupported stage {stage:?}",
                    named.id
                )));
            }
        }
    }
    let (aggregate, dimensions) = aggregate.ok_or_else(|| {
        SyncError::Invalid(format!("metric {}: missing single aggregate", named.id))
    })?;
    let argument = aggregate.argument.as_ref();
    if !aggregate.function.is_count() && argument.is_none() {
        return Err(SyncError::Invalid(format!(
            "metric {}: missing aggregate argument",
            named.id
        )));
    }
    let sources = source_selection(&predicates, argument);
    let group_names = group_aliases(dimensions, &named.id)?;
    if let Some(group) = dimensions.first()
        && let Expr::Call {
            function: Function::TimeBucket,
            args,
        } = &group.expr
        && let Some(Expr::Literal {
            value: Literal::Duration(micros),
        }) = args.get(1)
        && *micros > 0
    {
        ensure_bucket_rows((*micros).cast_unsigned() / 1_000_000, window, &named.id)?;
    }
    let mut branches = Vec::new();
    for source in &sources {
        let value = if let Some(argument) = argument {
            let path = expr_path(argument)
                .map_err(|e| SyncError::Invalid(format!("metric {}: {e}", named.id)))?;
            path_sql(
                path,
                *source,
                !matches!(aggregate.function, EyesAggregate::CountDistinct),
            )
            .map_err(|e| SyncError::Invalid(format!("metric {}: {e}", named.id)))?
        } else {
            "NULL".into()
        };
        let mut selected = vec![format!("{value} AS _metric_value")];
        for (i, group) in dimensions.iter().enumerate() {
            selected.push(format!(
                "{} AS {}",
                group_sql(&group.expr, *source).map_err(|e| SyncError::Invalid(format!(
                    "metric {} group {i}: {e}",
                    named.id
                )))?,
                group.alias
            ));
        }
        let mut conditions = vec![source.app_column(app), window.time_predicate()?];
        for predicate in &predicates {
            conditions.push(
                predicate_sql(predicate, *source)
                    .map_err(|e| SyncError::Invalid(format!("metric {}: {e}", named.id)))?,
            );
        }
        branches.push(format!(
            "SELECT {} FROM {} WHERE {}",
            selected.join(", "),
            source.table(),
            conditions.join(" AND ")
        ));
    }
    render_plan(
        &branches,
        &sources,
        &group_names,
        aggregate_sql(aggregate.function),
        !matches!(
            aggregate.function,
            EyesAggregate::Count | EyesAggregate::CountDistinct
        ),
        app,
        window,
    )
}
#[allow(dead_code)] // Pure SQL interface retained for direct contract tests.
fn telemetry_sql(
    metric: &TelemetryMetric,
    app: &str,
    window: SqlWindow,
) -> Result<String, SyncError> {
    Ok(telemetry_plan(metric, app, window)?.sql)
}
fn product_plan(
    metric: &ProductEventMetric,
    app: &str,
    window: SqlWindow,
) -> Result<SqlPlan, SyncError> {
    let (aggregate, property) = match &metric.aggregate {
        ProductAggregate::Count => ("count()", None),
        ProductAggregate::DistinctUsers => ("count(DISTINCT _metric_value)", None),
        ProductAggregate::Sum { property } => ("sum(_metric_value)", Some(property)),
        ProductAggregate::Avg { property } => ("avg(_metric_value)", Some(property)),
        ProductAggregate::Min { property } => ("min(_metric_value)", Some(property)),
        ProductAggregate::Max { property } => ("max(_metric_value)", Some(property)),
        ProductAggregate::Median { property } => ("quantile(0.50)(_metric_value)", Some(property)),
        ProductAggregate::P90 { property } => ("quantile(0.90)(_metric_value)", Some(property)),
        ProductAggregate::P95 { property } => ("quantile(0.95)(_metric_value)", Some(property)),
        ProductAggregate::P99 { property } => ("quantile(0.99)(_metric_value)", Some(property)),
    };
    let value = property.map_or_else(
        || match metric.aggregate {
            ProductAggregate::DistinctUsers => "distinct_id".into(),
            _ => "NULL".into(),
        },
        |property| format!("toFloatOrNull({})", map_value("properties", property)),
    );
    let mut groups = Vec::new();
    let mut selected = vec![format!("{value} AS _metric_value")];
    if let Some(bucket) = metric.bucket {
        let bucket = match bucket {
            Bucket::Seconds(seconds) if seconds > 0 => {
                ensure_bucket_rows(seconds, window, &metric.event)?;
                format!("toStartOfInterval(timestamp, INTERVAL {seconds} SECOND)")
            }
            Bucket::Weeks(weeks) if weeks > 0 => {
                let width = u64::from(weeks) * 604_800;
                ensure_bucket_rows(width, window, &metric.event)?;
                format!(
                    "toStartOfInterval(timestamp, INTERVAL {width} SECOND, toDateTime64('1970-01-05 00:00:00', 6, 'UTC'))"
                )
            }
            _ => return Err(SyncError::Invalid("invalid product bucket".into())),
        };
        groups.push("bucket".into());
        selected.push(format!("{bucket} AS bucket"));
    }
    if let Some(property) = &metric.breakdown_property {
        groups.push("_dim_0".into());
        selected.push(format!("{} AS _dim_0", map_value("properties", property)));
    }
    let mut conditions = vec![
        SqlSource::Events.app_column(app),
        window.time_predicate()?,
        format!("event = {}", sql_quote(&metric.event)),
    ];
    for filter in &metric.property_filters {
        let numeric = matches!(filter.value, FilterValue::Int(_) | FilterValue::Float(_));
        let raw = map_value("properties", &filter.property);
        let field = if numeric {
            format!("toFloatOrNull({raw})")
        } else {
            raw
        };
        let value = match &filter.value {
            FilterValue::Bool(v) => sql_quote(if *v { "true" } else { "false" }),
            FilterValue::Int(v) => v.to_string(),
            FilterValue::Float(v) => format!("{v:e}"),
            FilterValue::Str(v) => sql_quote(v),
        };
        conditions.push(format!(
            "{field} {} {value}",
            match filter.operator {
                FilterOperator::Eq => "=",
                FilterOperator::Gte => ">=",
            }
        ));
    }
    let branch = format!(
        "SELECT {} FROM events WHERE {}",
        selected.join(", "),
        conditions.join(" AND ")
    );
    render_plan(
        &[branch],
        &[SqlSource::Events],
        &groups,
        aggregate,
        !matches!(
            metric.aggregate,
            ProductAggregate::Count | ProductAggregate::DistinctUsers
        ),
        app,
        window,
    )
}
fn operand_plan(
    metric: &super::Metric,
    app: &str,
    window: SqlWindow,
) -> Result<SqlPlan, SyncError> {
    match &metric.query {
        MetricQuery::Telemetry(telemetry) => telemetry_plan(telemetry, app, window),
        MetricQuery::ProductEvent(product) => product_plan(product, app, window),
        MetricQuery::Ratio(_) => Err(SyncError::Invalid(format!(
            "ratio {} cannot be an operand",
            metric.id
        ))),
    }
}
fn ratio_plan(
    ratio: &RatioMetric,
    declarations: &Declarations,
    app: &str,
    window: SqlWindow,
) -> Result<SqlPlan, SyncError> {
    if ratio_mixes_clusters(ratio, declarations) {
        return Err(SyncError::Invalid(format!(
            "ratio {} mixes product events with logs/spans: PostHog cannot query both clusters together",
            ratio.numerator
        )));
    }
    let numerator = declarations
        .metrics
        .iter()
        .find(|m| m.id == ratio.numerator)
        .ok_or_else(|| {
            SyncError::Invalid(format!("missing ratio numerator {}", ratio.numerator))
        })?;
    let denominator = declarations
        .metrics
        .iter()
        .find(|m| m.id == ratio.denominator)
        .ok_or_else(|| {
            SyncError::Invalid(format!("missing ratio denominator {}", ratio.denominator))
        })?;
    let num_window = SqlWindow {
        seconds: ratio.numerator_window_seconds.unwrap_or(window.seconds),
        delay: window.delay,
    };
    let den_window = SqlWindow {
        seconds: ratio.denominator_window_seconds.unwrap_or(window.seconds),
        delay: window.delay,
    };
    let num = operand_plan(numerator, app, num_window)?;
    let den = operand_plan(denominator, app, den_window)?;
    if !den.groups.is_empty() && num.groups != den.groups {
        return Err(SyncError::Invalid("ratio operand groups differ".into()));
    }
    let expression = if ratio.as_percentage {
        "_num_value * 100 / _den_value"
    } else {
        "_num_value / _den_value"
    };
    let formula = format!(
        "if(isNull(_num_value) OR isNull(_den_value) OR _den_value = 0, NULL, {expression})"
    );
    let sql = if num.groups.is_empty() {
        format!(
            "SELECT {formula} AS value FROM (SELECT value AS _num_value FROM ({}) LIMIT 1) AS _num CROSS JOIN (SELECT value AS _den_value FROM ({}) LIMIT 1) AS _den LIMIT 1",
            num.sql, den.sql
        )
    } else if den.groups.is_empty() {
        let keys = num.groups.join(", ");
        format!(
            "SELECT {formula} AS value, {keys} FROM (SELECT value AS _num_value, {keys} FROM ({}) AS _num_rows) AS _num CROSS JOIN (SELECT value AS _den_value FROM ({}) LIMIT 1) AS _den ORDER BY {keys} LIMIT {MAX_SQL_ROWS}",
            num.sql, den.sql
        )
    } else {
        let keys = num.groups.join(", ");
        let recent = if num.groups.first().is_some_and(|group| group == "bucket") {
            format!(" ORDER BY {}", descending_bucket_order(&num.groups))
        } else {
            format!(" ORDER BY {keys}")
        };
        let grouped = format!(
            "SELECT if({}, if(isNull(max(num_present)), 0, max(numerator)), NULL) AS _num_value, max(denominator) AS _den_value, {keys} FROM (SELECT toNullable(value) AS numerator, NULL AS denominator, 1 AS num_present, {keys} FROM ({}) UNION ALL SELECT NULL AS numerator, toNullable(value) AS denominator, NULL AS num_present, {keys} FROM ({})) AS _combined GROUP BY {keys}{recent} LIMIT {MAX_SQL_ROWS}",
            num.coverage, num.sql, den.sql
        );
        let selected = format!("SELECT {formula} AS value, {keys} FROM ({grouped}) AS _ratios");
        if num.groups.first().is_some_and(|group| group == "bucket") {
            let recent_order = descending_bucket_order(&num.groups);
            format!(
                "SELECT * FROM ({selected} ORDER BY {recent_order} LIMIT {MAX_SQL_ROWS}) AS _recent ORDER BY {keys} LIMIT {MAX_SQL_ROWS}"
            )
        } else {
            format!("{selected} ORDER BY {keys} LIMIT {MAX_SQL_ROWS}")
        }
    };
    Ok(SqlPlan {
        sql,
        groups: num.groups,
        coverage: format!("{} AND {}", num.coverage, den.coverage),
    })
}
#[allow(dead_code)] // Pure SQL interface retained for direct contract tests.
fn ratio_sql(
    ratio: &RatioMetric,
    declarations: &Declarations,
    app: &str,
    window: SqlWindow,
) -> Result<String, SyncError> {
    Ok(ratio_plan(ratio, declarations, app, window)?.sql)
}
fn ratio_mixes_clusters(ratio: &RatioMetric, declarations: &Declarations) -> bool {
    let is_product = |id: &str| {
        declarations
            .metrics
            .iter()
            .find(|metric| metric.id == id)
            .is_some_and(|metric| matches!(metric.query, MetricQuery::ProductEvent(_)))
    };
    is_product(&ratio.numerator) != is_product(&ratio.denominator)
}
fn health_sql(app: &str) -> Result<String, SyncError> {
    let window = SqlWindow::trailing(86_400);
    let lower = window.lower()?;
    Ok(format!(
        "SELECT if({}, count(), NULL) AS value FROM logs WHERE {} AND upper(level) = 'ERROR' AND {} LIMIT 1",
        SqlSource::Logs.coverage(app, &lower),
        SqlSource::Logs.app_column(app),
        window.time_predicate()?
    ))
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
const LEFT_IN_PLACE: &str = "the existing object was left in place";
/// Remove an object `PostHog` just created without our identity. Left alone
/// it would be invisible to every later sync: one orphan per deploy.
async fn discard(api: &Api<'_>, kind: &str, row: &Value, key: &str) -> String {
    let removed = if kind == "alerts" {
        match alert_path(row) {
            Ok(path) => api
                .call(Method::DELETE, &format!("{}{path}", api.base), None, key)
                .await
                .map(drop),
            Err(e) => Err(e),
        }
    } else {
        match id(row, key) {
            Ok(n) => api
                .write(
                    Method::PATCH,
                    &format!("{kind}/{n}/"),
                    &json!({"deleted":true}),
                    key,
                )
                .await
                .map(drop),
            Err(e) => Err(e),
        }
    };
    match removed {
        Ok(()) => "the object just created was removed".into(),
        Err(e) => format!(
            "removing the object just created failed, remove it by hand: {}",
            message(&e)
        ),
    }
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
        && (kind != "insights" || insight_absences_match(&body, row))
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
        let cleanup = if old.is_some() {
            LEFT_IN_PLACE.into()
        } else {
            discard(api, kind, &row, key).await
        };
        return Err(SyncError::TagNotPersisted {
            object: key.into(),
            tag: key.into(),
            cleanup,
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
/// `PostHog` caps an alert name at this many characters.
const ALERT_NAME_LIMIT: usize = 255;
/// Display name plus the identity suffix that marks the alert as ours.
fn alert_name(app: &str, a: &Alert) -> String {
    format!(
        "{} [cja:{app}:alert:{}]",
        a.description.as_ref().unwrap_or(&a.id),
        a.id
    )
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
    skipped_metrics: &BTreeMap<&str, &'static str>,
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
            if let Some(reason) = skipped_metrics.get(a.metric_id.as_str()) {
                r.skipped.push(format!(
                    "{tag}: alert for {} skipped ({reason})",
                    a.metric_id
                ));
            } else {
                r.skipped.push(format!(
                    "{tag}: SQL metric {} alert (PostHog-unsupported: SQL alert payload and NULL evaluation unverified)", a.metric_id
                ));
            }
            continue;
        };
        let cadence =
            ["hourly", "daily", "weekly", "monthly"][interval(a.evaluation_interval_seconds)];
        let mut body = json!({
            "name": alert_name(app, a),
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
            let response = api
                .write(Method::PATCH, &alert_path(&row)?, &body, &tag)
                .await?;
            if alert_key(response["name"].as_str().unwrap_or_default(), app) != Some(a.id.as_str())
            {
                return Err(SyncError::TagNotPersisted {
                    object: tag.clone(),
                    tag,
                    cleanup: LEFT_IN_PLACE.into(),
                });
            }
            r.updated.push(tag);
        } else {
            // Creation requires a subscriber list. Only an operator edits it afterwards.
            body["subscribed_users"] = json!([]);
            let response = api.write(Method::POST, "alerts/", &body, &tag).await?;
            if alert_key(response["name"].as_str().unwrap_or_default(), app) != Some(a.id.as_str())
            {
                let cleanup = discard(api, "alerts", &response, &tag).await;
                return Err(SyncError::TagNotPersisted {
                    object: tag.clone(),
                    tag,
                    cleanup,
                });
            }
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
/// Insights a dashboard should tile, in declaration order. Items with no
/// `PostHog` equivalent are reported as skipped.
fn wanted_tiles(
    dash: &Dashboard,
    key: &str,
    presentations: &BTreeMap<(&str, u64), i64>,
    health: &BTreeMap<(String, usize), i64>,
    skipped_metrics: &BTreeMap<&str, &'static str>,
    skipped_presentations: &BTreeMap<(&str, u64), &'static str>,
    r: &mut SyncReport,
) -> Result<Vec<i64>, SyncError> {
    let mut wanted = vec![];
    let mut health_ordinal = 0;
    for item in dash.sections.iter().flat_map(|s| &s.items) {
        match (item, resolve_presentation(item, dash)) {
            (_, Some((mid, _, seconds))) => {
                if let Some(reason) = skipped_metrics
                    .get(mid)
                    .copied()
                    .or_else(|| skipped_presentations.get(&(mid, seconds)).copied())
                {
                    r.skipped
                        .push(format!("{key}: tile for {mid} skipped ({reason})"));
                    continue;
                }
                wanted.push(*presentations.get(&(mid, seconds)).ok_or_else(|| {
                    SyncError::Invalid(format!("{key}: missing insight for metric {mid}"))
                })?);
            }
            (Item::Links { .. }, None) => {
                r.skipped.push(format!("{key}: links item (Eyes-only)"));
            }
            (Item::HealthSummary { .. }, None) => {
                if let Some(id) = health.get(&(dash.id.clone(), health_ordinal)) {
                    wanted.push(*id);
                }
                health_ordinal += 1;
            }
            (_, None) => {
                return Err(SyncError::Invalid(format!(
                    "{key}: item lacks a presentation"
                )));
            }
        }
    }
    Ok(wanted)
}
fn alert_path(row: &Value) -> Result<String, SyncError> {
    let id = row["id"]
        .as_str()
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
        .ok_or_else(|| SyncError::Invalid("invalid alert UUID".into()))?;
    Ok(format!("alerts/{id}/"))
}
type Presentations<'a> = BTreeMap<&'a str, Vec<(&'static str, u64)>>;
struct Prepared<'a> {
    alerted_ids: BTreeSet<&'a str>,
    presentations: Presentations<'a>,
    insights: BTreeMap<String, Value>,
    health: BTreeMap<String, Value>,
    skipped_metrics: BTreeMap<&'a str, &'static str>,
    skipped_presentations: BTreeMap<(&'a str, u64), &'static str>,
    canonical_windows: BTreeMap<&'a str, u64>,
}
#[allow(clippy::too_many_lines)]
fn preflight<'a>(
    config: &SyncConfig,
    declarations: &'a Declarations,
) -> Result<Prepared<'a>, SyncError> {
    slug(&config.app)
        .and_then(|()| declarations.validate())
        .map_err(SyncError::Invalid)?;
    for alert in &declarations.alerts {
        let length = alert_name(&config.app, alert).chars().count();
        if length > ALERT_NAME_LIMIT {
            return Err(SyncError::AlertNameTooLong {
                alert: format!("cja:{}:alert:{}", config.app, alert.id),
                length,
                hint: if alert.description.is_some() {
                    "alert description"
                } else {
                    "alert ID"
                },
            });
        }
    }
    let product_ids = declarations
        .metrics
        .iter()
        .filter(|metric| matches!(metric.query, MetricQuery::ProductEvent(_)))
        .map(|metric| metric.id.as_str())
        .collect::<BTreeSet<_>>();
    let alerted_ids = declarations
        .alerts
        .iter()
        .filter(|alert| product_ids.contains(alert.metric_id.as_str()))
        .map(|alert| alert.metric_id.as_str())
        .collect::<BTreeSet<_>>();
    let mut declared_presentations: Presentations<'a> = BTreeMap::new();
    for dash in &declarations.dashboards {
        if let Some(seconds) = dash.default_range_seconds
            && !(60..=MAX_RANGE_SECONDS).contains(&seconds)
        {
            return Err(SyncError::Invalid(format!(
                "dashboard {}: default_range_seconds {seconds} must be 60..={MAX_RANGE_SECONDS}",
                dash.id
            )));
        }
        for item in dash.sections.iter().flat_map(|section| &section.items) {
            let Some((metric_id, display, seconds)) = resolve_presentation(item, dash) else {
                continue;
            };
            if let Some(window) = item.declared_window_seconds()
                && !(1..=MAX_RANGE_SECONDS).contains(&window)
            {
                return Err(SyncError::Invalid(format!(
                    "dashboard {} metric {metric_id}: window_seconds {window} must be 1..={MAX_RANGE_SECONDS}",
                    dash.id
                )));
            }
            let entries = declared_presentations.entry(metric_id).or_default();
            if !entries.iter().any(|(_, old)| *old == seconds) {
                entries.push((display, seconds));
            }
        }
    }
    for (metric_id, entries) in &declared_presentations {
        for (_, seconds) in entries
            .iter()
            .skip(usize::from(!alerted_ids.contains(metric_id)))
        {
            let key = format!("cja:{}:insight:{metric_id}@{seconds}s", config.app);
            if key.len() > TAG_LIMIT {
                return Err(SyncError::Invalid(format!(
                    "insight variant tag {key} is {} characters; PostHog allows at most {TAG_LIMIT}",
                    key.len()
                )));
            }
        }
    }
    for metric in &declarations.metrics {
        let key = format!("cja:{}:insight:{}", config.app, metric.id);
        if key.len() > TAG_LIMIT {
            return Err(SyncError::Invalid(format!(
                "insight tag {key} exceeds {TAG_LIMIT} characters"
            )));
        }
    }
    for dash in &declarations.dashboards {
        let dashboard_key = format!("cja:{}:dashboard:{}", config.app, dash.id);
        if dashboard_key.len() > TAG_LIMIT {
            return Err(SyncError::Invalid(format!(
                "dashboard tag {dashboard_key} exceeds {TAG_LIMIT} characters"
            )));
        }
        let mut ordinal = 0;
        for item in dash.sections.iter().flat_map(|section| &section.items) {
            if matches!(item, Item::HealthSummary { .. }) {
                let key = format!("cja:{}:insight:{}@health{ordinal}", config.app, dash.id);
                if key.len() > TAG_LIMIT {
                    return Err(SyncError::Invalid(format!(
                        "health tag {key} exceeds {TAG_LIMIT} characters"
                    )));
                }
                ordinal += 1;
            }
        }
    }
    let mut prepared = Prepared {
        alerted_ids,
        presentations: declared_presentations,
        insights: BTreeMap::new(),
        health: BTreeMap::new(),
        skipped_metrics: BTreeMap::new(),
        skipped_presentations: BTreeMap::new(),
        canonical_windows: BTreeMap::new(),
    };
    for metric in &declarations.metrics {
        if let MetricQuery::Ratio(ratio) = &metric.query
            && ratio_mixes_clusters(ratio, declarations)
        {
            prepared
                .skipped_metrics
                .insert(&metric.id, "ratio mixes product events with logs/spans");
            continue;
        }
        let declared = prepared.presentations.get(metric.id.as_str());
        let alerted = prepared.alerted_ids.contains(metric.id.as_str());
        let fallback = [("ActionsLineGraph", DEFAULT_RANGE_SECONDS)];
        let entries = declared.map_or(fallback.as_slice(), Vec::as_slice);
        let mut rendered = Vec::new();
        for &(display, seconds) in entries {
            let query = metric_query(metric, declarations, &config.app, display, seconds, false)?;
            if query.is_none() {
                prepared.skipped_presentations.insert(
                    (metric.id.as_str(), seconds),
                    "PostHog-unsupported: SQL line charts break down by one column",
                );
            }
            rendered.push((display, seconds, query));
        }
        let legacy =
            (alerted || declared.is_none()) && matches!(metric.query, MetricQuery::ProductEvent(_));
        let canonical = if legacy {
            metric_query(
                metric,
                declarations,
                &config.app,
                entries[0].0,
                entries[0].1,
                true,
            )?
        } else {
            rendered.iter().find_map(|(_, _, query)| query.clone())
        };
        let Some(canonical) = canonical else { continue };
        let first_supported = rendered.iter().find(|(_, _, query)| query.is_some());
        if !legacy && let Some((_, seconds, _)) = first_supported {
            prepared
                .canonical_windows
                .insert(metric.id.as_str(), *seconds);
        }
        let name = metric.display_name.as_ref().unwrap_or(&metric.id);
        let description = if matches!(metric.query, MetricQuery::ProductEvent(_)) {
            Value::Null
        } else {
            json!(SQL_COVERAGE_DESCRIPTION)
        };
        let key = format!("cja:{}:insight:{}", config.app, metric.id);
        prepared.insights.insert(
            key,
            json!({"name":name,"description":description,"query":canonical}),
        );
        if declared.is_some() {
            for (_, seconds, query) in rendered {
                let Some(query) = query else { continue };
                if !alerted && prepared.canonical_windows.get(metric.id.as_str()) == Some(&seconds)
                {
                    continue;
                }
                let key = format!("cja:{}:insight:{}@{seconds}s", config.app, metric.id);
                prepared.insights.insert(
                    key,
                    json!({"name":name,"description":description,"query":query}),
                );
            }
        }
    }
    for dash in &declarations.dashboards {
        let mut ordinal = 0;
        for item in dash.sections.iter().flat_map(|section| &section.items) {
            if let Item::HealthSummary { label } = item {
                let key = format!("cja:{}:insight:{}@health{ordinal}", config.app, dash.id);
                let name = format!(
                    "{}Recent error events (24h) — health substitute",
                    label.as_deref().map_or(String::new(), |v| format!("{v}: "))
                );
                let sql = health_sql(&config.app)?;
                prepared.health.insert(key, json!({"name":name,"description":SQL_COVERAGE_DESCRIPTION,"query":sql_query(&sql, SqlDisplay::Number, 0)}));
                ordinal += 1;
            }
        }
    }
    Ok(prepared)
}
fn metric_query(
    metric: &super::Metric,
    decl: &Declarations,
    app: &str,
    display: &str,
    seconds: u64,
    legacy: bool,
) -> Result<Option<Value>, SyncError> {
    Ok(match &metric.query {
        MetricQuery::ProductEvent(product) => {
            Some(query(product, app, (!legacy).then_some((display, seconds))))
        }
        MetricQuery::Telemetry(telemetry) => {
            let plan = telemetry_plan(telemetry, app, SqlWindow::trailing(seconds))?;
            sql_node_for_plan(&plan, display)?
        }
        MetricQuery::Ratio(ratio) => {
            let plan = ratio_plan(ratio, decl, app, SqlWindow::trailing(seconds))?;
            sql_node_for_plan(&plan, display)?
        }
    })
}
fn sql_node_for_plan(plan: &SqlPlan, display: &str) -> Result<Option<Value>, SyncError> {
    let display = SqlDisplay::from_name(display)?;
    let dimensions = plan
        .groups
        .iter()
        .filter(|group| group.as_str() != "bucket")
        .count();
    if display == SqlDisplay::Line && dimensions > 1 {
        return Ok(None);
    }
    Ok(Some(sql_query(&plan.sql, display, dimensions)))
}
async fn prune(
    api: &Api<'_>,
    dashboards: Index,
    insights: Index,
    keep: &BTreeSet<String>,
    report: &mut SyncReport,
) -> Result<(), SyncError> {
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
    Ok(())
}
async fn sync_dashboard_rows(
    api: &Api<'_>,
    declarations: &Declarations,
    dashboards: &mut Index,
    keep: &mut BTreeSet<String>,
    report: &mut SyncReport,
) -> Result<(), SyncError> {
    for dash in &declarations.dashboards {
        let key = format!("cja:{}:dashboard:{}", api.config.app, dash.id);
        let has_sql =
            dash.sections
                .iter()
                .flat_map(|section| &section.items)
                .any(|item| {
                    matches!(item, Item::HealthSummary { .. })
                        || item.reference().is_some_and(|(id, _)| {
                            declarations.metrics.iter().any(|m| {
                                m.id == id && !matches!(m.query, MetricQuery::ProductEvent(_))
                            })
                        })
                });
        let description = if has_sql {
            let note = "SQL tiles show NULL or no rows until the full source window has boundary coverage; health shows recent error events, not durable process/job state.";
            match dash.description.as_deref().filter(|s| !s.is_empty()) {
                Some(description) => format!("{description}\n{note}"),
                None => note.to_owned(),
            }
        } else {
            dash.description.clone().unwrap_or_default()
        };
        let body = json!({"name": dash.title, "description": description});
        upsert(api, "dashboards", &key, body, dashboards, report).await?;
        keep.insert(key);
    }
    Ok(())
}
async fn sync_health_rows(
    api: &Api<'_>,
    declarations: &Declarations,
    bodies: &BTreeMap<String, Value>,
    insights: &mut Index,
    keep: &mut BTreeSet<String>,
    report: &mut SyncReport,
) -> Result<BTreeMap<(String, usize), i64>, SyncError> {
    let mut ids = BTreeMap::new();
    for dash in &declarations.dashboards {
        let mut ordinal = 0;
        for item in dash.sections.iter().flat_map(|section| &section.items) {
            if let Item::HealthSummary { .. } = item {
                let key = format!("cja:{}:insight:{}@health{ordinal}", api.config.app, dash.id);
                let n = upsert(
                    api,
                    "insights",
                    &key,
                    bodies.get(&key).cloned().ok_or_else(|| {
                        SyncError::Invalid(format!("missing prepared health {key}"))
                    })?,
                    insights,
                    report,
                )
                .await?;
                ids.insert((dash.id.clone(), ordinal), n);
                keep.insert(key);
                ordinal += 1;
            }
        }
    }
    Ok(ids)
}
/// Apply a complete declaration set, including removal of stale owned objects.
#[allow(clippy::too_many_lines)]
pub async fn sync(
    config: &SyncConfig,
    declarations: &Declarations,
    http: &SyncClient,
) -> Result<SyncReport, SyncError> {
    let prepared = preflight(config, declarations)?;
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
    let mut presentation_ids = BTreeMap::new();
    sync_dashboard_rows(&api, declarations, &mut dashboards, &mut keep, &mut report).await?;
    for metric in &declarations.metrics {
        let key = tag("insight", &metric.id);
        if prepared.skipped_metrics.contains_key(metric.id.as_str()) {
            report.skipped.push(format!("{key}: ratio mixes product events with logs/spans (PostHog-unsupported: HogQL cannot join events and logs/spans in one query)"));
            continue;
        }
        let Some(body) = prepared.insights.get(&key).cloned() else {
            continue;
        };
        let declared = prepared.presentations.get(metric.id.as_str());
        let alerted = prepared.alerted_ids.contains(metric.id.as_str());
        let insight_id = upsert(&api, "insights", &key, body, &mut insights, &mut report).await?;
        if matches!(metric.query, MetricQuery::ProductEvent(_)) {
            ids.insert(metric.id.as_str(), insight_id);
        }
        keep.insert(key);
        if let Some(entries) = declared {
            for (_, seconds) in entries {
                if prepared
                    .skipped_presentations
                    .contains_key(&(metric.id.as_str(), *seconds))
                {
                    continue;
                }
                let n = if !alerted
                    && prepared.canonical_windows.get(metric.id.as_str()) == Some(seconds)
                {
                    insight_id
                } else {
                    let variant = tag("insight", &format!("{}@{seconds}s", metric.id));
                    let body = prepared.insights.get(&variant).cloned().ok_or_else(|| {
                        SyncError::Invalid(format!("missing prepared insight {variant}"))
                    })?;
                    let n = upsert(&api, "insights", &variant, body, &mut insights, &mut report)
                        .await?;
                    keep.insert(variant);
                    n
                };
                presentation_ids.insert((metric.id.as_str(), *seconds), n);
            }
        }
    }
    let health_ids = sync_health_rows(
        &api,
        declarations,
        &prepared.health,
        &mut insights,
        &mut keep,
        &mut report,
    )
    .await?;
    let owned = insights
        .iter()
        .map(|(key, v)| id(v, key))
        .collect::<Result<_, _>>()?;
    for dash in &declarations.dashboards {
        let key = tag("dashboard", &dash.id);
        let wanted = wanted_tiles(
            dash,
            &key,
            &presentation_ids,
            &health_ids,
            &prepared.skipped_metrics,
            &prepared.skipped_presentations,
            &mut report,
        )?;
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
    alerts(
        &api,
        declarations,
        alert_rows,
        &ids,
        &prepared.skipped_metrics,
        &mut report,
    )
    .await?;
    prune(&api, dashboards, insights, &keep, &mut report).await?;
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
/// Pass a client from [`SyncClient::new`].
pub async fn run_from_env(app: &str, d: &Declarations, http: &SyncClient) -> ExitCode {
    if let Err(e) = d.validate() {
        eprintln!("invalid declarations: {e}");
        return ExitCode::from(2);
    }
    match SyncConfig::from_env(app) {
        Ok(Some(c)) => match sync(&c, d, http).await {
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
        let source = SyncClient::new()
            .unwrap()
            .0
            .get("https://127.0.0.1:1/")
            .send()
            .await
            .unwrap_err();
        let err = SyncError::Http {
            object: "test-app".into(),
            operation: "listing",
            source: TransportError(source),
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
    fn presentation_kinds_and_ranges() {
        let dash = Dashboard::new("d", "D")
            .unwrap()
            .default_range_seconds(364 * 86_400);
        for (item, display, seconds) in [
            (Item::stat("m"), "BoldNumber", 364 * 86_400),
            (
                Item::table("m").window_seconds(604_800),
                "ActionsTable",
                604_800,
            ),
            (Item::time_series("m"), "ActionsLineGraph", 364 * 86_400),
        ] {
            assert_eq!(
                resolve_presentation(&item, &dash),
                Some(("m", display, seconds))
            );
        }
        assert_eq!(resolve_presentation(&Item::health_summary(), &dash), None);
        assert_eq!(resolve_presentation(&Item::links(vec![]), &dash), None);
        for (seconds, expected) in [
            (61, "-61s"),
            (300, "-5M"),
            (3_600, "-1h"),
            (604_800, "-7d"),
            (2_592_000, "-30d"),
            (31_449_600, "-364d"),
        ] {
            assert_eq!(date_from(seconds), expected);
        }
    }
    #[test]
    fn legacy_absences_require_a_legacy_query() {
        let wanted =
            json!({"query":{"source":{"kind":"TrendsQuery","dateRange":{"date_from":"-30d"}}}});
        let mut actual = wanted.clone();
        assert!(insight_absences_match(&wanted, &actual));
        actual["query"]["source"]["trendsFilter"] = json!({});
        assert!(insight_absences_match(&wanted, &actual));
        actual["query"]["source"]["trendsFilter"] = json!({"display":"ActionsLineGraph"});
        actual["query"]["source"]["dateRange"]["explicitDate"] = json!(false);
        assert!(insight_absences_match(&wanted, &actual));
        actual["query"]["source"]["trendsFilter"] = json!({"display":"BoldNumber"});
        assert!(!insight_absences_match(&wanted, &actual));
        actual["query"]["source"]["dateRange"]["explicitDate"] = json!(true);
        assert!(!insight_absences_match(&wanted, &actual));
        actual["query"]["source"]["trendsFilter"] = json!({});
        assert!(!insight_absences_match(&wanted, &actual));
        let line = json!({"query":{"source":{"kind":"TrendsQuery","dateRange":{"date_from":"-30d"},"trendsFilter":{"display":"ActionsLineGraph"}}}});
        let mut stored_line = line.clone();
        stored_line["query"]["source"]["dateRange"]["explicitDate"] = json!(true);
        assert!(!insight_absences_match(&line, &stored_line));
        let sql = json!({"query":{"kind":"DataVisualizationNode","source":{"kind":"HogQLQuery","query":"SELECT 1 AS value"},"display":"BoldNumber"}});
        let stored_sql = json!({"query":{"kind":"DataVisualizationNode","source":{"kind":"HogQLQuery","query":"SELECT 1 AS value","version":2},"display":"BoldNumber"}});
        assert!(insight_absences_match(&sql, &stored_sql));
    }
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
            query(&spec, "demo", None),
            json!({
 "kind":"InsightVizNode","source":{"kind":"TrendsQuery",
 "series":[{"kind":"EventsNode","event":"opened","math":"p95","math_property":"seconds","properties":[
 {"type":"event","key":"service.name","operator":"exact","value":"demo"},
 {"type":"event","key":"paid","operator":"exact","value":true},
 {"type":"event","key":"attempt","operator":"gte","value":2}]}],
 "interval":"hour","dateRange":{"date_from":"-30d"},"filterTestAccounts":false,
 "breakdownFilter":{"breakdowns":[{"property":"tier","type":"event"}]}}})
        );
        let presented = query(&spec, "demo", Some(("ActionsTable", 300)));
        assert_eq!(
            presented["source"]["dateRange"],
            json!({"date_from":"-5M","explicitDate":true})
        );
        assert_eq!(
            presented["source"]["trendsFilter"],
            json!({"display":"ActionsTable"})
        );
        assert_eq!(
            presented["source"]["series"],
            query(&spec, "demo", None)["source"]["series"]
        );
        assert_eq!(
            presented["source"]["breakdownFilter"],
            query(&spec, "demo", None)["source"]["breakdownFilter"]
        );
        assert_eq!(presented["source"]["interval"], "hour");
        let line = query(&spec, "demo", Some(("ActionsLineGraph", 604_800)));
        assert_eq!(line["source"]["dateRange"], json!({"date_from":"-7d"}));
        assert_eq!(
            line["source"]["trendsFilter"],
            json!({"display":"ActionsLineGraph"})
        );
        let count = ProductMetricBuilder::count("sessions", "opened")
            .build()
            .unwrap();
        let MetricQuery::ProductEvent(spec) = count.query else {
            panic!("expected product metric")
        };
        let q = query(&spec, "demo", None);
        assert_eq!(q["source"]["interval"], "day");
        for weeks in [1, 2] {
            let weekly = ProductMetricBuilder::count("weekly", "opened")
                .bucket_weeks(weeks)
                .build()
                .unwrap();
            let MetricQuery::ProductEvent(spec) = weekly.query else {
                panic!("expected product metric")
            };
            assert_eq!(query(&spec, "demo", None)["source"]["interval"], "week");
        }
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
