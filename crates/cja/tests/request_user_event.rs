//! `finished processing request` event user fields (`user_id`, `auth_type`)
//! through the stack cja serves.
//!
//! This is its own test binary so it can install one global subscriber, as
//! production does. A per-test scoped subscriber races other test threads
//! for the event callsite's cached interest and silently records nothing.

use std::sync::{Arc, Mutex, OnceLock};

use axum::{Router, body::Body, extract::FromRequestParts, routing::get};
use cja::server::App;
use cja::server::trace::{RequestUserLayer, Tracer, record_request_user};
use http::{Request, StatusCode, Response};
use tower::ServiceExt as _;
use tracing::{Subscriber, field::Visit};
use tracing_subscriber::{Layer, layer::Context, prelude::*, registry::LookupSpan};

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<State>>);

#[derive(Default)]
struct State {
    /// `finished processing request` events, in emission order: the
    /// (field, value) pairs actually recorded.
    events: Vec<Vec<(String, String)>>,
    /// Declared field names per `server.request` span, in creation order.
    span_declarations: Vec<Vec<String>>,
    /// (field, value) pairs recorded on `server.request` spans after
    /// creation, in record order.
    span_records: Vec<(String, String)>,
}

/// Serializes the per-scenario tests so `clear()` on the process-global
/// capture is safe.
static SERIALIZE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn capture() -> &'static Capture {
    static CAPTURE: OnceLock<Capture> = OnceLock::new();
    CAPTURE.get_or_init(|| {
        let capture = Capture::default();
        tracing::subscriber::set_global_default(
            tracing_subscriber::registry().with(capture.clone()),
        )
        .expect("install one test tracing subscriber");
        capture
    })
}

impl Capture {
    /// A handler-side assertion panic must not poison the capture for the
    /// remaining scenarios.
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn clear(&self) {
        let mut state = self.lock();
        state.events.clear();
        state.span_declarations.clear();
        state.span_records.clear();
    }
}

/// The value of `name` on one captured event, if recorded.
fn field<'a>(event: &'a [(String, String)], name: &str) -> Option<&'a str> {
    event
        .iter()
        .find(|(f, _)| f == name)
        .map(|(_, v)| v.as_str())
}

#[derive(Default)]
struct EventFields {
    message: Option<String>,
    fields: Vec<(String, String)>,
}

impl Visit for EventFields {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_owned());
        } else {
            self.fields
                .push((field.name().to_owned(), value.to_owned()));
        }
    }

    // `status = status_code` (u16) visits here. Implemented explicitly: the
    // trait default forwards to `record_debug`, and silently dropping the
    // explicit impl would lose numeric fields if that default ever changed.
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.fields
            .push((field.name().to_owned(), value.to_string()));
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = Some(format!("{value:?}"));
        } else {
            self.fields
                .push((field.name().to_owned(), format!("{value:?}")));
        }
    }
}

struct SpanRecords(Vec<(String, String)>);

