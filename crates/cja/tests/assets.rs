use axum::{
    Router,
    body::to_bytes,
    http::{
        Request, StatusCode,
        header::{CACHE_CONTROL, CONTENT_TYPE},
    },
    routing::get,
};
use cja::assets::{Asset, AssetError, Manifest};
use tower::ServiceExt as _;

static APP_ASSETS: [Asset; 5] = [
    Asset {
        logical_name: "style.css",
        hashed_name: "style.abc12345.css",
        content_type: "text/css",
        bytes: b"body{}",
    },
    Asset {
        logical_name: "logo.svg",
        hashed_name: "logo.abc12345.svg",
        content_type: "image/svg+xml",
        bytes: b"<svg/>",
    },
    Asset {
        logical_name: "img/logo.png",
        hashed_name: "img/logo.abc12345.png",
        content_type: "image/png",
        bytes: b"png",
    },
    Asset {
        logical_name: "img/logo@2x.png",
        hashed_name: "img/logo@2x.abc12345.png",
        content_type: "image/png",
        bytes: b"png2",
    },
    Asset {
        logical_name: "robots.txt",
        hashed_name: "robots.abc12345.txt",
        content_type: "text/plain",
        bytes: b"User-agent: *",
    },
];
static APP: Manifest = Manifest::new("test-app", &APP_ASSETS);
static MAP_ASSETS: [Asset; 1] = [Asset {
    logical_name: "studio.js.map",
    hashed_name: "studio.js.abc12345.map",
    content_type: "application/json",
    bytes: br#"{"version":3,"sources":[],"mappings":""}"#,
}];
static MAP: Manifest = Manifest::new("test-app", &MAP_ASSETS);
static LIB_ASSETS: [Asset; 1] = [Asset {
    logical_name: "style.css",
    hashed_name: "lib.abc12345.css",
    content_type: "text/css",
    bytes: b"lib{}",
}];
static LIB: Manifest = Manifest::new("test-lib", &LIB_ASSETS);

#[tokio::test]
async fn serves_hashed_assets_and_stable_alias() {
    let app = cja::assets::router::<()>(&[&APP, &LIB]).unwrap();
    for (path, mime, bytes) in [
        ("/assets/style.abc12345.css", "text/css", &b"body{}"[..]),
        ("/assets/logo.abc12345.svg", "image/svg+xml", &b"<svg/>"[..]),
        ("/assets/img/logo.abc12345.png", "image/png", &b"png"[..]),
        (
            "/assets/img/logo@2x.abc12345.png",
            "image/png",
            &b"png2"[..],
        ),
        ("/assets/lib.abc12345.css", "text/css", &b"lib{}"[..]),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        assert_eq!(response.headers()[CONTENT_TYPE], mime);
        assert_eq!(
            response.headers()[CACHE_CONTROL],
            "public, max-age=31536000, immutable"
        );
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            bytes
        );
    }
    let response = app
        .oneshot(
            Request::builder()
                .uri("/robots.txt")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CACHE_CONTROL], "public, max-age=3600");
    assert_eq!(
        to_bytes(response.into_body(), usize::MAX).await.unwrap(),
        &b"User-agent: *"[..]
    );
}

#[tokio::test]
async fn unknown_and_unhashed_paths_are_404() {
    let app = cja::assets::router::<()>(&[&APP, &MAP]).unwrap();
    for path in [
        "/assets/style.css",
        "/assets/style.deadbeef.css",
        "/favicon.ico",
        "/assets/unknown",
        "/assets/studio.js.map",
        "/assets/studio.js.deadbeef.map",
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
}

#[tokio::test]
async fn source_map_has_json_mime_and_immutable_cache() {
    let app = cja::assets::router::<()>(&[&MAP]).unwrap();
    let response = app
        .oneshot(
            Request::builder()
                .uri("/assets/studio.js.abc12345.map")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CONTENT_TYPE], "application/json");
    assert_eq!(
        response.headers()[CACHE_CONTROL],
        "public, max-age=31536000, immutable"
    );
}

