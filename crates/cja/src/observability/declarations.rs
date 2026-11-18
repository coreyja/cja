//! App-owned declarations shared by both observability sinks.
use super::{AggregateFunction, MetricValueType};
use eyes_subscriber as eyes;
/// A named dashboard with ordered sections and a stable remote identity.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct Dashboard {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub default_range_seconds: Option<u64>,
    pub sections: Vec<Section>,
}

/// One titled grouping of dashboard items.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct Section {
    pub title: Option<String>,
    pub description: Option<String>,
    pub items: Vec<Item>,
}

/// Presentation of a metric, health summary or navigation links on a dashboard.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub enum Item {
    Stat {
        metric_id: String,
        label: Option<String>,
        unit: Option<String>,
        window_seconds: Option<u64>,
    },
    TimeSeries {
        metric_id: String,
        label: Option<String>,
        unit: Option<String>,
        preferred_bucket_seconds: Option<u64>,
        window_seconds: Option<u64>,
    },
    Table {
        metric_id: String,
        label: Option<String>,
        unit: Option<String>,
        window_seconds: Option<u64>,
    },
    HealthSummary {
        label: Option<String>,
    },
    Links {
        links: Vec<Link>,
    },
}

/// One navigation link: rendered, never fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub label: String,
    pub href: String,
}

/// Dashboard-id grammar (the named-metric id grammar): non-empty, trimmed,
/// ASCII, at most 128 bytes, chars in `[A-Za-z0-9._:-]`.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id.trim() == id
        && id.is_ascii()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
}

/// Link href grammar: root-relative path (starts with `/` but not `//` —
/// protocol-relative URLs resolve as scheme-relative external URLs) or an
/// absolute `http(s)://` URL, with no backslash anywhere (WHATWG parsing
/// treats `\` as `/` for special schemes, so `/\host` becomes `//host`), no
/// ASCII control bytes, at most 512 chars. Links are rendered, never
/// fetched — no SSRF machinery applies; the grammar check is the whole
/// rule.
fn valid_href(href: &str) -> bool {
    !href.is_empty()
        && href.chars().count() <= 512
        && !href.chars().any(|c| c.is_ascii_control())
        && !href.contains('\\')
        && (href.starts_with("http://")
            || href.starts_with("https://")
            || (href.starts_with('/') && !href.starts_with("//")))
}

impl Dashboard {
    /// Start a declaration. **Fallible**: validates the id grammar
    /// (non-empty, `[A-Za-z0-9._:-]`, ASCII, at most 128 bytes, trimmed)
    /// and the title (non-empty after trim, at most 256 chars) — a bad
    /// declaration fails at boot in the app.
    pub fn new(id: impl Into<String>, title: impl Into<String>) -> Result<Self, String> {
        let id = id.into();
        if !valid_id(&id) {
            return Err(format!(
                "invalid dashboard id {id:?}: must be non-empty, trimmed, ASCII, at most 128 bytes, chars [A-Za-z0-9._:-]"
            ));
        }
        let title = title.into();
        if title.trim().is_empty() || title.chars().count() > 256 {
            return Err(format!(
                "invalid dashboard title {title:?}: must be non-empty after trim and at most 256 characters"
            ));
        }
        Ok(Self {
            id,
            title,
            description: None,
            default_range_seconds: None,
            sections: Vec::new(),
        })
    }

    /// **Infallible**: the server rejects descriptions over 4096 chars at
    /// manifest ingestion.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// **Infallible**: stores the value verbatim; the server rejects
    /// anything outside `60..=31_622_400` seconds (one minute to 366
    /// days, the unified engine's range cap) at manifest ingestion.
    pub fn default_range_seconds(mut self, seconds: u64) -> Self {
        self.default_range_seconds = Some(seconds);
        self
    }

    /// **Infallible**: appends a section; the server rejects zero sections
    /// and more than 20 at ingestion. The builder stays permissive, like
    /// `HttpMonitor`'s setters.
    pub fn section(mut self, section: Section) -> Self {
        self.sections.push(section);
        self
    }
}

impl Section {
    pub fn new() -> Self {
        Self {
            title: None,
            description: None,
            items: Vec::new(),
        }
    }

    /// **Infallible**: the server enforces the 256-char cap at ingestion.
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// **Infallible**: the server enforces the 4096-char cap at ingestion.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// **Infallible**: appends an item; the server rejects empty sections
    /// and more than 24 items at ingestion.
    pub fn item(mut self, item: Item) -> Self {
        self.items.push(item);
        self
    }
}

impl Default for Section {
    fn default() -> Self {
        Self::new()
    }
}

impl Item {
    /// A single-value item rendering a `scalar` named metric.
    /// **Infallible**: a `metric_id` is an opaque reference to a
    /// same-manifest metric; the server rejects unknown ones at ingest.
    pub fn stat(metric_id: impl Into<String>) -> Self {
        Self::Stat {
            metric_id: metric_id.into(),
            label: None,
            unit: None,
            window_seconds: None,
        }
    }