impl Visit for SpanRecords {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.push((field.name().to_owned(), value.to_owned()));
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.0.push((field.name().to_owned(), value.to_string()));
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.push((field.name().to_owned(), format!("{value:?}")));
    }
}

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for Capture {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut visit = EventFields::default();
        event.record(&mut visit);
        // The message literal records via `record_str`; trimming the Debug
        // quoting is belt-and-braces in case it is routed through
        // `record_debug`.
        let is_target = visit
            .message
            .as_deref()
            .is_some_and(|m| m.trim_matches('"') == "finished processing request");
        if is_target {
            self.lock().events.push(visit.fields);
        }
    }

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
        self.lock().span_declarations.push(declared);
    }

    fn on_record(&self, id: &tracing::Id, values: &tracing::span::Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        if span.metadata().name() != "server.request" {
            return;
        }
        let mut visit = SpanRecords(Vec::new());
        values.record(&mut visit);
        self.lock().span_records.extend(visit.0);
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

/// Sends to a hand-rolled stack: a plain `Router` with the `Tracer` trace
/// layer (and optionally [`RequestUserLayer`]) instead of an [`App`].
async fn send_router(router: Router, path: &str) -> StatusCode {
    let request = Request::get(path)
        .header("host", "example.com")
        .body(Body::empty())
        .unwrap();
    router.oneshot(request).await.unwrap().status()
}

/// Applies the hand-rolled trace layer (the `Tracer` exactly as
/// [`Tracer`]'s docs describe it) to a plain `Router`.
fn with_trace_layer(routes: Router) -> Router {
    let tracer = Tracer;
    routes.layer(
        tower_http::trace::TraceLayer::new_for_http()
            .make_span_with(tracer)
            .on_response(tracer),
    )
}

#[tokio::test]
async fn handler_record_lands_on_the_event() {
    let _guard = SERIALIZE.lock().await;
    let capture = capture();
    capture.clear();

    let app = App::new(Router::new().route(
        "/me",
        get(|| async {
            assert!(record_request_user(
                "11111111-1111-1111-1111-111111111111",
                "session"
            ));
            "authed"
        }),
    ));

    assert_eq!(send(app, "example.com", "/me").await, StatusCode::OK);

    let state = capture.lock();
    assert_eq!(state.events.len(), 1);
    let event = &state.events[0];
    assert_eq!(
        field(event, "user_id"),
        Some("11111111-1111-1111-1111-111111111111")
    );
    assert_eq!(field(event, "auth_type"), Some("session"));
    assert_eq!(field(event, "status"), Some("200"));
    assert!(
        field(event, "latency").is_some_and(|v| v.ends_with(" ms")),
        "latency missing or malformed: {event:?}"
    );
}

#[tokio::test]
async fn extractor_record_lands_on_the_event() {
    let _guard = SERIALIZE.lock().await;
    let capture = capture();
    capture.clear();

    struct Authed;

    impl<S: Send + Sync> FromRequestParts<S> for Authed {
        type Rejection = std::convert::Infallible;

        async fn from_request_parts(
            _parts: &mut http::request::Parts,
            _state: &S,
        ) -> Result<Self, Self::Rejection> {
            // Suspends mid-extraction: the record has to survive await
            // points between extraction and use.
            tokio::task::yield_now().await;
            assert!(record_request_user(
                "22222222-2222-2222-2222-222222222222",
                "token"
            ));
            Ok(Authed)
        }
    }

    let app = App::new(Router::new().route("/me", get(|_: Authed| async { "authed" })));

    assert_eq!(send(app, "example.com", "/me").await, StatusCode::OK);

    let state = capture.lock();
    assert_eq!(state.events.len(), 1);
    let event = &state.events[0];
    assert_eq!(
        field(event, "user_id"),
        Some("22222222-2222-2222-2222-222222222222")
    );
    assert_eq!(field(event, "auth_type"), Some("token"));
}

#[tokio::test]
async fn middleware_inside_the_trace_layer_records() {
    let _guard = SERIALIZE.lock().await;
    let capture = capture();
    capture.clear();

    // Layered onto the router before `App::new`, so cja's trace layer — and
    // the request-user scope it sits inside — wraps this middleware.
    let routes = Router::new()
        .route("/mw", get(|| async { "via middleware" }))
        .layer(axum::middleware::from_fn(
            |req: axum::extract::Request, next: axum::middleware::Next| async move {
                assert!(record_request_user(
                    "33333333-3333-3333-3333-333333333333",
                    "session"
                ));
                next.run(req).await
            },
        ));

    let app = App::new(routes);

    assert_eq!(send(app, "example.com", "/mw").await, StatusCode::OK);

    let state = capture.lock();
    assert_eq!(state.events.len(), 1);
    let event = &state.events[0];
    assert_eq!(
        field(event, "user_id"),
        Some("33333333-3333-3333-3333-333333333333")
    );
    assert_eq!(field(event, "auth_type"), Some("session"));
}

#[tokio::test]
async fn anonymous_request_has_no_user_fields() {
    let _guard = SERIALIZE.lock().await;
    let capture = capture();
    capture.clear();

    let app = App::new(Router::new().route("/public", get(|| async { "anon" })));

    assert_eq!(send(app, "example.com", "/public").await, StatusCode::OK);

    let state = capture.lock();
    assert_eq!(state.events.len(), 1);
    let event = &state.events[0];
    assert!(
        event
            .iter()
            .all(|(f, _)| f != "user_id" && f != "auth_type"),
        "anonymous event carries user fields: {event:?}"
    );
    assert_eq!(field(event, "status"), Some("200"));
    assert!(field(event, "latency").is_some());
}

#[tokio::test]
async fn span_no_longer_declares_user_fields() {
    let _guard = SERIALIZE.lock().await;
    let capture = capture();
    capture.clear();

    let app = App::new(Router::new().route(
        "/me",
        get(|| async {
            assert!(record_request_user(
                "55555555-5555-5555-5555-555555555555",
                "session"
            ));
            "authed"
        }),
    ));

    assert_eq!(send(app, "example.com", "/me").await, StatusCode::OK);

    let state = capture.lock();
    let last_declaration = state
        .span_declarations
        .last()
        .expect("a server.request span was created");
    assert!(
        last_declaration
            .iter()
            .all(|f| f != "user_id" && f != "auth_type"),
        "server.request still declares user fields: {last_declaration:?}"
    );
    assert!(
        state
            .span_records
            .iter()
            .all(|(f, _)| f != "user_id" && f != "auth_type"),
        "user fields recorded on the span: {:?}",
        state.span_records
    );
    assert!(
        state
            .span_records
            .iter()
            .any(|(f, v)| f == "http.response.status_code" && v == "200"),
        "http.response.status_code no longer recorded on the span: {:?}",
        state.span_records
    );
}

#[tokio::test]
async fn concurrent_requests_stay_isolated() {
    let _guard = SERIALIZE.lock().await;
    let capture = capture();
    capture.clear();

    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let user_a = "aaaaaaaa-1111-1111-1111-111111111111";
    let user_b = "bbbbbbbb-2222-2222-2222-222222222222";

    let routes = Router::new()
        .route(
            "/a",
            get({
                let barrier = Arc::clone(&barrier);
                move || {
                    let barrier = Arc::clone(&barrier);
                    async move {
                        assert!(record_request_user(user_a, "session"));
                        barrier.wait().await;
                        "a"
                    }
                }
            }),
        )
        .route(
            "/b",
            get({
                let barrier = Arc::clone(&barrier);
                move || {
                    let barrier = Arc::clone(&barrier);
                    async move {
                        barrier.wait().await;
                        assert!(record_request_user(user_b, "token"));
                        "b"
                    }
                }
            }),
        );

    let app = App::new(routes);
    let (status_a, status_b) = tokio::join!(
        send(app.clone(), "example.com", "/a"),
        send(app, "example.com", "/b"),
    );
    assert_eq!(status_a, StatusCode::OK);
    assert_eq!(status_b, StatusCode::OK);

    let state = capture.lock();
    assert_eq!(state.events.len(), 2);
    let mut saw_a = false;
    let mut saw_b = false;
    for event in &state.events {
        match field(event, "user_id") {
            Some(uid) if uid == user_a => {
                saw_a = true;
                assert_eq!(field(event, "auth_type"), Some("session"));
            }
            Some(uid) if uid == user_b => {
                saw_b = true;
                assert_eq!(field(event, "auth_type"), Some("token"));
            }
            other => panic!("unexpected user on an event: {other:?} in {event:?}"),
        }
    }
    assert!(
        saw_a && saw_b,
        "a request lost its user: {:?}",
        state.events
    );
}

#[tokio::test]
async fn spawned_task_cannot_record() {
    let _guard = SERIALIZE.lock().await;
    let capture = capture();
    capture.clear();

    let app = App::new(Router::new().route(
        "/spawn",
        get(|| async {
            let spawned = tokio::spawn(async {
                record_request_user("44444444-4444-4444-4444-444444444444", "token")
            });
            let recorded = spawned.await.unwrap();
            assert!(!recorded, "spawned task claimed to record a user");
            assert!(record_request_user(
                "55555555-5555-5555-5555-555555555555",
                "session"
            ));
            "done"
        }),
    ));

    assert_eq!(send(app, "example.com", "/spawn").await, StatusCode::OK);

    let state = capture.lock();
    assert_eq!(state.events.len(), 1);
    let event = &state.events[0];
    assert_eq!(
        field(event, "user_id"),
        Some("55555555-5555-5555-5555-555555555555")
    );
    assert_eq!(field(event, "auth_type"), Some("session"));
}

#[tokio::test]
async fn nested_router_inner_user_stays_inner() {
    let _guard = SERIALIZE.lock().await;
    let capture = capture();
    capture.clear();

    let inner = App::new(Router::new().route(
        "/inner",
        get(|| async {
            assert!(record_request_user(
                "66666666-6666-6666-6666-666666666666",
                "token"
            ));
            "inner"
        }),
    ));

    async fn call_inner(
        axum::extract::State(inner): axum::extract::State<App>,
    ) -> axum::response::Response {
        let request = Request::get("/inner").body(Body::empty()).unwrap();
        inner.into_router().oneshot(request).await.unwrap()
    }

    let routes = Router::new()
        .route("/outer", get(call_inner))
        .with_state(inner);
    let app = App::new(routes);

    assert_eq!(send(app, "example.com", "/outer").await, StatusCode::OK);

    // Assert by content, not emission order.
    let state = capture.lock();
    assert_eq!(state.events.len(), 2);
    let inner_events = state
        .events
        .iter()
        .filter(|ev| {
            field(ev, "user_id") == Some("66666666-6666-6666-6666-666666666666")
                && field(ev, "auth_type") == Some("token")
        })
        .count();
    assert_eq!(inner_events, 1, "events: {:?}", state.events);
    let anonymous_events = state
        .events
        .iter()
        .filter(|ev| ev.iter().all(|(f, _)| f != "user_id" && f != "auth_type"))
        .count();
    assert_eq!(
        anonymous_events, 1,
        "the outer event is not anonymous: {:?}",
        state.events
    );
}

#[tokio::test]
async fn host_router_record_lands_on_its_event() {
    let _guard = SERIALIZE.lock().await;
    let capture = capture();
    capture.clear();

    let anon = Router::new().route("/public", get(|| async { "anon" }));
    let engine = Router::new().route(
        "/engine",
        get(|| async {
            assert!(record_request_user(
                "77777777-7777-7777-7777-777777777777",
                "token"
            ));
            "engine"
        }),
    );

    let app = App::new(anon).host(["engine.example.com"], engine);

    assert_eq!(
        send(app, "engine.example.com", "/engine").await,
        StatusCode::OK
    );

    let state = capture.lock();
    assert_eq!(state.events.len(), 1);
    let event = &state.events[0];
    assert_eq!(
        field(event, "user_id"),
        Some("77777777-7777-7777-7777-777777777777")
    );
    assert_eq!(field(event, "auth_type"), Some("token"));
}

#[tokio::test]
async fn layer_outside_the_stack_gets_false() {
    let _guard = SERIALIZE.lock().await;
    let capture = capture();
    capture.clear();

    let app = App::new(Router::new().route("/outside", get(|| async { "anon" })));
    // Layering onto `into_router()` puts this middleware outside cja's
    // whole stack, including the request-user scope.
    let router = app.into_router().layer(axum::middleware::from_fn(
        |req: axum::extract::Request, next: axum::middleware::Next| async move {
            assert!(
                !record_request_user("88888888-8888-8888-8888-888888888888", "token"),
                "a layer outside the stack recorded a user"
            );
            next.run(req).await
        },
    ));

    let request = Request::get("/outside")
        .header("host", "example.com")
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let state = capture.lock();
    assert_eq!(state.events.len(), 1);
    let event = &state.events[0];
    assert!(
        event
            .iter()
            .all(|(f, _)| f != "user_id" && f != "auth_type"),
        "event is not anonymous: {event:?}"
    );
}

#[tokio::test]
async fn record_without_a_request_returns_false() {
    let _guard = SERIALIZE.lock().await;
    let capture = capture();
    capture.clear();

    // Nothing is in flight: the guard is held and no request was sent.
    assert!(!record_request_user("no-request", "session"));

    let state = capture.lock();
    assert_eq!(state.events.len(), 0);
}

/// A `Display` impl that re-enters `record_request_user` while its
/// `record_request_user` call is formatting must not panic on the slot's
/// `RefCell`, and the outer call's values must win over the re-entrant
/// inner call's.
#[tokio::test]
async fn reentrant_display_does_not_panic() {
    let _guard = SERIALIZE.lock().await;
    let capture = capture();
    capture.clear();

    struct ReentrantUserId;

    impl std::fmt::Display for ReentrantUserId {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            // Runs while the outer `record_request_user` call is
            // formatting: formatting caller `Display` code while the slot
            // is mutably borrowed would panic here on the `RefCell`.
            assert!(record_request_user(
                "eeeeeeee-eeee-eeee-eeee-eeeeeeeeeeee",
                "token"
            ));
            f.write_str("cccccccc-cccc-cccc-cccc-cccccccccccc")
        }
    }

    let app = App::new(Router::new().route(
        "/reentrant",
        get(|| async {
            assert!(record_request_user(ReentrantUserId, "session"));
            "authed"
        }),
    ));

    assert_eq!(send(app, "example.com", "/reentrant").await, StatusCode::OK);

    let state = capture.lock();
    assert_eq!(state.events.len(), 1);
    let event = &state.events[0];
    assert_eq!(
        field(event, "user_id"),
        Some("cccccccc-cccc-cccc-cccc-cccccccccccc")
    );
    assert_eq!(field(event, "auth_type"), Some("session"));
}

