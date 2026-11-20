#![cfg(not(feature = "postgres"))]

use std::time::Duration;

use axum::{Router, extract::State, routing::get};
use cja::{app_state::AppState, server::cookies::CookieKey};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct StatelessState {
    cookie_key: CookieKey,
}

impl AppState for StatelessState {
    fn version(&self) -> &'static str {
        "1.0.0"
    }

    fn cookie_key(&self) -> &CookieKey {
        &self.cookie_key
    }
}

#[tokio::test]
async fn stateless_app_serves_health_route() {
    let state = StatelessState {
        cookie_key: CookieKey::generate(),
    };
    let routes = Router::new()
        .route(
            "/health",
            get(|State(state): State<StatelessState>| async move { state.version().to_owned() }),
        )
        .with_state(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(cja::server::serve_until(
        listener,
        routes,
        shutdown.clone().cancelled_owned(),
    ));

    let response = reqwest::get(format!("http://{addr}/health")).await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(response.text().await.unwrap(), "1.0.0");

    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