    /// A chart item rendering a `time_series` named metric.
    /// **Infallible**: see [`Item::stat`].
    pub fn time_series(metric_id: impl Into<String>) -> Self {
        Self::TimeSeries {
            metric_id: metric_id.into(),
            label: None,
            unit: None,
            preferred_bucket_seconds: None,
            window_seconds: None,
        }
    }

    /// A table item rendering a `table` named metric.
    /// **Infallible**: see [`Item::stat`].
    pub fn table(metric_id: impl Into<String>) -> Self {
        Self::Table {
            metric_id: metric_id.into(),
            label: None,
            unit: None,
            window_seconds: None,
        }
    }

    /// Renders the app's durable run-health state; carries no query.
    pub fn health_summary() -> Self {
        Self::HealthSummary { label: None }
    }

    /// A navigation link list; carries no query.
    pub fn links(links: Vec<Link>) -> Self {
        Self::Links { links }
    }

    /// **Infallible**: sets the display label, overriding the referenced
    /// metric's `display_name`. The server enforces the 256-char cap at
    /// ingestion. No documented no-op case: every item kind carries a
    /// `label`.
    pub fn label(mut self, label: impl Into<String>) -> Self {
        let new_label = Some(label.into());
        match &mut self {
            Self::Stat { label, .. }
            | Self::TimeSeries { label, .. }
            | Self::Table { label, .. }
            | Self::HealthSummary { label } => *label = new_label,
            Self::Links { .. } => {}
        }
        self
    }

    /// **Infallible**: sets the display unit, overriding the referenced
    /// metric's `unit`. When the referenced metric declares a unit, the
    /// server requires exact equality at ingest; over a unit-less metric
    /// any valid unit text is allowed. The builder does not duplicate that
    /// check. Documented no-op on `health_summary` and `links`, which
    /// carry no unit field — returns `self` unchanged.
    pub fn unit(mut self, unit: impl Into<String>) -> Self {
        let new_unit = Some(unit.into());
        match &mut self {
            Self::Stat { unit, .. } | Self::TimeSeries { unit, .. } | Self::Table { unit, .. } => {
                *unit = new_unit;
            }
            Self::HealthSummary { .. } | Self::Links { .. } => {}
        }
        self
    }

    /// **Infallible**: bucket-width guidance for callers of the execute
    /// endpoint (which already accepts `bucket_seconds`); the server never
    /// rewrites queries for it. Documented no-op on every kind except
    /// `time_series` — returns `self` unchanged.
    pub fn preferred_bucket_seconds(mut self, seconds: u64) -> Self {
        if let Self::TimeSeries {
            preferred_bucket_seconds,
            ..
        } = &mut self
        {
            *preferred_bucket_seconds = Some(seconds);
        }
        self
    }

    /// **Infallible**: declares a fixed trailing window for this item —
    /// `seconds` back from the dashboard page's window end at render time —
    /// overriding the page range for this item alone. The server rejects
    /// anything outside `1..=31_622_400` seconds (one second to 366 days)
    /// at manifest ingestion. Documented no-op on `health_summary` and
    /// `links`, which carry no window — returns `self` unchanged.
    pub fn window_seconds(mut self, seconds: u64) -> Self {
        let new_window = Some(seconds);
        match &mut self {
            Self::Stat { window_seconds, .. }
            | Self::TimeSeries { window_seconds, .. }
            | Self::Table { window_seconds, .. } => *window_seconds = new_window,
            Self::HealthSummary { .. } | Self::Links { .. } => {}
        }
        self
    }
}

impl Link {
    /// **Fallible**: validates the href grammar (root-relative `/…` but not
    /// `//…`, no backslash anywhere, or `http(s)://…`; no ASCII control
    /// bytes; at most 512 chars) and the label (non-empty after trim, at
    /// most 256 chars) so a bad link fails at boot — the same grammar the
    /// server enforces.
    pub fn new(label: impl Into<String>, href: impl Into<String>) -> Result<Self, String> {
        let label = label.into();
        if label.trim().is_empty() || label.chars().count() > 256 {
            return Err(format!(
                "invalid dashboard link label {label:?}: must be non-empty after trim and at most 256 characters"
            ));
        }
        let href = href.into();
        if !valid_href(&href) {
            return Err(format!(
                "invalid dashboard link href {href:?}: must be a root-relative path (not //), an http(s) URL, at most 512 characters, with no backslash or control characters"
            ));
        }
        Ok(Self { label, href })
    }
}

/// Direction in which a threshold is breached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Comparison {
    Above,
    Below,
}

/// One severity policy of a metric threshold: a comparison direction and
/// the boundary value. The threshold crosses the wire as an `f64` number —
/// serde emits `1.0` for whole values, and the server validates by value,
/// not JSON representation, so `1.0` is accepted for an int metric.
#[derive(Debug, Clone, PartialEq)]
pub struct Severity {
    pub comparison: Comparison,
    pub threshold: f64,
}

/// How multi-row results feed a threshold breach: `Any` breaches when at
/// least one numeric cell breaches; `All` when every numeric cell does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupMatch {
    Any,
    All,
}