#[tokio::test]
async fn last_write_wins() {
    let _guard = SERIALIZE.lock().await;
    let capture = capture();
    capture.clear();

    // The app's middleware (inside cja's trace layer) records first; the
    // handler records after it. The handler's pair must win.
    let routes = Router::new()
        .route(
            "/both",
            get(|| async {
                assert!(record_request_user(
                    "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
                    "token"
                ));
                "handler wins"
            }),
        )
        .layer(axum::middleware::from_fn(
            |req: axum::extract::Request, next: axum::middleware::Next| async move {
                assert!(record_request_user(
                    "99999999-9999-9999-9999-999999999999",
                    "session"
                ));
                next.run(req).await
            },
        ));

    let app = App::new(routes);

    assert_eq!(send(app, "example.com", "/both").await, StatusCode::OK);

    let state = capture.lock();
    assert_eq!(state.events.len(), 1);
    let event = &state.events[0];
    assert_eq!(
        field(event, "user_id"),
        Some("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa")
    );
    assert_eq!(field(event, "auth_type"), Some("token"));
}

/// The reverse of [`nested_router_inner_user_stays_inner`]: the outer
/// handler records a user, then calls an anonymous inner `App`. The inner
/// event must stay anonymous and the outer event must keep the user. The
/// events are told apart by `status` — the inner handler returns 202, the
/// outer returns its own 200 — because a shared slot would only swap
/// which event carries the user.
#[tokio::test]
async fn nested_router_outer_user_survives_anonymous_inner() {
    let _guard = SERIALIZE.lock().await;
    let capture = capture();
    capture.clear();

    let inner = App::new(Router::new().route(
        "/inner",
        get(|| async { StatusCode::ACCEPTED }),
    ));

    async fn call_inner(
        axum::extract::State(inner): axum::extract::State<App>,
    ) -> (StatusCode, &'static str) {
        assert!(record_request_user(
            "15151515-1515-1515-1515-151515151515",
            "session"
        ));
        let request = Request::get("/inner").body(Body::empty()).unwrap();
        let inner_status = inner
            .into_router()
            .oneshot(request)
            .await
            .unwrap()
            .status();
        assert_eq!(inner_status, StatusCode::ACCEPTED);
        // The outer response is its own 200, not the inner 202.
        (StatusCode::OK, "outer")
    }

    let routes = Router::new()
        .route("/outer", get(call_inner))
        .with_state(inner);
    let app = App::new(routes);

    assert_eq!(send(app, "example.com", "/outer").await, StatusCode::OK);

    let state = capture.lock();
    assert_eq!(state.events.len(), 2, "events: {:?}", state.events);
    let inner_event = state
        .events
        .iter()
        .find(|ev| field(ev, "status") == Some("202"))
        .expect("the inner event (202) fired");
    let outer_event = state
        .events
        .iter()
        .find(|ev| field(ev, "status") == Some("200"))
        .expect("the outer event (200) fired");
    assert!(
        inner_event.iter().all(|(f, _)| f != "user_id" && f != "auth_type"),
        "the inner anonymous event carries a user: {inner_event:?}"
    );
    assert_eq!(
        field(outer_event, "user_id"),
        Some("15151515-1515-1515-1515-151515151515")
    );
    assert_eq!(field(outer_event, "auth_type"), Some("session"));
}

