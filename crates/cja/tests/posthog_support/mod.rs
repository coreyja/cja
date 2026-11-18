// Shared by several test binaries; each uses a different subset.
#![allow(dead_code)]

pub mod boot;

use axum::{
    Router,
    body::Bytes,
    http::{HeaderMap, StatusCode, Uri},
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

pub struct Capture {
    pub uri: Uri,
    pub headers: HeaderMap,
    pub body: Bytes,
}

pub type Captures = Arc<Mutex<Vec<Capture>>>;

/// A loopback server that records every request, then answers 200 after `delay`.
pub async fn mock(delay: Duration) -> (String, Captures) {
    mock_with(delay, StatusCode::OK).await
}

pub async fn mock_with(delay: Duration, status: StatusCode) -> (String, Captures) {
    let captures = Arc::new(Mutex::new(Vec::new()));
    let state = Arc::clone(&captures);
    let app = Router::new().fallback(move |uri: Uri, headers: HeaderMap, body: Bytes| {
        let state = Arc::clone(&state);
        async move {
            state.lock().unwrap().push(Capture { uri, headers, body });
            tokio::time::sleep(delay).await;
            status
        }
    });
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host = format!("http://{}", socket.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(socket, app).await.unwrap() });
    (host, captures)
}

/// Every captured body, concatenated, for requests whose path passes `path`.
pub fn payload(captures: &Captures, path: impl Fn(&str) -> bool) -> String {
    captures
        .lock()
        .unwrap()
        .iter()
        .filter(|c| path(c.uri.path()))
        .map(|c| String::from_utf8_lossy(&c.body).into_owned())
        .collect()
}
