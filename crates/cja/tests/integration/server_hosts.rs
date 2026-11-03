use std::time::Duration;

use axum::{Router, routing::get};
use cja::server::App;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn serve_until_dispatches_an_app_by_host() {
    let app = App::new(Router::new().route("/", get(|| async { "site" }))).host(
        ["engine.example.com"],
        Router::new().route("/", get(|| async { "engine" })),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(cja::server::serve_until(
        listener,
        app,
        shutdown.clone().cancelled_owned(),
    ));

    let client = reqwest::Client::new();
    for (host, expected) in [
        (None, "site"),
        (Some("engine.example.com"), "engine"),
        (Some("example.com"), "site"),
    ] {
        let mut request = client.get(format!("http://{addr}/"));
        if let Some(host) = host {
            request = request.header("host", host);
        }
        let body = request.send().await.unwrap().text().await.unwrap();
        assert_eq!(body, expected, "host {host:?}");
    }

    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