/// A hand-rolled stack with the trace layer but no [`RequestUserLayer`]:
/// the handler's record returns `false`, the event still fires with
/// `status` and `latency`, and neither user key is present.
#[tokio::test]
async fn hand_rolled_without_request_user_layer_fires_anonymous_event() {
    let _guard = SERIALIZE.lock().await;
    let capture = capture();
    capture.clear();

    let router = with_trace_layer(Router::new().route(
        "/me",
        get(|| async {
            assert!(
                !record_request_user(
                    "12121212-1212-1212-1212-121212121212",
                    "session"
                ),
                "a record with no RequestUserLayer claimed to succeed"
            );
            "authed"
        }),
    ));

    assert_eq!(send_router(router, "/me").await, StatusCode::OK);

    let state = capture.lock();
    assert_eq!(state.events.len(), 1);
    let event = &state.events[0];
    assert!(
        event.iter().all(|(f, _)| f != "user_id" && f != "auth_type"),
        "event is not anonymous: {event:?}"
    );
    assert_eq!(field(event, "status"), Some("200"));
    assert!(field(event, "latency").is_some());
}

/// The documented hand-rolled layering — trace layer first, then
/// [`RequestUserLayer`] outermost — lands the user on the event.
#[tokio::test]
async fn hand_rolled_documented_layering_lands_the_user() {
    let _guard = SERIALIZE.lock().await;
    let capture = capture();
    capture.clear();

    let router = with_trace_layer(Router::new().route(
        "/me",
        get(|| async {
            assert!(record_request_user(
                "13131313-1313-1313-1313-131313131313",
                "session"
            ));
            "authed"
        }),
    ))
    .layer(RequestUserLayer);

    assert_eq!(send_router(router, "/me").await, StatusCode::OK);

    let state = capture.lock();
    assert_eq!(state.events.len(), 1);
    let event = &state.events[0];
    assert_eq!(
        field(event, "user_id"),
        Some("13131313-1313-1313-1313-131313131313")
    );
    assert_eq!(field(event, "auth_type"), Some("session"));
}

