use std::{sync::Arc, time::Duration};

use axum::{Router, routing::get};
use tokio::{net::TcpListener, sync::Notify};
use tokio_util::sync::CancellationToken;
use tower_cookies::{Cookie, Cookies};

#[tokio::test]
async fn shutdown_drains_the_active_response_and_keeps_cookie_middleware() {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let handler_started = started.clone();
    let handler_release = release.clone();
    let routes = Router::new().route(
        "/active",
        get(move |cookies: Cookies| {
            let started = handler_started.clone();
            let release = handler_release.clone();
            async move {
                cookies.add(Cookie::new("drained", "yes"));
                started.notify_one();
                release.notified().await;
                "finished response"
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let shutdown = CancellationToken::new();
    let mut server = tokio::spawn(cja::server::serve_until(
        listener,
        routes,
        shutdown.clone().cancelled_owned(),
    ));
    let request = tokio::spawn(reqwest::get(format!("http://{addr}/active")));
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    shutdown.cancel();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut server)
            .await
            .is_err(),
        "shutdown must wait for an in-flight response"
    );
    release.notify_one();
    let response = tokio::time::timeout(Duration::from_secs(2), request)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["set-cookie"], "drained=yes");
    assert_eq!(response.text().await.unwrap(), "finished response");
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        TcpListener::bind(addr).await.is_ok(),
        "listener must be released"
    );
}

#[tokio::test]
async fn an_idle_server_stops_without_waiting_for_a_request() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(2),
        cja::server::serve_until(listener, Router::new(), async {}),
    )
    .await
    .unwrap()
    .unwrap();
}
