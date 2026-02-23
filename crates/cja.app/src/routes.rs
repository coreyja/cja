use axum::response::Redirect;
use axum::{Router, response::IntoResponse, routing::get};
use tower_http::services::ServeDir;

use crate::AppState;
use crate::templates;

pub fn router(app_state: AppState) -> Router {
    let docs_path = std::env::var("DOCS_PATH").unwrap_or_else(|_| "./target/doc".to_string());

    let docs_service = ServeDir::new(docs_path).fallback(get(|| async {
        Redirect::temporary("/docs/cja/index.html")
    }));

    Router::new()
        .route("/", get(landing))
        .route("/style.css", get(stylesheet))
        .route("/favicon.svg", get(favicon))
        .nest_service("/docs", docs_service)
        .with_state(app_state)
}

async fn landing() -> impl IntoResponse {
    templates::landing_page()
}

async fn stylesheet() -> impl IntoResponse {
    (
        [
            (axum::http::header::CONTENT_TYPE, "text/css"),
            (axum::http::header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_str!("../assets/style.css"),
    )
}

async fn favicon() -> impl IntoResponse {
    (
        [
            (axum::http::header::CONTENT_TYPE, "image/svg+xml"),
            (axum::http::header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_str!("../assets/favicon.svg"),
    )
}