/// A metric-threshold declaration carried by the boot manifest: references
/// a named metric declared in the same manifest by `metric_id` (no query
/// embedded — the server joins by id). `PartialEq` only (f64 thresholds),
/// mirroring `NamedMetric`.
#[derive(Debug, Clone, PartialEq)]
pub struct Alert {
    pub id: String,
    pub metric_id: String,
    pub description: Option<String>,
    pub window_seconds: u64,
    pub evaluation_interval_seconds: u64,
    pub evaluation_delay_seconds: u64,
    pub group_match: GroupMatch,
    pub failure_threshold: u32,
    pub recovery_threshold: u32,
    pub enabled: bool,
    pub warning: Option<Severity>,
    pub critical: Option<Severity>,
}

/// The ergonomic Rust surface for declaring a metric threshold. All
/// validation happens in [`AlertBuilder::build`] — no I/O, no
/// server round trip. The builder does **not** enforce int-range on
/// severity thresholds: the subscriber cannot know the referenced metric's
/// `value_type`; out-of-domain values are rejected server-side at ingest.
#[derive(Debug)]
#[must_use]
pub struct AlertBuilder {
    id: String,
    metric_id: String,
    description: Option<String>,
    window_seconds: Option<u64>,
    evaluation_interval_seconds: Option<u64>,
    evaluation_delay_seconds: u64,
    group_match: GroupMatch,
    failure_threshold: u32,
    recovery_threshold: u32,
    enabled: bool,
    warning: Option<Severity>,
    critical: Option<Severity>,
}

impl AlertBuilder {
    /// Start a declaration referencing `metric_id` (which must be declared
    /// in the same manifest's `metrics`).
    pub fn new(id: impl Into<String>, metric_id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            metric_id: metric_id.into(),
            description: None,
            window_seconds: None,
            evaluation_interval_seconds: None,
            evaluation_delay_seconds: 0,
            group_match: GroupMatch::Any,
            failure_threshold: 3,
            recovery_threshold: 1,
            enabled: true,
            warning: None,
            critical: None,
        }
    }

    pub fn window_seconds(mut self, v: u64) -> Self {
        self.window_seconds = Some(v);
        self
    }
    pub fn evaluation_interval_seconds(mut self, v: u64) -> Self {
        self.evaluation_interval_seconds = Some(v);
        self
    }
    pub fn evaluation_delay_seconds(mut self, v: u64) -> Self {
        self.evaluation_delay_seconds = v;
        self
    }
    pub fn group_match(mut self, v: GroupMatch) -> Self {
        self.group_match = v;
        self
    }
    pub fn failure_threshold(mut self, v: u32) -> Self {
        self.failure_threshold = v;
        self
    }
    pub fn recovery_threshold(mut self, v: u32) -> Self {
        self.recovery_threshold = v;
        self
    }
    pub fn enabled(mut self, v: bool) -> Self {
        self.enabled = v;
        self
    }
    pub fn description(mut self, v: impl Into<String>) -> Self {
        self.description = Some(v.into());
        self
    }
    /// Declare (or replace) the warning severity policy.
    pub fn warning(mut self, comparison: Comparison, threshold: f64) -> Self {
        self.warning = Some(Severity {
            comparison,
            threshold,
        });
        self
    }
    /// Declare (or replace) the critical severity policy.
    pub fn critical(mut self, comparison: Comparison, threshold: f64) -> Self {
        self.critical = Some(Severity {
            comparison,
            threshold,
        });
        self
    }

    /// Assemble + validate: window `1..=2_678_400`, interval `10..=86_400`,
    /// delay 0..=600, failure/recovery thresholds 1..=100, at least one
    /// severity, finite severity thresholds. Out-of-domain (int-range)
    /// thresholds are the server's job — see the struct docs.
    pub fn build(self) -> Result<Alert, String> {
        let Self {
            id,
            metric_id,
            description,
            window_seconds,
            evaluation_interval_seconds,
            evaluation_delay_seconds,
            group_match,
            failure_threshold,
            recovery_threshold,
            enabled,
            warning,
            critical,
        } = self;
        let Some(window_seconds) = window_seconds else {
            return Err(format!("metric threshold {id}: window_seconds is required"));
        };
        if !(1..=2_678_400).contains(&window_seconds) {
            return Err(format!(
                "metric threshold {id}: window_seconds must be 1..=2678400"
            ));
        }
        let Some(evaluation_interval_seconds) = evaluation_interval_seconds else {
            return Err(format!(
                "metric threshold {id}: evaluation_interval_seconds is required"
            ));
        };
        if !(10..=86_400).contains(&evaluation_interval_seconds) {
            return Err(format!(
                "metric threshold {id}: evaluation_interval_seconds must be 10..=86400"
            ));
        }
        if evaluation_delay_seconds > 600 {
            return Err(format!(
                "metric threshold {id}: evaluation_delay_seconds must be 0..=600"
            ));
        }
        for (label, value) in [
            ("failure_threshold", failure_threshold),
            ("recovery_threshold", recovery_threshold),
        ] {
            if !(1..=100).contains(&value) {
                return Err(format!("metric threshold {id}: {label} must be 1..=100"));
            }
        }
        if warning.is_none() && critical.is_none() {
            return Err(format!(
                "metric threshold {id}: at least one of warning or critical is required"
            ));
        }
        for (label, policy) in [("warning", &warning), ("critical", &critical)] {
            if let Some(policy) = policy
                && !policy.threshold.is_finite()
            {
                return Err(format!(
                    "metric threshold {id}: {label} threshold must be finite"
                ));
            }
        }
        Ok(Alert {
            id,
            metric_id,
            description,
            window_seconds,
            evaluation_interval_seconds,
            evaluation_delay_seconds,
            group_match,
            failure_threshold,
            recovery_threshold,
            enabled,
            warning,
            critical,
        })
    }
}
/// Scalar event property used in a metric filter.
#[derive(Debug, Clone, PartialEq)]
pub enum FilterValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
}

