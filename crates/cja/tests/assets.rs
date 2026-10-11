use axum::{
    body::to_bytes,
    http::{
        Request, StatusCode,
        header::{CACHE_CONTROL, CONTENT_TYPE},
    },
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
static APP: Manifest = Manifest::new(&APP_ASSETS);
static LIB_ASSETS: [Asset; 1] = [Asset {
    logical_name: "style.css",
    hashed_name: "lib.abc12345.css",
    content_type: "text/css",
    bytes: b"lib{}",
}];
static LIB: Manifest = Manifest::new(&LIB_ASSETS);

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
    let app = cja::assets::router::<()>(&[&APP]).unwrap();
    for path in [
        "/assets/style.css",
        "/assets/style.deadbeef.css",
        "/favicon.ico",
        "/assets/unknown",
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

#[test]
fn lookup_is_scoped_and_missing_name_panics() {
    assert_eq!(APP.asset_url("style.css"), "/assets/style.abc12345.css");
    assert_eq!(LIB.asset_url("style.css"), "/assets/lib.abc12345.css");
    let panic = std::panic::catch_unwind(|| APP.asset_url("missing.png")).unwrap_err();
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
static COLLIDING_HASH: Manifest = Manifest::new(&COLLIDING_HASH_ASSETS);
static COLLIDING_ALIAS_ASSETS: [Asset; 1] = [Asset {
    logical_name: "robots.txt",
    hashed_name: "other.abc12345.txt",
    content_type: "text/plain",
    bytes: b"x",
}];
static COLLIDING_ALIAS: Manifest = Manifest::new(&COLLIDING_ALIAS_ASSETS);

#[test]
fn collisions_fail_before_router_startup() {
    let err = cja::assets::router::<()>(&[&APP, &COLLIDING_HASH])
        .err()
        .unwrap();
    assert_eq!(
        err,
        AssetError::Collision {
            url: "/assets/style.abc12345.css".to_owned(),
            first: "style.css",
            second: "other.css"
        }
    );
    let err = cja::assets::router::<()>(&[&APP, &COLLIDING_ALIAS])
        .err()
        .unwrap();
    assert_eq!(
        err,
        AssetError::Collision {
            url: "/robots.txt".to_owned(),
            first: "robots.txt",
            second: "robots.txt"
        }
    );
}
