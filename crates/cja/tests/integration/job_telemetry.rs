use std::sync::{Arc, Mutex, OnceLock};

use cja::jobs::Job;
use serde_json::{Map, Value, json};
use tracing::{Subscriber, field::Visit};
use tracing_subscriber::{Layer, layer::Context, prelude::*, registry::LookupSpan};

#[derive(Debug, Clone)]
struct Record {
    span_id: String,
    is_span: bool,
    fields: Map<String, Value>,
}

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<Record>>>);

// Production has one subscriber for the process lifetime. Keep that same
// model in this integration-test binary instead of installing and dropping
// per-future subscribers while other tests use the same tracing callsites.
// Tests select their own records by the committed job/unique context and span.
fn capture() -> &'static Capture {
    static CAPTURE: OnceLock<Capture> = OnceLock::new();
    CAPTURE.get_or_init(|| {
        let capture = Capture::default();
        tracing::subscriber::set_global_default(
            tracing_subscriber::registry().with(capture.clone()),
        )
        .expect("integration tests install one tracing subscriber");
        capture
    })
}

#[derive(Default)]
struct Fields(Map<String, Value>);

impl Visit for Fields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().into(), json!(format!("{value:?}")));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().into(), json!(value));
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.0.insert(field.name().into(), json!(value));
    }
}

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for Capture {
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::Id,
        _ctx: Context<'_, S>,
    ) {
        if attrs.metadata().name() != "jobs.enqueue" {
            return;
        }
        let mut fields = Fields::default();
        attrs.record(&mut fields);
        self.0.lock().unwrap().push(Record {
            span_id: format!("{id:?}"),
            is_span: true,
            fields: fields.0,
        });
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.event_span(event) else {
            return;
        };
        if span.name() != "jobs.enqueue" {
            return;
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        self.0.lock().unwrap().push(Record {
            span_id: format!("{:?}", span.id()),
            is_span: false,
            fields: fields.0,
        });
    }
}

#[tokio::test]
async fn enqueue_identity_is_present_at_span_creation_and_matches_the_committed_job() {
    let (pool, _guard) = crate::common::db::setup_test_db().await.unwrap();
    let state = crate::common::app::TestAppState::new(pool.clone());
    let capture = capture();
    let job = super::jobs::TestJob {
        id: "game-123".into(),
        value: 42,
    };
    job.enqueue(state, "game game-123".into(), Some(-5))
        .await
        .unwrap();

    let stored: (uuid::Uuid, String, i32) =
        sqlx::query_as("SELECT job_id, context, priority FROM jobs")
            .fetch_one(&pool)
            .await
            .unwrap();
    let records = capture.0.lock().unwrap();
    let span = records
        .iter()
        .find(|record| {
            record.is_span && record.fields.get("job.id") == Some(&json!(stored.0.to_string()))
        })
        .unwrap();
    assert_eq!(span.fields["job.id"], stored.0.to_string());
    assert_eq!(span.fields["job.name"], "TestJob");
    assert_eq!(span.fields["job.context"], stored.1);
    assert_eq!(span.fields["job.priority"], stored.2);
    assert!(span.fields.contains_key("job.created_at"));
    assert!(span.fields.contains_key("job.run_at"));
    // The original context and self fields remain available to older readers.
    assert_eq!(span.fields["context"], "game game-123");
    assert!(span.fields["self"].as_str().unwrap().contains("game-123"));
    let receipts: Vec<_> = records
        .iter()
        .filter(|record| {
            !record.is_span
                && record.span_id == span.span_id
                && record.fields.get("event_type") == Some(&json!("job_enqueued"))
        })
        .collect();
    assert_eq!(
        receipts.len(),
        1,
        "missing receipt for committed job {}",
        stored.0
    );
    assert_eq!(receipts[0].fields["job_id"], stored.0.to_string());
    assert_eq!(receipts[0].span_id, span.span_id);
}

#[tokio::test]
async fn a_failed_enqueue_has_an_identity_and_error_but_no_commit_receipt() {
    let (pool, _guard) = crate::common::db::setup_test_db().await.unwrap();
    let state = crate::common::app::TestAppState::new(pool.clone());
    pool.close().await;
    let capture = capture();
    let job = super::jobs::TestJob {
        id: "game-456".into(),
        value: 42,
    };
    let context = format!("failed enqueue {}", uuid::Uuid::new_v4());
    assert!(job.enqueue(state, context.clone(), None).await.is_err());
    let records = capture.0.lock().unwrap();
    let span = records
        .iter()
        .find(|record| record.is_span && record.fields.get("job.context") == Some(&json!(context)))
        .unwrap();
    assert!(uuid::Uuid::parse_str(span.fields["job.id"].as_str().unwrap()).is_ok());
    assert!(!records.iter().any(|record| record.span_id == span.span_id
        && record.fields.get("event_type") == Some(&json!("job_enqueued"))));
    let failure = records
        .iter()
        .find(|record| record.span_id == span.span_id && record.fields.contains_key("error"))
        .unwrap();
    assert_eq!(failure.fields["job_id"], span.fields["job.id"]);
    assert_eq!(failure.span_id, span.span_id);
}