impl From<bool> for FilterValue {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}
impl From<i64> for FilterValue {
    fn from(value: i64) -> Self {
        Self::Int(value)
    }
}
impl From<f64> for FilterValue {
    fn from(value: f64) -> Self {
        Self::Float(value)
    }
}
impl From<&str> for FilterValue {
    fn from(value: &str) -> Self {
        Self::Str(value.to_owned())
    }
}
impl From<String> for FilterValue {
    fn from(value: String) -> Self {
        Self::Str(value)
    }
}

/// A metric with a stable ID and editable presentation metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct Metric {
    pub id: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub unit: Option<String>,
    pub query: MetricQuery,
}
/// The source and aggregation used by a metric.
#[derive(Debug, Clone, PartialEq)]
pub enum MetricQuery {
    Telemetry(TelemetryMetric),
    ProductEvent(ProductEventMetric),
}
/// An Eyes telemetry query validated by the telemetry builder.
#[derive(Debug, Clone, PartialEq)]
pub struct TelemetryMetric(eyes::NamedMetric);
/// Build a span or log metric for the Eyes manifest.
#[derive(Debug)]
#[must_use]
pub struct TelemetryMetricBuilder {
    inner: eyes::NamedMetricBuilder,
    filter_count: usize,
    float_literals: Vec<(usize, eyes::eyes_query::FiniteF64)>,
}
impl From<FilterValue> for eyes::MetricFilterValue {
    fn from(v: FilterValue) -> Self {
        match v {
            FilterValue::Int(x) => Self::Int(x),
            FilterValue::Str(x) => Self::Str(x),
            FilterValue::Bool(x) => Self::Bool(x),
            FilterValue::Float(x) => Self::Float(x),
        }
    }
}
impl TelemetryMetricBuilder {
    pub fn new(
        id: impl Into<String>,
        function: AggregateFunction,
        argument: Option<&str>,
    ) -> Result<Self, String> {
        Ok(Self {
            inner: eyes::NamedMetricBuilder::new(id, function, argument)?,
            filter_count: 0,
            float_literals: Vec::new(),
        })
    }
    fn filter(
        mut self,
        path: &str,
        operator: FilterOperator,
        value: FilterValue,
    ) -> Result<Self, String> {
        // The Eyes builder validates using a serde round-trip. With serde_json's
        // arbitrary_precision feature, serde's tagged-enum buffer cannot decode
        // floats. Validate the same predicate with an integer operand, then
        // restore its checked finite literal in the typed IR.
        let operand = if let FilterValue::Float(number) = value {
            self.float_literals
                .push((self.filter_count, eyes::eyes_query::FiniteF64::new(number)?));
            eyes::MetricFilterValue::Int(0)
        } else {
            value.into()
        };
        self.inner = match operator {
            FilterOperator::Eq => self.inner.filter_eq(path, operand)?,
            FilterOperator::Gte => self.inner.filter_gte(path, operand)?,
        };
        self.filter_count += 1;
        Ok(self)
    }
    pub fn filter_eq(self, path: &str, value: impl Into<FilterValue>) -> Result<Self, String> {
        self.filter(path, FilterOperator::Eq, value.into())
    }
    pub fn filter_gte(self, path: &str, value: impl Into<FilterValue>) -> Result<Self, String> {
        self.filter(path, FilterOperator::Gte, value.into())
    }
    pub fn filter_numeric(self, path: &str) -> Result<Self, String> {
        self.filter_gte(path, f64::MIN)
    }
    pub fn group_by(self, path: &str) -> Result<Self, String> {
        Ok(Self {
            inner: self.inner.group_by(path)?,
            ..self
        })
    }
    pub fn display_name(self, v: impl Into<String>) -> Self {
        Self {
            inner: self.inner.display_name(v),
            ..self
        }
    }
    pub fn description(self, v: impl Into<String>) -> Self {
        Self {
            inner: self.inner.description(v),
            ..self
        }
    }
    pub fn unit(self, v: impl Into<String>) -> Self {
        Self {
            inner: self.inner.unit(v),
            ..self
        }
    }
    pub fn value_type(self, v: MetricValueType) -> Self {
        Self {
            inner: self.inner.value_type(v),
            ..self
        }
    }
    pub fn preferred_bucket_seconds(self, v: u64) -> Self {
        Self {
            inner: self.inner.preferred_bucket_seconds(v),
            ..self
        }
    }
    pub fn time_bucket(self, v: u64) -> Self {
        Self {
            inner: self.inner.time_bucket(v),
            ..self
        }
    }
    pub fn time_bucket_weeks(self, v: u32) -> Self {
        Self {
            inner: self.inner.time_bucket_weeks(v),
            ..self
        }
    }
    fn build_eyes(self) -> Result<eyes::NamedMetric, String> {
        use eyes::eyes_query::{Expr, Literal, Stage};
        let mut metric = self.inner.build()?;
        for (index, number) in self.float_literals {
            let stage = metric
                .query
                .as_mut()
                .and_then(|doc| doc.query.stages.get_mut(index));
            let Some(Stage::Where {
                predicate: Expr::Binary { right, .. },
            }) = stage
            else {
                return Err(format!(
                    "metric {}: missing filter stage {index}",
                    metric.id
                ));
            };
            **right = Expr::Literal {
                value: Literal::Float(number),
            };
        }
        Ok(metric)
    }
    pub fn build(self) -> Result<Metric, String> {
        let m = self.build_eyes()?;
        Ok(Metric {
            id: m.id.clone(),
            display_name: m.display_name.clone(),
            description: m.description.clone(),
            unit: m.unit.clone(),
            query: MetricQuery::Telemetry(TelemetryMetric(m)),
        })
    }
}

