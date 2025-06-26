use axum::{extract::Request, response::Response};
use color_eyre::eyre::WrapErr;
use listenfd::ListenFd;
use std::{convert::Infallible, error::Error, net::SocketAddr};
use tokio::net::TcpListener;
use tower_cookies::CookieManagerLayer;
use tower_service::Service;

pub mod cookies {
    mod cookie_key;
    pub use cookie_key::CookieKey;

    mod cookie_jar;
    pub use cookie_jar::CookieJar;

    pub use tower_cookies::Cookie;

    pub use tower_cookies::cookie::SameSite;
}

pub mod page;

pub mod session;

pub mod trace;

pub async fn run_server<AS: Clone + Sync + Send + 'static, S, E>(
    routes: axum::Router<AS>,
) -> color_eyre::Result<()>
where
    for<'a> axum::Router<AS>: tower_service::Service<
            axum::serve::IncomingStream<'a, tokio::net::TcpListener>,
            Error = Infallible,
            Response = S,
        > + Send
        + Clone,
    S: Service<Request, Response = Response, Error = Infallible> + Clone + Send + 'static,
    S::Future: Send,
    axum::serve::Serve<tokio::net::TcpListener, axum::Router<AS>, S>:
        IntoFuture<Output = Result<(), E>>,
    E: Error + Send + Sync + 'static,
{
    let tracer = trace::Tracer;
    let trace_layer = tower_http::trace::TraceLayer::new_for_http()
        .make_span_with(tracer)
        .on_response(tracer);

    let app = routes.layer(trace_layer).layer(CookieManagerLayer::new());

    let port = std::env::var("PORT").unwrap_or_else(|_| "3000".to_string());
    let port: u16 = port.parse()?;
    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    // Check if we're being run under systemfd (LISTEN_FDS will be set)
    let listener = if let Some(fd_listener) = ListenFd::from_env().take_tcp_listener(0)? {
        // If systemfd is being used, we'll get a listener from fd
        let socket_addr = fd_listener.local_addr()?;

        tracing::info!("Zero-downtime reloading enabled");
        tracing::info!(
            "Using listener passed from systemfd on address {}",
            socket_addr
        );

        // Convert the std TcpListener to a tokio one
        TcpListener::from_std(fd_listener)?
    } else {
        // Otherwise, create our own listener
        tracing::info!("Starting server on port {}", port);
        TcpListener::bind(&addr)
            .await
            .wrap_err("Failed to open port")?
    };

    let addr = listener.local_addr()?;
    tracing::info!("Listening on {}", addr);

    axum::serve(listener, app)
        .await
        .wrap_err("Failed to run server")
}
