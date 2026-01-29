#![cfg(feature = "testing")]

use std::time::Duration;

use cja::testing::mock_oauth::{self, MockUserConfig};
use reqwest::redirect::Policy;
use serde_json::json;
use tokio::net::TcpListener;

/// Start a test server and return (port, `JoinHandle`)
async fn start_test_server() -> (u16, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = mock_oauth::create_router();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    // Give server time to start
    tokio::time::sleep(Duration::from_millis(10)).await;
    (port, handle)
}

fn create_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap()
}

#[tokio::test]
async fn test_full_oauth_flow() {
    let (port, handle) = start_test_server().await;
    let base_url = format!("http://127.0.0.1:{port}");
    let client = create_client();

    // Step 1: Make authorize request
    let authorize_url = format!(
        "{base_url}/login/oauth/authorize?client_id=test_client&redirect_uri=http://localhost:3000/callback&state=test_state_123"
    );
    let resp = client.get(&authorize_url).send().await.unwrap();

    // Should get a redirect
    assert!(resp.status().is_redirection(), "Expected redirect response");

    // Extract the code from the Location header
    let location = resp.headers().get("location").unwrap().to_str().unwrap();
    assert!(
        location.starts_with("http://localhost:3000/callback?code="),
        "Expected redirect to callback with code"
    );

    // Parse the code from the redirect URL
    let url = reqwest::Url::parse(location).unwrap();
    let code = url
        .query_pairs()
        .find(|(k, _)| k == "code")
        .map(|(_, v)| v.to_string())
        .unwrap();
    let state = url
        .query_pairs()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v.to_string())
        .unwrap();

    assert!(code.starts_with("mock_code_"), "Expected mock code format");
    assert_eq!(state, "test_state_123", "State should be preserved");

    // Step 2: Exchange code for token
    let token_url = format!("{base_url}/login/oauth/access_token");
    let token_resp = client
        .post(&token_url)
        .header("Accept", "application/json")
        .form(&[
            ("client_id", "test_client"),
            ("client_secret", "test_secret"),
            ("code", &code),
            ("redirect_uri", "http://localhost:3000/callback"),
        ])
        .send()
        .await
        .unwrap();

    assert!(
        token_resp.status().is_success(),
        "Token exchange should succeed"
    );

    let token_json: serde_json::Value = token_resp.json().await.unwrap();
    let access_token = token_json["access_token"].as_str().unwrap();
    assert!(
        access_token.starts_with("mock_token_"),
        "Expected mock token format"
    );
    assert_eq!(token_json["token_type"], "bearer");
    assert_eq!(token_json["scope"], "user:email");

    // Step 3: Get user info
    let user_url = format!("{base_url}/user");
    let user_resp = client
        .get(&user_url)
        .header("Authorization", format!("Bearer {access_token}"))
        .send()
        .await
        .unwrap();

    assert!(
        user_resp.status().is_success(),
        "User request should succeed"
    );

    let user_json: serde_json::Value = user_resp.json().await.unwrap();
    assert_eq!(user_json["id"], 12345);
    assert_eq!(user_json["login"], "mock_user");
    assert_eq!(user_json["name"], "Mock User");
    assert_eq!(user_json["email"], "mock@example.com");

    handle.abort();
}

#[tokio::test]
async fn test_pre_register_user_flow() {
    let (port, handle) = start_test_server().await;
    let base_url = format!("http://127.0.0.1:{port}");
    let client = create_client();

    // Pre-register a custom user for a specific state
    let admin_url = format!("{base_url}/_admin/set-user-for-state");
    let pre_register_resp = client
        .post(&admin_url)
        .json(&json!({
            "state": "custom_state_456",
            "user": {
                "id": 99999,
                "login": "custom_user",
                "name": "Custom Test User",
                "email": "custom@example.com",
                "avatar_url": "https://example.com/custom-avatar.png"
            }
        }))
        .send()
        .await
        .unwrap();

    assert!(
        pre_register_resp.status().is_success(),
        "Pre-registration should succeed"
    );

    // Make authorize request with the same state
    let authorize_url = format!(
        "{base_url}/login/oauth/authorize?client_id=test_client&redirect_uri=http://localhost:3000/callback&state=custom_state_456"
    );
    let resp = client.get(&authorize_url).send().await.unwrap();

    let location = resp.headers().get("location").unwrap().to_str().unwrap();
    let url = reqwest::Url::parse(location).unwrap();
    let code = url
        .query_pairs()
        .find(|(k, _)| k == "code")
        .map(|(_, v)| v.to_string())
        .unwrap();

    // Exchange code for token
    let token_url = format!("{base_url}/login/oauth/access_token");
    let token_resp = client
        .post(&token_url)
        .header("Accept", "application/json")
        .form(&[
            ("client_id", "test_client"),
            ("client_secret", "test_secret"),
            ("code", &code),
            ("redirect_uri", "http://localhost:3000/callback"),
        ])
        .send()
        .await
        .unwrap();

    let token_json: serde_json::Value = token_resp.json().await.unwrap();
    let access_token = token_json["access_token"].as_str().unwrap();

    // Get user info - should be our custom user
    let user_url = format!("{base_url}/user");
    let user_resp = client
        .get(&user_url)
        .header("Authorization", format!("Bearer {access_token}"))
        .send()
        .await
        .unwrap();

    let user_json: serde_json::Value = user_resp.json().await.unwrap();
    assert_eq!(user_json["id"], 99999);
    assert_eq!(user_json["login"], "custom_user");
    assert_eq!(user_json["name"], "Custom Test User");
    assert_eq!(user_json["email"], "custom@example.com");

    handle.abort();
}

