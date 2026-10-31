//! `server.request` span fields through the stack cja serves.
//!
//! This is its own test binary so it can install one global subscriber, as
//! production does. A per-test scoped subscriber races other test threads
//! for the span callsite's cached interest and silently records nothing.

use std::sync::{Arc, Mutex, OnceLock};

use axum::{Router, body::Body, routing::get};
use cja::server::App;
use http::Request;
use tower::ServiceExt as _;
use tracing::{Subscriber, field::Visit};
use tracing_subscriber::{Layer, layer::Context, prelude::*, registry::LookupSpan};

/// `http.route` as recorded when each `server.request` span is created.
#[derive(Clone, Default)]
struct SpanRoutes(Arc<Mutex<Vec<String>>>);

fn span_routes() -> &'static SpanRoutes {
    static ROUTES: OnceLock<SpanRoutes> = OnceLock::new();
    ROUTES.get_or_init(|| {
        let routes = SpanRoutes::default();
        tracing::subscriber::set_global_default(
            tracing_subscriber::registry().with(routes.clone()),
        )
        .expect("install one test tracing subscriber");
        routes
    })
}

struct RouteField(Option<String>);

impl Visit for RouteField {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "http.route" {
            self.0 = Some(value.to_owned());
        }
    }

    fn record_debug(&mut self, _field: &tracing::field::Field, _value: &dyn std::fmt::Debug) {}
}

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for SpanRoutes {
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _id: &tracing::Id,
        _ctx: Context<'_, S>,
    ) {
        if attrs.metadata().name() != "server.request" {
            return;
        }
        let mut route = RouteField(None);
        attrs.record(&mut route);
        self.0
            .lock()
            .unwrap()
            .push(route.0.expect("server.request spans record http.route"));
    }
}

async fn send(app: App, host: &str, path: &str) {
    let request = Request::get(path)
        .header("host", host)
        .body(Body::empty())
        .unwrap();
    app.into_router().oneshot(request).await.unwrap();
}

#[tokio::test]
async fn span_route_is_the_matched_template_on_every_host() {
    let routes = span_routes();
    let app = App::new(Router::new().route("/games/{id}", get(|| async { "site" }))).host(
        ["engine.example.com"],
        Router::new().route("/games/{id}/frames", get(|| async { "engine" })),
    );
    let plain = Router::new().route("/games/{id}", get(|| async { "plain" }));

    send(app.clone(), "example.com", "/games/1").await;
    send(app.clone(), "engine.example.com", "/games/1/frames").await;
    send(app, "engine.example.com", "/missing").await;
    send(plain.into(), "example.com", "/games/1").await;

    assert_eq!(
        *routes.0.lock().unwrap(),
        ["/games/{id}", "/games/{id}/frames", "", "/games/{id}"]
    );
}
