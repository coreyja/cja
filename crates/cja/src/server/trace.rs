use std::cell::RefCell;
use std::fmt;
use std::task::{Context, Poll};

use tokio::task::futures::TaskLocalFuture;
use tower_http::trace::{MakeSpan, OnResponse};
use tower_layer::Layer;
use tower_service::Service;
use tracing::Level;

/// The identity an app recorded for one request, in the words of
/// [`record_request_user`]. Carried from auth code to the `finished
/// processing request` event via the `REQUEST_USER` task-local.
///
/// Public with private fields only because it names a type in the public
/// [`RequestUserService`] future; it cannot be constructed outside cja.
#[derive(Debug)]
pub struct RequestUserIdentity {
    user_id: Box<str>,
    auth_type: Box<str>,
}

/// Per-request identity slot. `RefCell` is sound here because no caller
/// code runs while a borrow is held: [`record_request_user`] formats the
/// identity before taking the borrow, and `on_response` only moves a
/// value out.
type Slot = RefCell<Option<RequestUserIdentity>>;

tokio::task_local! {
    /// Per-request identity slot, written by auth code via
    /// [`record_request_user`] and read by [`Tracer`]'s `on_response` when
    /// it emits `finished processing request`. Scoped by
    /// [`RequestUserLayer`]; unset outside a request, so reads fail rather
    /// than mis-attribute.
    static REQUEST_USER: Slot;
}

/// Record the authenticated user for the request this code is serving, so
/// cja's `finished processing request` event carries `user_id` and
/// `auth_type` for Eyes to count.
///
/// Works anywhere the request's own future is being polled on its own task,
/// before the response resolves: an auth extractor, a middleware layered
/// inside cja's trace layer, a plain handler, and helpers under
/// `#[instrument]`. Returns `false` — recording nothing — anywhere else:
/// inside `tokio::spawn` or `spawn_blocking` (task-locals do not cross
/// tasks), inside a middleware that runs the inner call under
/// `tokio::spawn` (say, `axum::middleware::from_fn` middleware doing
/// `tokio::spawn(next.run(req)).await`), code that runs synchronously in a
/// tower `Service::call` rather than in the future it returns, a layer
/// applied outside [`RequestUserLayer`], a response-body stream polled
/// after the handler returned (e.g. SSE), or a hand-rolled serve loop that
/// did not add [`RequestUserLayer`]. A `false` return is never attributed
/// to another request.
///
/// On a hand-rolled stack, a layer sitting between the trace layer and
/// [`RequestUserLayer`] that records after the inner response resolved
/// gets `true`, but the event has already fired, so that record is lost.
///
/// If called twice on one request, the last call wins. A record cannot be
/// withdrawn: middleware that records from a session cookie before an
/// extractor rejects that session still ships the `401` response carrying
/// that `user_id`. Record only after the user is verified.
#[must_use = "returns false when this task is outside the request scope, e.g. inside tokio::spawn during a live request; bind the result and check it — debug_assert! alone is compiled out of release builds"]
pub fn record_request_user(user_id: impl fmt::Display, auth_type: impl fmt::Display) -> bool {
    REQUEST_USER
        .try_with(|slot| {
            // Build the identity before borrowing: caller `Display` code
            // runs here, and a `Display` impl that calls
            // `record_request_user` again would panic on the `RefCell` if
            // this ran while the borrow was held. Staying inside the
            // closure also keeps the no-scope path allocation-free.
            let identity = RequestUserIdentity {
                user_id: user_id.to_string().into_boxed_str(),
                auth_type: auth_type.to_string().into_boxed_str(),
            };
            *slot.borrow_mut() = Some(identity);
        })
        .is_ok()
}

/// Tower layer that scopes the `REQUEST_USER` task-local around the
/// response future of the inner stack — including cja's trace layer, so
/// `on_response` reads the slot when it emits `finished processing
/// request`. Synchronous code in an inner service's `Service::call` runs
/// before that future is first polled, outside the scope.
///
/// cja's `run_server`, `run_server_until`, `serve_until` and
/// [`App`](super::App) install this automatically, outside the trace layer.
/// Apps that manage their own `axum::serve` loop must layer it themselves,
/// outside (applied after) the trace layer:
///
/// ```rust,ignore
/// let tracer = cja::server::trace::Tracer;
/// let app = routes
///     .layer(tower_http::trace::TraceLayer::new_for_http()
///         .make_span_with(tracer)
///         .on_response(tracer))
///     .layer(cja::server::trace::RequestUserLayer);
/// ```
///
/// If it ends up inside the trace layer, [`record_request_user`] still
/// returns `true` from handlers, but the event silently carries no user:
/// `on_response` runs outside the scope. Do not also wrap a router that
/// passes through cja's serving path in a second trace layer — that
/// double-counts `server.request` spans.
#[derive(Debug, Clone, Copy)]
pub struct RequestUserLayer;

impl<S> Layer<S> for RequestUserLayer
where
    S: Service<axum::extract::Request>,
{
    type Service = RequestUserService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RequestUserService(inner)
    }
}

/// The service [`RequestUserLayer`] produces. Public with a private field
/// only because it names a type in the public layer impl.
#[derive(Debug, Clone)]
pub struct RequestUserService<S>(S);

impl<S> Service<axum::extract::Request> for RequestUserService<S>
where
    S: Service<axum::extract::Request>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = TaskLocalFuture<Slot, S::Future>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }

    #[inline]
    fn call(&mut self, request: axum::extract::Request) -> Self::Future {
        // `scope` builds the future in place: no boxing, no heap
        // allocation, even for requests that never record a user.
        REQUEST_USER.scope(Slot::default(), self.0.call(request))
    }
}