/// One predicate applied to the event series in both sinks.
#[derive(Debug, Clone, PartialEq)]
pub struct PropertyFilter {
    pub property: String,
    pub operator: FilterOperator,
    pub value: FilterValue,
}
/// Comparison operators shared by the Eyes and product-event backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterOperator {
    Eq,
    Gte,
}
/// A single event series with optional property aggregation, filters and grouping.
#[derive(Debug, Clone, PartialEq)]
pub struct ProductEventMetric {
    pub event: String,
    pub aggregate: ProductAggregate,
    pub property_filters: Vec<PropertyFilter>,
    pub breakdown_property: Option<String>,
    pub bucket_seconds: Option<u64>,
}
/// Aggregation over matching product events.
#[derive(Debug, Clone, PartialEq)]
pub enum ProductAggregate {
    Count,
    DistinctUsers,
    Sum { property: String },
    Avg { property: String },
    Min { property: String },
    Max { property: String },
    Median { property: String },
    P90 { property: String },
    P95 { property: String },
    P99 { property: String },
}
/// Build a product metric shared by Eyes and `PostHog`.
#[derive(Debug)]
#[must_use]
pub struct ProductMetricBuilder {
    id: String,
    display_name: Option<String>,
    description: Option<String>,
    unit: Option<String>,
    spec: ProductEventMetric,
}
impl ProductMetricBuilder {
    fn new(id: impl Into<String>, event: impl Into<String>, aggregate: ProductAggregate) -> Self {
        Self {
            id: id.into(),
            display_name: None,
            description: None,
            unit: None,
            spec: ProductEventMetric {
                event: event.into(),
                aggregate,
                property_filters: vec![],
                breakdown_property: None,
                bucket_seconds: None,
            },
        }
    }
    pub fn count(id: impl Into<String>, event: impl Into<String>) -> Self {
        Self::new(id, event, ProductAggregate::Count)
    }
    pub fn distinct_users(id: impl Into<String>, event: impl Into<String>) -> Self {
        Self::new(id, event, ProductAggregate::DistinctUsers)
    }
    pub fn sum(
        id: impl Into<String>,
        event: impl Into<String>,
        property: impl Into<String>,
    ) -> Self {
        Self::new(
            id,
            event,
            ProductAggregate::Sum {
                property: property.into(),
            },
        )
    }
    pub fn avg(
        id: impl Into<String>,
        event: impl Into<String>,
        property: impl Into<String>,
    ) -> Self {
        Self::new(
            id,
            event,
            ProductAggregate::Avg {
                property: property.into(),
            },
        )
    }
    pub fn min(
        id: impl Into<String>,
        event: impl Into<String>,
        property: impl Into<String>,
    ) -> Self {
        Self::new(
            id,
            event,
            ProductAggregate::Min {
                property: property.into(),
            },
        )
    }
    pub fn max(
        id: impl Into<String>,
        event: impl Into<String>,
        property: impl Into<String>,
    ) -> Self {
        Self::new(
            id,
            event,
            ProductAggregate::Max {
                property: property.into(),
            },
        )
    }
    pub fn median(
        id: impl Into<String>,
        event: impl Into<String>,
        property: impl Into<String>,
    ) -> Self {
        Self::new(
            id,
            event,
            ProductAggregate::Median {
                property: property.into(),
            },
        )
    }
    pub fn p90(
        id: impl Into<String>,
        event: impl Into<String>,
        property: impl Into<String>,
    ) -> Self {
        Self::new(
            id,
            event,
            ProductAggregate::P90 {
                property: property.into(),
            },
        )
    }
    pub fn p95(
        id: impl Into<String>,
        event: impl Into<String>,
        property: impl Into<String>,
    ) -> Self {
        Self::new(
            id,
            event,
            ProductAggregate::P95 {
                property: property.into(),
            },
        )
    }
    pub fn p99(
        id: impl Into<String>,
        event: impl Into<String>,
        property: impl Into<String>,
    ) -> Self {
        Self::new(
            id,
            event,
            ProductAggregate::P99 {
                property: property.into(),
            },
        )
    }
    pub fn display_name(mut self, v: impl Into<String>) -> Self {
        self.display_name = Some(v.into());
        self
    }
    pub fn description(mut self, v: impl Into<String>) -> Self {
        self.description = Some(v.into());
        self
    }
    pub fn unit(mut self, v: impl Into<String>) -> Self {
        self.unit = Some(v.into());
        self
    }

