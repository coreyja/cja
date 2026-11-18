use axum::{
    Router,
    body::Bytes,
    http::{HeaderMap, Uri},
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

pub async fn mock(delay: Duration) -> (String, Arc<Mutex<Vec<Capture>>>) {
    let captures = Arc::new(Mutex::new(Vec::new()));
    let state = Arc::clone(&captures);
    let app = Router::new().fallback(move |uri: Uri, headers: HeaderMap, body: Bytes| {
        let state = Arc::clone(&state);
        async move {
            state.lock().unwrap().push(Capture { uri, headers, body });
            tokio::time::sleep(delay).await;
        }
    });
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host = format!("http://{}", socket.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(socket, app).await.unwrap() });
    (host, captures)
}
