use axum::{Router, extract::State};
use cja::{app_state::AppState, deadpool_postgres::Pool, server::cookies::CookieKey};
use reqwest::cookie::{CookieStore as _, Jar};
use std::sync::Arc;

#[derive(Clone)]
pub struct TestAppState {
    pub db_pool: Pool,
    pub cookie_key: CookieKey,
}

impl AppState for TestAppState {
    fn version(&self) -> &'static str {
        "test-1.0.0"
    }

    fn db(&self) -> &Pool {
        &self.db_pool
    }

    fn cookie_key(&self) -> &CookieKey {
        &self.cookie_key
    }
}

impl TestAppState {
    pub fn new(db_pool: Pool) -> Self {
        Self {
            db_pool,
            cookie_key: CookieKey::from_env_or_generate().expect("Failed to generate cookie key"),
        }
    }
}

#[allow(dead_code)]
pub fn create_test_router<AS: AppState>(state: AS) -> Router {
    Router::new()
        .route("/health", axum::routing::get(|| async { "OK" }))
        .route("/session", axum::routing::get(session_handler::<AS>))
        .with_state(state)
}

#[allow(dead_code)]
async fn session_handler<AS: AppState>(State(_state): State<AS>) -> &'static str {
    "Session OK"
}

#[allow(dead_code)]
pub struct TestClient {
    client: reqwest::Client,
    base_url: String,
    cookies: Arc<Jar>,
}

#[allow(dead_code)]
impl TestClient {
    pub fn new(port: u16) -> Self {
        let cookie_jar = Arc::new(Jar::default());
        let client = reqwest::Client::builder()
            .cookie_provider(cookie_jar.clone())
            .build()
            .unwrap();

        Self {
            client,
            base_url: format!("http://localhost:{port}"),
            cookies: cookie_jar,
        }
    }

    pub async fn get(&self, path: &str) -> reqwest::Result<reqwest::Response> {
        self.client
            .get(format!("{}{}", self.base_url, path))
            .send()
            .await
    }

    pub fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.client.post(format!("{}{}", self.base_url, path))
    }

    pub fn cookie_value(&self, name: &str) -> Option<String> {
        let url = self.base_url.parse::<reqwest::Url>().ok()?;

        let header_str = self.cookies.cookies(&url)?.to_str().ok()?.to_string();

        header_str
            .split(';')
            .find(|cookie| cookie.trim().starts_with(&format!("{name}=")))
            .and_then(|cookie| cookie.split('=').nth(1))
            .map(std::string::ToString::to_string)
    }
}

#[allow(dead_code)]
pub async fn spawn_test_server(router: Router) -> (TestClient, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Failed to bind to port");

    let port = listener.local_addr().unwrap().port();

    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.expect("Server failed");
    });

    // Give server time to start
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    let client = TestClient::new(port);

    (client, handle)
}