    pub fn filter(
        mut self,
        property: impl Into<String>,
        operator: FilterOperator,
        value: impl Into<FilterValue>,
    ) -> Self {
        self.spec.property_filters.push(PropertyFilter {
            property: property.into(),
            operator,
            value: value.into(),
        });
        self
    }
    pub fn breakdown(mut self, property: impl Into<String>) -> Self {
        self.spec.breakdown_property = Some(property.into());
        self
    }
    pub fn bucket_seconds(mut self, seconds: u64) -> Self {
        self.spec.bucket_seconds = Some(seconds);
        self
    }
    pub fn build(self) -> Result<Metric, String> {
        let metric = Metric {
            id: self.id,
            display_name: self.display_name,
            description: self.description,
            unit: self.unit,
            query: MetricQuery::ProductEvent(self.spec),
        };
        metric.eyes()?;
        Ok(metric)
    }
}
fn name(v: &str) -> Result<&str, String> {
    if v.trim().is_empty() || v.contains('.') {
        Err(format!("invalid event or property name {v:?}"))
    } else {
        Ok(v)
    }
}
impl ProductAggregate {
    pub(super) fn parts(&self) -> (AggregateFunction, &'static str, Option<&str>) {
        match self {
            Self::Count => (AggregateFunction::Count, "total", None),
            Self::DistinctUsers => (AggregateFunction::CountDistinct, "dau", None),
            Self::Sum { property } => (AggregateFunction::Sum, "sum", Some(property)),
            Self::Avg { property } => (AggregateFunction::Avg, "avg", Some(property)),
            Self::Min { property } => (AggregateFunction::Min, "min", Some(property)),
            Self::Max { property } => (AggregateFunction::Max, "max", Some(property)),
            Self::Median { property } => (AggregateFunction::P50, "median", Some(property)),
            Self::P90 { property } => (AggregateFunction::P90, "p90", Some(property)),
            Self::P95 { property } => (AggregateFunction::P95, "p95", Some(property)),
            Self::P99 { property } => (AggregateFunction::P99, "p99", Some(property)),
        }
    }
}

impl Metric {
    fn eyes(&self) -> Result<eyes::NamedMetric, String> {
        if !valid_id(&self.id) {
            return Err(format!("invalid metric id {:?}", self.id));
        }
        let mut m = match &self.query {
            MetricQuery::Telemetry(m) => m.0.clone(),
            MetricQuery::ProductEvent(p) => {
                let (function, _, property) = p.aggregate.parts();
                let field = property
                    .map(|p| name(p).map(|p| format!("fields.{p}")))
                    .transpose()?;
                let arg = if matches!(p.aggregate, ProductAggregate::DistinctUsers) {
                    Some("fields.user_id")
                } else {
                    field.as_deref()
                };
                let mut b = TelemetryMetricBuilder::new(&self.id, function, arg)?
                    .filter_eq("fields.event_type", name(&p.event)?)?;
                if let Some(path) = field {
                    b = b.filter_numeric(&path)?;
                }
                for f in &p.property_filters {
                    let path = format!("fields.{}", name(&f.property)?);
                    let v = f.value.clone();
                    b = match f.operator {
                        FilterOperator::Eq => b.filter_eq(&path, v)?,
                        FilterOperator::Gte => b.filter_gte(&path, v)?,
                    };
                }
                if let Some(p) = &p.breakdown_property {
                    b = b.group_by(&format!("fields.{}", name(p)?))?;
                }
                if let Some(n) = p.bucket_seconds {
                    b = b.time_bucket(n);
                }
                b.build_eyes()?
            }
        };
        m.id.clone_from(&self.id);
        m.display_name.clone_from(&self.display_name);
        m.description.clone_from(&self.description);
        m.unit.clone_from(&self.unit);
        Ok(m)
    }
}