#[test]
fn lookup_is_scoped_and_missing_name_panics() {
    assert_eq!(APP.asset_url("style.css"), "/assets/style.abc12345.css");
    assert_eq!(LIB.asset_url("style.css"), "/assets/lib.abc12345.css");
    static HOOK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = HOOK.lock().unwrap();
    let old_hook = std::panic::take_hook();
    let location = std::sync::Arc::new(std::sync::Mutex::new(None));
    let captured = location.clone();
    std::panic::set_hook(Box::new(move |info| {
        *captured.lock().unwrap() = info.location().map(|l| l.file().to_owned());
    }));
    let panic = std::panic::catch_unwind(|| APP.asset_url("missing.png")).unwrap_err();
    std::panic::set_hook(old_hook);
    assert!(
        location
            .lock()
            .unwrap()
            .as_deref()
            .unwrap_or("")
            .ends_with("crates/cja/tests/assets.rs")
    );
    let message = panic
        .downcast_ref::<String>()
        .expect("String panic payload");
    assert!(message.contains("missing.png"));
}

static COLLIDING_HASH_ASSETS: [Asset; 1] = [Asset {
    logical_name: "other.css",
    hashed_name: "style.abc12345.css",
    content_type: "text/css",
    bytes: b"x",
}];
static COLLIDING_HASH: Manifest = Manifest::new("test-lib", &COLLIDING_HASH_ASSETS);
static COLLIDING_ALIAS_ASSETS: [Asset; 1] = [Asset {
    logical_name: "robots.txt",
    hashed_name: "other.abc12345.txt",
    content_type: "text/plain",
    bytes: b"x",
}];
static COLLIDING_ALIAS: Manifest = Manifest::new("test-lib", &COLLIDING_ALIAS_ASSETS);

#[test]
fn collisions_fail_before_router_startup() {
    let err = cja::assets::router::<()>(&[&APP, &COLLIDING_HASH])
        .err()
        .unwrap();
    assert_eq!(
        err,
        AssetError::Collision {
            url: "/assets/style.abc12345.css".to_owned(),
            first_owner: "test-app",
            first_logical_name: "style.css",
            second_owner: "test-lib",
            second_logical_name: "other.css"
        }
    );
    assert_eq!(
        err.to_string(),
        "asset URL collision at /assets/style.abc12345.css: test-app (style.css) and test-lib (other.css)"
    );
    let err = cja::assets::router::<()>(&[&APP, &COLLIDING_ALIAS])
        .err()
        .unwrap();
    assert_eq!(
        err,
        AssetError::Collision {
            url: "/robots.txt".to_owned(),
            first_owner: "test-app",
            first_logical_name: "robots.txt",
            second_owner: "test-lib",
            second_logical_name: "robots.txt"
        }
    );
    assert_eq!(
        err.to_string(),
        "asset URL collision at /robots.txt: test-app (robots.txt) and test-lib (robots.txt)"
    );
}

static FAVICON_ASSETS: [Asset; 2] = [
    Asset {
        logical_name: "favicon.ico",
        hashed_name: "favicon.abc12345.ico",
        content_type: "image/vnd.microsoft.icon",
        bytes: b"icon",
    },
    Asset {
        logical_name: "img/robots.txt",
        hashed_name: "img/robots.abc12345.txt",
        content_type: "text/plain",
        bytes: b"nested",
    },
];
static FAVICON: Manifest = Manifest::new("test-app", &FAVICON_ASSETS);

#[tokio::test]
async fn favicon_alias_is_root_only() {
    let app = cja::assets::router::<()>(&[&FAVICON]).unwrap();
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/favicon.ico")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CACHE_CONTROL], "public, max-age=3600");
    assert_eq!(
        to_bytes(response.into_body(), usize::MAX).await.unwrap(),
        &b"icon"[..]
    );
    let response = app
        .oneshot(
            Request::builder()
                .uri("/robots.txt")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unknown_assets_stay_404_with_app_fallbacks() {
    let assets = cja::assets::router::<()>(&[&APP]).unwrap();
    let fallback = Router::new()
        .fallback(|| async { (StatusCode::OK, "spa") })
        .merge(assets.clone());
    let wildcard = Router::new()
        .route("/{*path}", get(|| async { (StatusCode::OK, "spa") }))
        .merge(assets);
    let mut trailing_slash_statuses = Vec::new();
    for app in [fallback, wildcard] {
        for path in [
            "/assets",
            "/assets/",
            "/assets/style.css",
            "/assets/style.deadbeef.css",
            "/assets/unknown/nested.js",
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            if path == "/assets/" {
                trailing_slash_statuses.push(response.status());
            } else {
                assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
            }
        }
        let hit = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/assets/style.abc12345.css")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(hit.status(), StatusCode::OK);
        let spa = app
            .oneshot(
                Request::builder()
                    .uri("/other")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(spa.status(), StatusCode::OK);
    }
    assert_eq!(
        trailing_slash_statuses,
        [StatusCode::NOT_FOUND, StatusCode::NOT_FOUND]
    );
}
