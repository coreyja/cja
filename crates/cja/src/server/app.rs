use std::{
    convert::Infallible,
    sync::Arc,
    task::{Context, Poll},
};

use axum::{Router, extract::Request, response::Response, routing::future::RouteFuture};
use http::{header::HOST, uri::Authority};
use tower_service::Service;

/// What cja serves: a default router, plus routers for specific hosts.
///
/// Requests are dispatched by host *before* any route matching, and cja's
/// tracing and cookie middleware wrap each router separately. That keeps
/// `http.route` on `server.request` spans equal to the matched route
/// template on every host.
///
/// A plain [`Router`] converts into an `App` with no host routers, which
/// serves exactly like the router on its own.
///
/// ```rust,ignore
/// let app = cja::server::App::new(site_routes)
///     .host(["engine.example.com"], engine_routes);
/// cja::server::run_server_until(app, shutdown).await?;
/// ```
#[derive(Debug, Clone)]
pub struct App {
    default: Router,
    hosts: Vec<HostRouter>,
}

#[derive(Debug, Clone)]
struct HostRouter {
    hosts: Vec<String>,
    router: Router,
}

impl App {
    /// An app that sends every request to `default`.
    pub fn new(default: Router) -> Self {
        Self {
            default,
            hosts: Vec::new(),
        }
    }

    /// Send requests for any of `hosts` to `router` instead of the default router.
    ///
    /// `hosts` are host names without a port. A request's host comes from its
    /// `Host` header, or from the request URI's authority when there is no
    /// `Host` header (HTTP/2 `:authority`). Matching ignores ASCII case, the
    /// port and a trailing dot. When more than one host router lists a host,
    /// the first one added wins. Requests with no host, or an unparseable
    /// one, go to the default router.
    #[must_use]
    pub fn host<I, H>(mut self, hosts: I, router: Router) -> Self
    where
        I: IntoIterator<Item = H>,
        H: Into<String>,
    {
        let hosts = hosts
            .into_iter()
            .map(|host| {
                let mut host = host.into();
                if host.ends_with('.') {
                    host.pop();
                }
                host
            })
            .collect();
        self.hosts.push(HostRouter { hosts, router });
        self
    }

    /// The router cja serves for this app, with cja's tracing and cookie
    /// middleware applied to each of its routers.
    ///
    /// Use it to drive the production middleware stack in tests.
    pub fn into_router(self) -> Router {
        let default = super::prepare_routes(self.default);
        if self.hosts.is_empty() {
            return default;
        }

        let hosts = self
            .hosts
            .into_iter()
            .map(|host_router| HostRouter {
                hosts: host_router.hosts,
                router: super::prepare_routes(host_router.router),
            })
            .collect();
        Router::new().fallback_service(HostDispatch { default, hosts })
    }
}

impl From<Router> for App {
    fn from(router: Router) -> Self {
        Self::new(router)
    }
}

#[derive(Clone)]
struct HostDispatch {
    default: Router,
    hosts: Arc<[HostRouter]>,
}

impl HostDispatch {
    fn router_for(&self, request: &Request) -> &Router {
        let authority = match request.headers().get(HOST) {
            Some(host) => Authority::try_from(host.as_bytes()).ok(),
            None => request.uri().authority().cloned(),
        };
        let Some(authority) = authority else {
            return &self.default;
        };
        let host = authority.host();
        let host = host.strip_suffix('.').unwrap_or(host);

        self.hosts
            .iter()
            .find(|host_router| {
                host_router
                    .hosts
                    .iter()
                    .any(|candidate| candidate.eq_ignore_ascii_case(host))
            })
            .map_or(&self.default, |host_router| &host_router.router)
    }
}

impl Service<Request> for HostDispatch {
    type Response = Response;
    type Error = Infallible;
    type Future = RouteFuture<Infallible>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: Request) -> Self::Future {
        // `Router` is always ready, so calling a clone directly is sound.
        self.router_for(&request).clone().call(request)
    }
}