impl Item {
    pub(super) fn reference(&self) -> Option<(&str, eyes::MetricResultShape)> {
        use eyes::MetricResultShape::{Scalar, Table, TimeSeries};
        match self {
            Self::Stat { metric_id, .. } => Some((metric_id, Scalar)),
            Self::TimeSeries { metric_id, .. } => Some((metric_id, TimeSeries)),
            Self::Table { metric_id, .. } => Some((metric_id, Table)),
            Self::HealthSummary { .. } | Self::Links { .. } => None,
        }
    }
    fn eyes(self) -> Result<eyes::DashboardItem, String> {
        use eyes::DashboardItem as E;
        Ok(match self {
            Self::Stat {
                metric_id,
                label,
                unit,
                window_seconds,
            } => E::Stat {
                query_id: metric_id,
                label,
                unit,
                window_seconds,
            },
            Self::Table {
                metric_id,
                label,
                unit,
                window_seconds,
            } => E::Table {
                query_id: metric_id,
                label,
                unit,
                window_seconds,
            },
            Self::TimeSeries {
                metric_id,
                label,
                unit,
                window_seconds,
                preferred_bucket_seconds,
            } => E::TimeSeries {
                query_id: metric_id,
                label,
                unit,
                window_seconds,
                preferred_bucket_seconds,
            },
            Self::HealthSummary { label } => E::HealthSummary { label },
            Self::Links { links } => E::Links {
                links: links
                    .into_iter()
                    .map(|l| eyes::DashboardLink::new(l.label, l.href))
                    .collect::<Result<_, _>>()?,
            },
        })
    }
}
impl Dashboard {
    fn eyes(&self) -> Result<eyes::NamedDashboard, String> {
        let mut d = eyes::NamedDashboard::new(&self.id, &self.title)?;
        d.description.clone_from(&self.description);
        d.default_range_seconds = self.default_range_seconds;
        for s in &self.sections {
            d.sections.push(eyes::DashboardSection {
                title: s.title.clone(),
                description: s.description.clone(),
                items: s
                    .items
                    .iter()
                    .cloned()
                    .map(Item::eyes)
                    .collect::<Result<_, _>>()?,
            });
        }
        Ok(d)
    }
}
impl Alert {
    fn eyes(&self) -> Result<eyes::MetricThreshold, String> {
        let mut b = eyes::MetricThresholdBuilder::new(&self.id, &self.metric_id)
            .window_seconds(self.window_seconds)
            .evaluation_interval_seconds(self.evaluation_interval_seconds)
            .evaluation_delay_seconds(self.evaluation_delay_seconds)
            .failure_threshold(self.failure_threshold)
            .recovery_threshold(self.recovery_threshold)
            .enabled(self.enabled)
            .group_match(match self.group_match {
                GroupMatch::Any => eyes::ThresholdGroupMatch::Any,
                GroupMatch::All => eyes::ThresholdGroupMatch::All,
            });
        if let Some(v) = &self.description {
            b = b.description(v);
        }
        let comparison = |c| match c {
            Comparison::Above => eyes::ThresholdComparison::Above,
            Comparison::Below => eyes::ThresholdComparison::Below,
        };
        if let Some(v) = &self.warning {
            b = b.warning(comparison(v.comparison), v.threshold);
        }
        if let Some(v) = &self.critical {
            b = b.critical(comparison(v.comparison), v.threshold);
        }
        b.build()
    }
}
/// One application's declared dashboards, metrics and alerts.
#[derive(Debug, Clone, Default, PartialEq)]
#[must_use]
pub struct Declarations {
    pub metrics: Vec<Metric>,
    pub dashboards: Vec<Dashboard>,
    pub alerts: Vec<Alert>,
}
impl Declarations {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn metric(mut self, m: Metric) -> Self {
        self.metrics.push(m);
        self
    }
    pub fn dashboard(mut self, d: Dashboard) -> Self {
        self.dashboards.push(d);
        self
    }
    pub fn alert(mut self, a: Alert) -> Self {
        self.alerts.push(a);
        self
    }
    /// Check declaration identities, references, result shapes and manifest limits.
    pub fn validate(&self) -> Result<(), String> {
        use std::collections::{HashMap, HashSet};
        if self.metrics.len() > 100 || self.dashboards.len() > 20 {
            return Err("manifest exceeds 100 metrics or 20 dashboards".into());
        }
        let mut shapes = HashMap::new();
        for m in &self.metrics {
            if shapes
                .insert(m.id.as_str(), m.eyes()?.result_shape)
                .is_some()
            {
                return Err(format!("duplicate metric {}", m.id));
            }
        }
        let mut ids = HashSet::new();
        for a in &self.alerts {
            if !valid_id(&a.id) || !ids.insert(&a.id) || !shapes.contains_key(a.metric_id.as_str())
            {
                return Err(format!(
                    "invalid or duplicate alert {} or missing metric {}",
                    a.id, a.metric_id
                ));
            }
            a.eyes()?;
        }
        ids.clear();
        for d in &self.dashboards {
            if !ids.insert(&d.id) || d.sections.len() > 20 {
                return Err(format!("duplicate or oversized dashboard {}", d.id));
            }
            for s in &d.sections {
                if s.items.len() > 24 {
                    return Err(format!("too many items on {}", d.id));
                }
                for item in &s.items {
                    if let Some((id, shape)) = item.reference()
                        && shapes.get(id) != Some(&shape)
                    {
                        return Err(format!(
                            "{}: missing metric or incompatible shape for {id}",
                            d.id
                        ));
                    }
                }
            }
            d.eyes()?;
        }
        Ok(())
    }
    /// Populate the three declaration lists while preserving other manifest fields.
    pub fn attach_to_eyes_manifest(
        self,
        manifest: eyes::AppManifest,
    ) -> Result<eyes::AppManifest, String> {
        self.validate()?;
        Ok(manifest
            .metrics(
                self.metrics
                    .iter()
                    .map(Metric::eyes)
                    .collect::<Result<_, _>>()?,
            )
            .metric_thresholds(
                self.alerts
                    .iter()
                    .map(Alert::eyes)
                    .collect::<Result<_, _>>()?,
            )
            .dashboards(
                self.dashboards
                    .iter()
                    .map(Dashboard::eyes)
                    .collect::<Result<_, _>>()?,
            ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn both_sinks_validate_the_same_shapes() {
        let mut metric =
            TelemetryMetricBuilder::new("old", AggregateFunction::P95, Some("fields.duration"))
                .unwrap()
                .filter_eq("fields.semantic_kind", "request")
                .unwrap()
                .time_bucket(300)
                .unit("ms")
                .build()
                .unwrap();
        metric.id = "latency".into();
        metric.display_name = Some("Latency".into());
        let dashboard = Dashboard::new("ops", "Ops")
            .unwrap()
            .default_range_seconds(300)
            .section(
                Section::new()
                    .description("Latency percentiles")
                    .item(Item::time_series("latency")),
            );
        let alert = AlertBuilder::new("slow", "latency")
            .window_seconds(300)
            .evaluation_interval_seconds(60)
            .critical(Comparison::Above, 100.)
            .build()
            .unwrap();
        let mut d = Declarations::new()
            .metric(metric)
            .dashboard(dashboard)
            .alert(alert);
        let manifest = d
            .clone()
            .attach_to_eyes_manifest(eyes::AppManifest::default())
            .unwrap();
        assert_eq!(manifest.metrics.as_ref().unwrap()[0].id, "latency");
        assert_eq!(
            manifest.metrics.as_ref().unwrap()[0]
                .display_name
                .as_deref(),
            Some("Latency")
        );
        assert_eq!(
            manifest.metrics.as_ref().unwrap()[0].unit.as_deref(),
            Some("ms")
        );
        assert_eq!(
            manifest.metrics.as_ref().unwrap()[0].result_shape,
            eyes::MetricResultShape::TimeSeries
        );
        assert_eq!(
            manifest.dashboards.as_ref().unwrap()[0].default_range_seconds,
            Some(300)
        );
        assert_eq!(
            manifest.dashboards.as_ref().unwrap()[0].sections[0]
                .description
                .as_deref(),
            Some("Latency percentiles")
        );
        assert_eq!(
            manifest.metric_thresholds.as_ref().unwrap()[0].metric_id,
            "latency"
        );
        d.dashboards[0].sections[0].items[0] = Item::stat("latency");
        assert!(d.validate().is_err());
    }
    #[test]
    fn product_names_caps_and_refs() {
        for bad in ["", "a.b"] {
            assert!(ProductMetricBuilder::count("m", bad).build().is_err());
            assert!(
                ProductMetricBuilder::sum("m", "event", bad)
                    .build()
                    .is_err()
            );
        }
        let mut d = Declarations::new();
        for i in 0..100 {
            d.metrics.push(
                ProductMetricBuilder::count(format!("m{i}"), "e")
                    .build()
                    .unwrap(),
            );
        }
        d.validate().unwrap();
        d.metrics
            .push(ProductMetricBuilder::count("extra", "e").build().unwrap());
        assert!(d.validate().is_err());
        d.metrics.pop();
        d.metrics[0].id = "m1".into();
        assert!(d.validate().is_err());
    }
}

#[cfg(test)]
mod product_tests {
    use super::*;
    #[test]
    fn product_queries_match_the_emission_fields() {
        let product = ProductMetricBuilder::sum("orders", "placed", "amount")
            .filter("tier", FilterOperator::Eq, "pro")
            .filter("units", FilterOperator::Gte, 3_i64)
            .breakdown("region")
            .build()
            .unwrap();
        let expected =
            eyes::NamedMetricBuilder::new("orders", AggregateFunction::Sum, Some("fields.amount"))
                .unwrap()
                .filter_eq("fields.event_type", "placed")
                .unwrap()
                .filter_gte("fields.amount", 0_i64)
                .unwrap()
                .filter_eq("fields.tier", "pro")
                .unwrap()
                .filter_gte("fields.units", 3_i64)
                .unwrap()
                .group_by("fields.region")
                .unwrap()
                .build()
                .unwrap();
        let mut expected_json = serde_json::to_value(expected).unwrap();
        expected_json["query"]["query"]["stages"][1]["predicate"]["right"]["value"] =
            serde_json::json!({"type":"float","value":f64::MIN});
        assert_eq!(
            serde_json::to_value(product.eyes().unwrap()).unwrap(),
            expected_json
        );
    }
    #[test]
    fn invalid_alert_policies() {
        let builder = || {
            AlertBuilder::new("alert", "count")
                .window_seconds(60)
                .evaluation_interval_seconds(10)
        };
        assert!(builder().build().is_err());
        assert!(
            builder()
                .critical(Comparison::Above, f64::NAN)
                .build()
                .is_err()
        );
        for (window, ok) in [(0, false), (1, true), (2_678_400, true), (2_678_401, false)] {
            assert_eq!(
                builder()
                    .window_seconds(window)
                    .warning(Comparison::Below, 3.)
                    .build()
                    .is_ok(),
                ok
            );
        }
    }
}