#[tokio::test]
async fn test_invalid_code_returns_400() {
    let (port, handle) = start_test_server().await;
    let base_url = format!("http://127.0.0.1:{port}");
    let client = create_client();

    let token_url = format!("{base_url}/login/oauth/access_token");
    let resp = client
        .post(&token_url)
        .form(&[
            ("client_id", "test_client"),
            ("client_secret", "test_secret"),
            ("code", "invalid_code"),
            ("redirect_uri", "http://localhost:3000/callback"),
        ])
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        400,
        "Invalid code should return 400"
    );

    handle.abort();
}

#[tokio::test]
async fn test_missing_auth_header_returns_401() {
    let (port, handle) = start_test_server().await;
    let base_url = format!("http://127.0.0.1:{port}");
    let client = create_client();

    let user_url = format!("{base_url}/user");
    let resp = client.get(&user_url).send().await.unwrap();

    assert_eq!(
        resp.status().as_u16(),
        401,
        "Missing auth header should return 401"
    );

    handle.abort();
}

#[tokio::test]
async fn test_invalid_token_returns_401() {
    let (port, handle) = start_test_server().await;
    let base_url = format!("http://127.0.0.1:{port}");
    let client = create_client();

    let user_url = format!("{base_url}/user");
    let resp = client
        .get(&user_url)
        .header("Authorization", "Bearer invalid_token_xyz")
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        401,
        "Invalid token should return 401"
    );

    handle.abort();
}

#[tokio::test]
async fn test_form_urlencoded_token_response() {
    let (port, handle) = start_test_server().await;
    let base_url = format!("http://127.0.0.1:{port}");
    let client = create_client();

    // Get a valid code first
    let authorize_url = format!(
        "{base_url}/login/oauth/authorize?client_id=test_client&redirect_uri=http://localhost:3000/callback&state=test_state"
    );
    let resp = client.get(&authorize_url).send().await.unwrap();
    let location = resp.headers().get("location").unwrap().to_str().unwrap();
    let url = reqwest::Url::parse(location).unwrap();
    let code = url
        .query_pairs()
        .find(|(k, _)| k == "code")
        .map(|(_, v)| v.to_string())
        .unwrap();

    // Exchange code without Accept: application/json (should get form-urlencoded)
    let token_url = format!("{base_url}/login/oauth/access_token");
    let resp = client
        .post(&token_url)
        .form(&[
            ("client_id", "test_client"),
            ("client_secret", "test_secret"),
            ("code", &code),
            ("redirect_uri", "http://localhost:3000/callback"),
        ])
        .send()
        .await
        .unwrap();

    let body = resp.text().await.unwrap();
    assert!(
        body.contains("access_token="),
        "Should contain access_token"
    );
    assert!(
        body.contains("token_type=bearer"),
        "Should contain token_type"
    );
    assert!(body.contains("scope="), "Should contain scope");

    handle.abort();
}

#[tokio::test]
async fn test_mock_user_config_default() {
    let config = MockUserConfig::default();
    assert_eq!(config.id, 12345);
    assert_eq!(config.login, "mock_user");
    assert_eq!(config.name, Some("Mock User".to_string()));
    assert_eq!(config.email, Some("mock@example.com".to_string()));
    assert_eq!(config.avatar_url, "https://example.com/avatar.png");
}
