pub mod common;
use axum::http::StatusCode;
use common::{Fixture, cookie, jwt};
use serde_json::Value;

#[tokio::test]
async fn native_handoff_is_single_use_and_bearer_is_revoked_on_logout() {
    let fixture = Fixture::new().await;
    let (_, old_cookie, state) = fixture
        .start("/auth/native/start?redirect_uri=testapp%3A%2F%2F%2Fauth")
        .await;
    let nonce = fixture.nonce().await;
    fixture
        .token_mock(
            jwt(&fixture.server.base_url(), &nonce, "userid:123"),
            "refresh-1",
        )
        .await;
    let callback = fixture.callback(&old_cookie, &state).await;
    assert_eq!(callback.status(), StatusCode::SEE_OTHER);
    assert_eq!(callback.headers()["cache-control"], "no-store");
    let uri = url::Url::parse(callback.headers()["location"].to_str().unwrap()).unwrap();
    assert_eq!(uri.scheme(), "testapp");
    assert_eq!(uri.path(), "/auth");
    let params: Vec<_> = uri.query_pairs().collect();
    assert_eq!(params.len(), 1);
    assert_eq!(params[0].0, "code");
    let handoff = params[0].1.to_string();
    let new_cookie = cookie(&callback);

    let exchange = fixture
        .request(
            "POST",
            "/auth/native/exchange",
            None,
            None,
            Some(&serde_json::json!({"code":handoff}).to_string()),
        )
        .await;
    assert_eq!(exchange.status(), StatusCode::OK);
    assert_eq!(exchange.headers()["cache-control"], "no-store");
    let body = axum::body::to_bytes(exchange.into_body(), 2048)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["token_type"], "Bearer");
    let bearer = format!("Bearer {}", json["token"].as_str().unwrap());
    let replay = fixture
        .request(
            "POST",
            "/auth/native/exchange",
            None,
            None,
            Some(&serde_json::json!({"code":handoff}).to_string()),
        )
        .await;
    assert_eq!(replay.status(), StatusCode::BAD_REQUEST);
    assert_bearer_auth_and_logout(&fixture, &bearer, &new_cookie).await;
    fixture.close().await;
}

async fn assert_bearer_auth_and_logout(fixture: &Fixture, bearer: &str, new_cookie: &str) {
    let good = fixture
        .request("GET", "/private", None, Some(bearer), None)
        .await;
    assert_eq!(good.status(), StatusCode::OK);
    let api = fixture
        .request("GET", "/api", None, Some(bearer), None)
        .await;
    assert_eq!(api.status(), StatusCode::OK);
    let missing = fixture.request("GET", "/api", None, None, None).await;
    assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);
    let sessions: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions")
        .fetch_one(&fixture.state.pool)
        .await
        .unwrap();
    assert_eq!(sessions, 1);
    let invalid = fixture
        .request(
            "GET",
            "/private",
            Some(new_cookie),
            Some("Bearer invalid"),
            None,
        )
        .await;
    assert_eq!(invalid.status(), StatusCode::UNAUTHORIZED);
    let invalid_optional = fixture
        .request(
            "GET",
            "/optional",
            Some(new_cookie),
            Some("Bearer invalid"),
            None,
        )
        .await;
    assert_eq!(invalid_optional.status(), StatusCode::UNAUTHORIZED);

    let logout = fixture
        .request("POST", "/auth/logout", None, Some(bearer), None)
        .await;
    assert_eq!(logout.status(), StatusCode::NO_CONTENT);
    let revoked = fixture
        .request("GET", "/private", None, Some(bearer), None)
        .await;
    assert_eq!(revoked.status(), StatusCode::UNAUTHORIZED);
    let revoked_api = fixture
        .request("GET", "/api", None, Some(bearer), None)
        .await;
    assert_eq!(revoked_api.status(), StatusCode::UNAUTHORIZED);
    let anonymous = fixture.request("GET", "/optional", None, None, None).await;
    assert_eq!(anonymous.status(), StatusCode::OK);
    let body = axum::body::to_bytes(anonymous.into_body(), 1024)
        .await
        .unwrap();
    assert_eq!(&body[..], b"none");
}

