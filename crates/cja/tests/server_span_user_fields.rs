//! `server.request` span user fields (`user_id`, `auth_type`) through the
//! stack cja serves.
//!
//! This is its own test binary so it can install one global subscriber, as
//! production does. A per-test scoped subscriber races other test threads
//! for the span callsite's cached interest and silently records nothing.

use std::sync::{Arc, Mutex, OnceLock};

use axum::{Router, body::Body, routing::get};
use cja::server::App;
use http::{Request, StatusCode};
use tower::ServiceExt as _;
use tracing::{Subscriber, field::Visit};
use tracing_subscriber::{Layer, layer::Context, prelude::*, registry::LookupSpan};

#[derive(Clone, Default)]
struct SpanFields(Arc<Mutex<SpanFieldState>>);

#[derive(Default)]
struct SpanFieldState {
    /// Declared field names per `server.request` span, in creation order.
    declared: Vec<Vec<String>>,
    /// (field, value) pairs visited at span creation. `tracing` skips
    /// `Empty` fields, so fields that are declared but never set at
    /// creation do not appear here.
    creation: Vec<(String, String)>,
    /// (field name, value) recorded on `server.request` spans after
    /// creation, in record order.
    recorded: Vec<(String, String)>,
}

fn span_fields() -> &'static SpanFields {
    static FIELDS: OnceLock<SpanFields> = OnceLock::new();
    FIELDS.get_or_init(|| {
        let fields = SpanFields::default();
        tracing::subscriber::set_global_default(
            tracing_subscriber::registry().with(fields.clone()),
        )
        .expect("install one test tracing subscriber");
        fields
    })
}

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for SpanFields {
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _id: &tracing::Id,
        _ctx: Context<'_, S>,
    ) {
        if attrs.metadata().name() != "server.request" {
            return;
        }
        let declared = attrs
            .metadata()
            .fields()
            .iter()
            .map(|field| field.name().to_owned())
            .collect();
        let mut creation = FieldValues(Vec::new());
        attrs.record(&mut creation);
        let mut state = self.0.lock().unwrap();
        state.declared.push(declared);
        state.creation.extend(creation.0);
    }

    fn on_record(&self, id: &tracing::Id, values: &tracing::span::Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        if span.metadata().name() != "server.request" {
            return;
        }
        let mut visit = FieldValues(Vec::new());
        values.record(&mut visit);
        self.0.lock().unwrap().recorded.extend(visit.0);
    }
}

struct FieldValues(Vec<(String, String)>);

impl Visit for FieldValues {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.push((field.name().to_owned(), value.to_owned()));
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.push((field.name().to_owned(), format!("{value:?}")));
    }
}

async fn send(app: App, host: &str, path: &str) -> StatusCode {
    let request = Request::get(path)
        .header("host", host)
        .body(Body::empty())
        .unwrap();
    let response = app.into_router().oneshot(request).await.unwrap();
    response.status()
}

#[tokio::test]
async fn user_fields_recorded_in_a_handler_land_on_the_server_request_span() {
    let fields = span_fields();
    let authed = Router::new().route(
        "/me",
        get(|| async {
            tracing::Span::current().record("user_id", "11111111-1111-1111-1111-111111111111");
            tracing::Span::current().record("auth_type", "session");
            "authed"
        }),
    );
    let anon = Router::new().route("/public", get(|| async { "anon" }));
    let app = App::new(authed).host(["anon.example.com"], anon);

    assert_eq!(send(app.clone(), "example.com", "/me").await, StatusCode::OK);
    assert_eq!(send(app, "anon.example.com", "/public").await, StatusCode::OK);

    let state = fields.0.lock().unwrap();

    // Both spans declare the user fields (the fix itself).
    assert_eq!(state.declared.len(), 2);
    for d in &state.declared {
        assert!(
            d.iter().any(|f| f == "user_id"),
            "user_id missing from declared fields: {d:?}"
        );
        assert!(
            d.iter().any(|f| f == "auth_type"),
            "auth_type missing from declared fields: {d:?}"
        );
    }

    // Declared-but-`Empty` fields emit nothing at span creation.
    assert!(
        !state
            .creation
            .iter()
            .any(|(f, _)| f == "user_id" || f == "auth_type"),
        "user fields recorded at creation: {:?}",
        state.creation
    );

    // The exact set of recorded values for each user field is the authed
    // handler's pair. Anything else — the anon request, or a regression
    // recording on every request — shows up here and fails.
    let user_ids: Vec<_> = state
        .recorded
        .iter()
        .filter(|(f, _)| f == "user_id")
        .map(|(_, v)| v.as_str())
        .collect();
    assert_eq!(user_ids, ["11111111-1111-1111-1111-111111111111"]);
    let auth_types: Vec<_> = state
        .recorded
        .iter()
        .filter(|(f, _)| f == "auth_type")
        .map(|(_, v)| v.as_str())
        .collect();
    assert_eq!(auth_types, ["session"]);

    // `Tracer::on_response` recording still works (`u16` visits via the
    // `record_u64` -> `record_debug` default, captured as the string), and
    // both requests were actually served: exactly one 200 each.
    let status_codes: Vec<_> = state
        .recorded
        .iter()
        .filter(|(f, _)| f == "http.response.status_code")
        .map(|(_, v)| v.as_str())
        .collect();
    assert_eq!(status_codes, ["200", "200"]);
}
