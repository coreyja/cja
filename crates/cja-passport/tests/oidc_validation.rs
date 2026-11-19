pub mod common;
use axum::http::StatusCode;
use common::{Fixture, jwt_claims};
use serde_json::json;

#[tokio::test]
async fn invalid_id_token_claims_cannot_authenticate() {
    for variant in ["aud", "exp", "nonce", "nbf", "iss", "sub"] {
        let fixture = Fixture::new().await;
        let (_, cookie, state) = fixture.start("/auth/login").await;
        let nonce = fixture.nonce().await;
        let now = jiff::Timestamp::now().as_second();
        let mut claims = json!({
            "iss": fixture.server.base_url(), "sub": "userid:123",
            "aud": "test-client", "exp": now+300, "iat":now,
            "nbf":now-1, "jti":"test-jti", "nonce":nonce
        });
        match variant {
            "aud" => claims["aud"] = json!("other-client"),
            "exp" => claims["exp"] = json!(now - 1),
            "nonce" => claims["nonce"] = json!("wrong"),
            "nbf" => claims["nbf"] = json!(now + 100),
            "iss" => claims["iss"] = json!("https://other.example"),
            "sub" => claims["sub"] = json!("node:123"),
            _ => unreachable!(),
        }
        fixture.token_mock(jwt_claims(&claims), "refresh-1").await;
        let callback = fixture.callback(&cookie, &state).await;
        assert!(
            matches!(
                callback.status(),
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
            ),
            "{variant}: {}",
            callback.status()
        );
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM passport_sessions")
            .fetch_one(&fixture.state.pool)
            .await
            .unwrap();
        assert_eq!(count, 0, "{variant}");
        fixture.close().await;
    }
}

#[tokio::test]
async fn bad_signature_unknown_kid_and_access_hash_are_rejected() {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    for variant in ["signature", "kid", "at_hash"] {
        let fixture = Fixture::new().await;
        let (_, cookie, state) = fixture.start("/auth/login").await;
        let nonce = fixture.nonce().await;
        let now = jiff::Timestamp::now().as_second();
        let mut claims = json!({
            "iss": fixture.server.base_url(), "sub": "userid:123",
            "aud": "test-client", "exp": now+300, "iat":now,
            "nbf":now-1, "jti":"test-jti", "nonce":nonce
        });
        if variant == "at_hash" {
            claims["at_hash"] = json!("AAAAAAAAAAAAAAAAAAAAAA");
        }
        let mut token = jwt_claims(&claims);
        if variant == "signature" {
            let mut parts: Vec<String> = token.split('.').map(str::to_owned).collect();
            let first = parts[2].remove(0);
            parts[2].insert(0, if first == 'A' { 'B' } else { 'A' });
            token = parts.join(".");
        }
        if variant == "kid" {
            let mut parts: Vec<String> = token.split('.').map(str::to_owned).collect();
            parts[0] = URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(&json!({"alg":"RS256","kid":"unknown"})).unwrap());
            token = parts.join(".");
        }
        fixture.token_mock(token, "refresh-1").await;
        let callback = fixture.callback(&cookie, &state).await;
        assert_eq!(callback.status(), StatusCode::UNAUTHORIZED, "{variant}");
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM passport_sessions")
            .fetch_one(&fixture.state.pool)
            .await
            .unwrap();
        assert_eq!(count, 0, "{variant}");
        fixture.close().await;
    }
}

#[tokio::test]
async fn refreshed_id_token_with_other_subject_revokes_session() {
    let fixture = Fixture::new().await;
    let (_, old_cookie, state) = fixture.start("/auth/login").await;
    let nonce = fixture.nonce().await;
    fixture
        .token_mock(
            common::jwt(&fixture.server.base_url(), &nonce, "userid:123"),
            "refresh-1",
        )
        .await;
    let callback = fixture.callback(&old_cookie, &state).await;
    let cookie = common::cookie(&callback);
    sqlx::query("UPDATE passport_sessions SET next_refresh_at = now() - interval '1 second'")
        .execute(&fixture.state.pool)
        .await
        .unwrap();
    let wrong = common::jwt(&fixture.server.base_url(), "old-nonce", "userid:999");
    fixture
        .server
        .mock_async(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/token")
                .form_urlencoded_tuple("refresh_token", "refresh-1");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "access_token":"a", "token_type":"Bearer", "refresh_token":"refresh-2",
                    "id_token":wrong
                }));
        })
        .await;
    let protected = fixture
        .request("GET", "/private", Some(&cookie), None, None)
        .await;
    assert_eq!(protected.status(), StatusCode::UNAUTHORIZED);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM passport_sessions")
        .fetch_one(&fixture.state.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    fixture.close().await;
}

#[tokio::test]
async fn refreshed_id_token_with_same_subject_does_not_require_old_nonce() {
    let fixture = Fixture::new().await;
    let (_, old_cookie, state) = fixture.start("/auth/login").await;
    let nonce = fixture.nonce().await;
    fixture
        .token_mock(
            common::jwt(&fixture.server.base_url(), &nonce, "userid:123"),
            "refresh-1",
        )
        .await;
    let callback = fixture.callback(&old_cookie, &state).await;
    let cookie = common::cookie(&callback);
    sqlx::query("UPDATE passport_sessions SET next_refresh_at = now() - interval '1 second'")
        .execute(&fixture.state.pool)
        .await
        .unwrap();
    let refreshed_id = common::jwt(
        &fixture.server.base_url(),
        "new-nonce-not-from-login",
        "userid:123",
    );
    let refresh = fixture
        .server
        .mock_async(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/token")
                .form_urlencoded_tuple("refresh_token", "refresh-1");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "access_token":"a", "token_type":"Bearer", "refresh_token":"refresh-2",
                    "id_token":refreshed_id
                }));
        })
        .await;
    let protected = fixture
        .request("GET", "/private", Some(&cookie), None, None)
        .await;
    assert_eq!(protected.status(), StatusCode::OK);
    refresh.assert_calls_async(1).await;
    let stored: String = sqlx::query_scalar("SELECT refresh_token FROM passport_sessions")
        .fetch_one(&fixture.state.pool)
        .await
        .unwrap();
    assert_eq!(stored, "refresh-2");
    fixture.close().await;
}