#[tokio::test]
async fn native_redirect_validation_and_handoff_expiry() {
    let fixture = Fixture::new().await;
    for redirect in [
        "https%3A%2F%2Fexample.com",
        "other%3A%2F%2Fauth",
        "testapp%3A%2F%2Fuser%40auth",
        "testapp%3A%2F%2Fauth%23x",
        "testapp%3A%2F%2Fauth%3Fcode%3Dx",
    ] {
        let response = fixture
            .request(
                "GET",
                &format!("/auth/native/start?redirect_uri={redirect}"),
                None,
                None,
                None,
            )
            .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    let (_, old_cookie, state) = fixture
        .start("/auth/native/start?redirect_uri=testapp%3A%2F%2Fauth")
        .await;
    let nonce = fixture.nonce().await;
    fixture
        .token_mock(
            jwt(&fixture.server.base_url(), &nonce, "userid:123"),
            "refresh-1",
        )
        .await;
    let callback = fixture.callback(&old_cookie, &state).await;
    let uri = url::Url::parse(callback.headers()["location"].to_str().unwrap()).unwrap();
    let handoff = uri
        .query_pairs()
        .find(|(k, _)| k == "code")
        .unwrap()
        .1
        .to_string();
    sqlx::query("UPDATE passport_handoffs SET expires_at = now() - interval '1 second'")
        .execute(&fixture.state.pool)
        .await
        .unwrap();
    let expired = fixture
        .request(
            "POST",
            "/auth/native/exchange",
            None,
            None,
            Some(&serde_json::json!({"code":handoff}).to_string()),
        )
        .await;
    assert_eq!(expired.status(), StatusCode::BAD_REQUEST);
    fixture.close().await;
}

#[tokio::test]
async fn bearer_uses_refresh_policy_and_original_hard_cap() {
    let fixture = Fixture::new().await;
    let (_, old_cookie, state) = fixture
        .start("/auth/native/start?redirect_uri=testapp%3A%2F%2Fauth")
        .await;
    let nonce = fixture.nonce().await;
    fixture
        .token_mock(
            jwt(&fixture.server.base_url(), &nonce, "userid:123"),
            "refresh-1",
        )
        .await;
    let callback = fixture.callback(&old_cookie, &state).await;
    let uri = url::Url::parse(callback.headers()["location"].to_str().unwrap()).unwrap();
    let handoff = uri
        .query_pairs()
        .find(|(k, _)| k == "code")
        .unwrap()
        .1
        .to_string();
    let exchange = fixture
        .request(
            "POST",
            "/auth/native/exchange",
            None,
            None,
            Some(&serde_json::json!({"code":handoff}).to_string()),
        )
        .await;
    let body = axum::body::to_bytes(exchange.into_body(), 2048)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();
    let bearer = format!("Bearer {}", json["token"].as_str().unwrap());
    sqlx::query("UPDATE passport_sessions SET next_refresh_at = now() - interval '1 second'")
        .execute(&fixture.state.pool)
        .await
        .unwrap();
    let refreshed = fixture
        .server
        .mock_async(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/token")
                .form_urlencoded_tuple("refresh_token", "refresh-1");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({
                    "access_token":"a", "token_type":"Bearer", "refresh_token":"refresh-2"
                }));
        })
        .await;
    assert_eq!(
        fixture
            .request("GET", "/api", None, Some(&bearer), None)
            .await
            .status(),
        StatusCode::OK
    );
    refreshed.assert_calls_async(1).await;
    sqlx::query("UPDATE passport_sessions SET authenticated_at = now() - interval '9 hours'")
        .execute(&fixture.state.pool)
        .await
        .unwrap();
    assert_eq!(
        fixture
            .request("GET", "/api", None, Some(&bearer), None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        fixture
            .request("GET", "/optional", None, Some(&bearer), None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM passport_sessions")
        .fetch_one(&fixture.state.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    fixture.close().await;
}