/// A record from a response-body stream, polled after the handler
/// returned: the service future — and the request-user scope with it — has
/// already resolved, so the record returns `false` and the event stays
/// anonymous.
#[tokio::test]
async fn body_stream_record_after_handler_returns_false() {
    let _guard = SERIALIZE.lock().await;
    let capture = capture();
    capture.clear();

    let app = App::new(Router::new().route(
        "/stream",
        get(|| async {
            let stream = futures::stream::once(async {
                let recorded = record_request_user(
                    "16161616-1616-1616-1616-161616161616",
                    "token",
                );
                Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(
                    if recorded { "true" } else { "false" },
                ))
            });
            Response::builder()
                .status(StatusCode::OK)
                .body(Body::from_stream(stream))
                .unwrap()
        }),
    ));

    let request = Request::get("/stream")
        .header("host", "example.com")
        .body(Body::empty())
        .unwrap();
    let response = app.into_router().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // Polling the body runs after the handler — and the request's service
    // future, which carried the scope — resolved.
    let body = axum::body::to_bytes(response.into_body(), 64)
        .await
        .unwrap();
    assert_eq!(body, "false");

    let state = capture.lock();
    assert_eq!(state.events.len(), 1);
    let event = &state.events[0];
    assert!(
        event.iter().all(|(f, _)| f != "user_id" && f != "auth_type"),
        "event is not anonymous: {event:?}"
    );
}