#[cfg(test)]
mod tests {
    use axum::{body::Body, routing::get};
    use http::{Request, StatusCode};
    use tower::ServiceExt as _;
    use tower_cookies::{Cookie, Cookies};

    use super::App;

    fn app() -> App {
        let site = axum::Router::new()
            .route("/", get(|| async { "site" }))
            .route("/games/{id}", get(|| async { "site game" }));
        let engine = axum::Router::new()
            .route("/", get(|| async { "engine" }))
            .route("/games/{id}/frames", get(|| async { "engine frames" }));
        App::new(site).host(["engine.example.com", "Engine.Example.ORG."], engine)
    }

    async fn send(app: App, request: Request<Body>) -> (StatusCode, String) {
        let response = app.into_router().oneshot(request).await.unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    fn with_host(host: &str, path: &str) -> Request<Body> {
        Request::get(path)
            .header("host", host)
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn dispatches_by_host_ignoring_case_port_and_trailing_dot() {
        let cases = [
            ("engine.example.com", "engine"),
            ("ENGINE.EXAMPLE.COM:443", "engine"),
            ("engine.example.com:8080", "engine"),
            ("engine.example.com.", "engine"),
            ("engine.example.org", "engine"),
            ("example.com", "site"),
            ("notengine.example.com", "site"),
            ("engine.example.com.evil.test", "site"),
            ("[::1]:3000", "site"),
            ("not a host", "site"),
        ];
        for (host, expected) in cases {
            let (status, body) = send(app(), with_host(host, "/")).await;
            assert_eq!(status, StatusCode::OK, "host {host:?}");
            assert_eq!(body, expected, "host {host:?}");
        }
    }

    #[tokio::test]
    async fn a_request_without_a_host_goes_to_the_default_router() {
        let request = Request::get("/").body(Body::empty()).unwrap();
        assert_eq!(send(app(), request).await, (StatusCode::OK, "site".into()));
    }

    #[tokio::test]
    async fn falls_back_to_the_uri_authority_without_a_host_header() {
        let request = Request::get("http://Engine.Example.com:443/")
            .body(Body::empty())
            .unwrap();
        assert!(request.headers().get("host").is_none());
        assert_eq!(
            send(app(), request).await,
            (StatusCode::OK, "engine".into())
        );
    }

    #[tokio::test]
    async fn the_host_header_wins_over_the_uri_authority() {
        let request = Request::get("http://engine.example.com/")
            .header("host", "example.com")
            .body(Body::empty())
            .unwrap();
        assert_eq!(send(app(), request).await, (StatusCode::OK, "site".into()));
    }

    #[tokio::test]
    async fn routes_only_match_within_their_own_host_router() {
        assert_eq!(
            send(app(), with_host("engine.example.com", "/games/1"))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            send(app(), with_host("example.com", "/games/1/frames"))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn the_first_host_router_listing_a_host_wins() {
        let app = App::new(axum::Router::new())
            .host(
                ["a.example.com"],
                axum::Router::new().route("/", get(|| async { "first" })),
            )
            .host(
                ["A.example.com", "b.example.com"],
                axum::Router::new().route("/", get(|| async { "second" })),
            );
        assert_eq!(
            send(app.clone(), with_host("a.example.com", "/")).await.1,
            "first"
        );
        assert_eq!(send(app, with_host("b.example.com", "/")).await.1, "second");
    }

    #[tokio::test]
    async fn every_host_router_gets_the_cookie_middleware() {
        let cookie_route = || {
            axum::Router::new().route(
                "/",
                get(|cookies: Cookies| async move {
                    cookies.add(Cookie::new("seen", "yes"));
                    "ok"
                }),
            )
        };
        let app = App::new(cookie_route()).host(["engine.example.com"], cookie_route());
        for host in ["example.com", "engine.example.com"] {
            let response = app
                .clone()
                .into_router()
                .oneshot(with_host(host, "/"))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "host {host}");
            assert_eq!(response.headers()["set-cookie"], "seen=yes", "host {host}");
        }
    }
}
