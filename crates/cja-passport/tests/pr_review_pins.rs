pub mod common;
use axum::http::StatusCode;
use common::Fixture;
use tower::ServiceExt as _;

/// HTTP/2 requests (and HTTP/1.1 absolute-form requests) reach handlers with an
/// absolute `parts.uri` (`https://host/path?query`). The plan's contract is that
/// `CurrentUser`'s 303 carries `return_to=<percent-encoded original path+query>`,
/// which `/auth/login` must accept; a full URL is rejected there with 400.
#[tokio::test]
async fn current_user_redirect_uses_path_and_query_for_absolute_form_uri() {
    let fixture = Fixture::new().await;
    let request = axum::http::Request::builder()
        .method("GET")
        .uri("https://app.example.test/private?x=1")
        .header("accept", "text/html")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = fixture.router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let location = response.headers()["location"].to_str().unwrap().to_owned();
    assert_eq!(location, "/auth/login?return_to=%2Fprivate%3Fx%3D1");
    let login = fixture.request("GET", &location, None, None, None).await;
    assert_eq!(login.status(), StatusCode::SEE_OTHER);
    fixture.close().await;
}