/// `spawn_blocking` moves to the blocking pool: a record made there does
/// not reach the request, while the handler's own record does.
#[tokio::test]
async fn spawn_blocking_cannot_record() {
    let _guard = SERIALIZE.lock().await;
    let capture = capture();
    capture.clear();

    let app = App::new(Router::new().route(
        "/blocking",
        get(|| async {
            let recorded = tokio::task::spawn_blocking(|| {
                record_request_user("1b1b1b1b-1b1b-1b1b-1b1b-1b1b1b1b1b1b", "token")
            })
            .await
            .unwrap();
            assert!(!recorded, "spawn_blocking claimed to record a user");
            assert!(record_request_user(
                "17171717-1717-1717-1717-171717171717",
                "session"
            ));
            "done"
        }),
    ));

    assert_eq!(send(app, "example.com", "/blocking").await, StatusCode::OK);

    let state = capture.lock();
    assert_eq!(state.events.len(), 1);
    let event = &state.events[0];
    assert_eq!(
        field(event, "user_id"),
        Some("17171717-1717-1717-1717-171717171717")
    );
    assert_eq!(field(event, "auth_type"), Some("session"));
}

/// A helper under `#[tracing::instrument]` runs on the request's task and
/// poll chain, so its record lands on the request's event.
#[tracing::instrument]
async fn authed_helper() -> bool {
    tokio::task::yield_now().await;
    record_request_user("18181818-1818-1818-1818-181818181818", "token")
}

