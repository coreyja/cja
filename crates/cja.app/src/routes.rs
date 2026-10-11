use axum::response::Redirect;
use axum::{Router, response::IntoResponse, routing::get};
use tower_http::services::ServeDir;

use crate::AppState;
use crate::templates;

pub fn router(app_state: AppState) -> Result<Router, cja::assets::AssetError> {
    let docs_path = std::env::var("DOCS_PATH").unwrap_or_else(|_| "./target/doc".to_string());

    let docs_service = ServeDir::new(docs_path).fallback(get(|| async {
        Redirect::temporary("/docs/cja/index.html")
    }));

    Ok(Router::new()
        .route("/", get(landing))
        .nest_service("/docs", docs_service)
        .merge(cja::assets::router::<AppState>(&[
            &crate::site_assets::MANIFEST,
        ])?)
        .with_state(app_state))
}

async fn landing() -> impl IntoResponse {
    templates::landing_page()
}