/// `MakeSpan`/`OnResponse` implementation behind cja's `server.request`
/// spans, plus the per-request user slot that feeds the `finished
/// processing request` event.
///
/// [`run_server`](super::run_server) installs this automatically. Apps that
/// manage their own `axum::serve` loop (custom bind address, graceful
/// shutdown) can attach it themselves to keep the same span vocabulary —
/// with [`RequestUserLayer`] outside the trace layer so
/// [`record_request_user`] still reaches the event:
///
/// ```rust,ignore
/// let tracer = cja::server::trace::Tracer;
/// let trace_layer = tower_http::trace::TraceLayer::new_for_http()
///     .make_span_with(tracer)
///     .on_response(tracer);
/// let app = routes
///     .layer(trace_layer)
///     .layer(cja::server::trace::RequestUserLayer);
/// ```
///
/// Apps record the authenticated user with [`record_request_user`]. It
/// works while the request's own future is being polled on its own task,
/// before the response resolves: an auth extractor, a middleware layered
/// inside cja's trace layer, a plain handler, and helpers under
/// `#[instrument]`. It returns `false` — recording nothing — anywhere else:
/// inside `tokio::spawn` or `spawn_blocking` (task-locals do not cross
/// tasks), inside a middleware that runs the inner call under `tokio::spawn`
/// (say, `axum::middleware::from_fn` middleware doing
/// `tokio::spawn(next.run(req)).await`), code that runs synchronously in a
/// tower `Service::call` rather than in the future it returns, a layer
/// applied outside [`RequestUserLayer`], a response-body stream polled
/// after the handler returned (e.g. SSE), or a hand-rolled serve loop that
/// did not add [`RequestUserLayer`]. A `false` return is never attributed
/// to another request. On a hand-rolled stack, a layer between the trace
/// layer and [`RequestUserLayer`] that records after the inner response
/// resolved still gets `true`, but the event has already fired, so that
/// record is lost. If called twice on one request, the last call wins. A
/// record cannot be withdrawn: middleware that records from a session
/// cookie before an extractor rejects that session still ships the `401`
/// response carrying that `user_id`. Record only after the user is
/// verified.
///
/// The event's field names use underscores (`user_id`, not `user.id`)
/// because Eyes keys are atomic: a dotted name would need quoting as
/// `fields["user.id"]`. Requests that record nothing emit no values for
/// these fields.
#[derive(Debug, Clone, Copy)]
pub struct Tracer;

impl<Body> MakeSpan<Body> for Tracer {
    fn make_span(&mut self, request: &http::Request<Body>) -> tracing::Span {
        let route = http_route(request);
        let span_name = format!("{} {}", request.method(), route);

        tracing::span!(
            Level::INFO,
            "server.request",
            otel.name = span_name,
            kind = "server",
            uri = %request.uri(),
            url.path = %request.uri().path(),
            url.query = request.uri().query(),
            url.scheme = request.uri().scheme_str(),
            server.address = request.uri().host(),
            server.port = request.uri().port_u16(),
            http_version = ?request.version(),
            user_agent.original = request.headers().get("user-agent").and_then(|h| h.to_str().ok()),
            http.route = route,
            http.request.method = %request.method(),
            http.request.header.host = request.headers().get("host").and_then(|h| h.to_str().ok()),
            http.request.header.forwarded_for = request.headers().get("x-forwarded-for").and_then(|h| h.to_str().ok()),
            http.request.header.forwarded_proto = request.headers().get("x-forwarded-proto").and_then(|h| h.to_str().ok()),
            http.request.header.forwarded_ssl = request.headers().get("x-forwarded-ssl").and_then(|h| h.to_str().ok()),
            http.request.header.referer = request.headers().get("referer").and_then(|h| h.to_str().ok()),
            http.request.header.fly_forwarded_port = request.headers().get("fly-forwarded-port").and_then(|h| h.to_str().ok()),
            http.request.header.fly_region = request.headers().get("fly-region").and_then(|h| h.to_str().ok()),
            http.request.header.via = request.headers().get("via").and_then(|h| h.to_str().ok()),

            http.response.status_code = tracing::field::Empty,
            http.response.header.content_type = tracing::field::Empty,
        )
    }
}

impl<Body> OnResponse<Body> for Tracer {
    fn on_response(
        self,
        response: &http::Response<Body>,
        latency: std::time::Duration,
        span: &tracing::Span,
    ) {
        let status_code = response.status().as_u16();
        // Runs inside `RequestUserLayer`'s scope for stacks cja serves, so
        // the slot is readable here. A hand-rolled loop with the `Tracer`
        // but no `RequestUserLayer` gets the same event without the user
        // fields — take the value out first so the event always fires.
        let user = REQUEST_USER
            .try_with(|slot| slot.borrow_mut().take())
            .ok()
            .flatten();
        tracing::event!(
            Level::INFO,
            status = status_code,
            latency = format_args!("{} ms", latency.as_millis()),
            user_id = user.as_ref().map(|u| &*u.user_id),
            auth_type = user.as_ref().map(|u| &*u.auth_type),
            "finished processing request"
        );

        span.record("http.response.status_code", status_code);
        span.record(
            "http.response.header.content_type",
            response
                .headers()
                .get("content-type")
                .and_then(|h| h.to_str().ok()),
        );
    }
}

#[inline]
fn http_route<B>(req: &http::Request<B>) -> &str {
    req.extensions()
        .get::<axum::extract::MatchedPath>()
        .map_or_else(|| "", |mp| mp.as_str())
}