#[tokio::test]
async fn instrumented_helper_records_for_the_request() {
    let _guard = SERIALIZE.lock().await;
    let capture = capture();
    capture.clear();

    let app = App::new(Router::new().route(
        "/helper",
        get(|| async {
            assert!(authed_helper().await);
            "authed"
        }),
    ));

    assert_eq!(send(app, "example.com", "/helper").await, StatusCode::OK);

    let state = capture.lock();
    assert_eq!(state.events.len(), 1);
    let event = &state.events[0];
    assert_eq!(
        field(event, "user_id"),
        Some("18181818-1818-1818-1818-181818181818")
    );
    assert_eq!(field(event, "auth_type"), Some("token"));
}

/// [`RequestUserLayer`] inside the trace layer — the documented trap: the
/// handler's record returns `true` (the scope wraps the routes), but
/// `on_response` runs outside the scope, so the event has neither user
/// key.
#[tokio::test]
async fn hand_rolled_request_user_layer_inside_trace_is_silent() {
    let _guard = SERIALIZE.lock().await;
    let capture = capture();
    capture.clear();

    let router = with_trace_layer(
        Router::new().route(
            "/me",
            get(|| async {
                assert!(record_request_user(
                    "14141414-1414-1414-1414-141414141414",
                    "session"
                ));
                "authed"
            }),
        )
        .layer(RequestUserLayer),
    );

    assert_eq!(send_router(router, "/me").await, StatusCode::OK);

    let state = capture.lock();
    assert_eq!(state.events.len(), 1);
    let event = &state.events[0];
    assert!(
        event.iter().all(|(f, _)| f != "user_id" && f != "auth_type"),
        "event is not anonymous: {event:?}"
    );
}
