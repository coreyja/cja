pub mod common;
use axum::http::StatusCode;
use common::{Fixture, cookie, jwt};

#[tokio::test]
async fn browser_login_rotates_session_and_logout_revokes() {
    let fixture = Fixture::new().await;
    let (auth, old_cookie, state) = fixture
        .start("/auth/login?return_to=%2Fprivate%3Fx%3D1")
        .await;
    let parsed = url::Url::parse(&auth).unwrap();
    let params: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
    assert_eq!(params["scope"], "openid email profile");
    assert_eq!(params["code_challenge_method"], "S256");
    assert_ne!(params["code_challenge"], "");
    assert_eq!(params["response_type"], "code");
    assert_eq!(params["client_id"], "test-client");
    assert_eq!(
        params["redirect_uri"],
        "https://app.example.test/auth/callback"
    );
    let nonce = fixture.nonce().await;
    assert_eq!(params["nonce"], nonce);
    let token_mock = fixture
        .token_mock(
            jwt(&fixture.server.base_url(), &nonce, "userid:123"),
            "refresh-1",
        )
        .await;
    let callback = fixture.callback(&old_cookie, &state).await;
    assert_eq!(callback.status(), StatusCode::SEE_OTHER);
    assert_eq!(callback.headers()["location"], "/private?x=1");
    let new_cookie = cookie(&callback);
    assert_ne!(old_cookie, new_cookie);
    token_mock.assert_calls_async(1).await;

    let old = fixture
        .request("GET", "/private", Some(&old_cookie), None, None)
        .await;
    assert_eq!(old.status(), StatusCode::UNAUTHORIZED);
    let current = fixture
        .request("GET", "/private", Some(&new_cookie), None, None)
        .await;
    assert_eq!(current.status(), StatusCode::OK);
    let body = axum::body::to_bytes(current.into_body(), 1024)
        .await
        .unwrap();
    let user_id = std::str::from_utf8(&body).unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM passport_users WHERE user_id = $1")
        .bind(uuid::Uuid::parse_str(user_id).unwrap())
        .fetch_one(&fixture.state.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);

    let logout = fixture
        .request("POST", "/auth/logout", Some(&new_cookie), None, None)
        .await;
    assert_eq!(logout.status(), StatusCode::NO_CONTENT);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM passport_sessions")
        .fetch_one(&fixture.state.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    fixture.close().await;
}

#[tokio::test]
async fn denied_node_and_replayed_or_missing_state_never_authenticate() {
    let fixture = Fixture::new().await;
    let (_, cookie, state) = fixture.start("/auth/login").await;
    let denied = fixture
        .request(
            "GET",
            &format!("/auth/callback?state={state}&error=access_denied"),
            Some(&cookie),
            None,
            None,
        )
        .await;
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    let denied_body = axum::body::to_bytes(denied.into_body(), 1024)
        .await
        .unwrap();
    assert!(
        std::str::from_utf8(&denied_body)
            .unwrap()
            .contains("human Tailscale user")
    );
    let replay = fixture
        .request(
            "GET",
            &format!("/auth/callback?state={state}&error=access_denied"),
            Some(&cookie),
            None,
            None,
        )
        .await;
    assert_eq!(replay.status(), StatusCode::BAD_REQUEST);
    let cookieless = fixture
        .request(
            "GET",
            &format!("/auth/callback?state={state}&code=x"),
            None,
            None,
            None,
        )
        .await;
    assert_eq!(cookieless.status(), StatusCode::BAD_REQUEST);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM passport_sessions")
        .fetch_one(&fixture.state.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    fixture.close().await;
}

#[tokio::test]
async fn allowlist_rejects_before_authenticated_row() {
    let fixture = Fixture::new().await;
    let (_, cookie, state) = fixture.start("/auth/login").await;
    let nonce = fixture.nonce().await;
    fixture
        .token_mock(
            jwt(&fixture.server.base_url(), &nonce, "userid:999"),
            "refresh-1",
        )
        .await;
    let callback = fixture.callback(&cookie, &state).await;
    assert_eq!(callback.status(), StatusCode::FORBIDDEN);
    let users: i64 = sqlx::query_scalar("SELECT count(*) FROM passport_users")
        .fetch_one(&fixture.state.pool)
        .await
        .unwrap();
    assert_eq!(users, 0);
    let bindings: i64 = sqlx::query_scalar("SELECT count(*) FROM passport_sessions")
        .fetch_one(&fixture.state.pool)
        .await
        .unwrap();
    assert_eq!(bindings, 0);
    fixture.close().await;
}

#[tokio::test]
async fn unsafe_return_path_and_navigation_redirect() {
    let fixture = Fixture::new().await;
    for path in ["%2F%2Fevil.example", "%2Ffoo%23fragment", "%2Ffoo%5Cbar"] {
        let response = fixture
            .request(
                "GET",
                &format!("/auth/login?return_to={path}"),
                None,
                None,
                None,
            )
            .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    let mut request = axum::http::Request::builder()
        .method("GET")
        .uri("/private?x=1")
        .header("accept", "text/html")
        .body(axum::body::Body::empty())
        .unwrap();
    use tower::ServiceExt as _;
    let response = fixture
        .router
        .clone()
        .oneshot(std::mem::replace(
            &mut request,
            axum::http::Request::new(axum::body::Body::empty()),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        response.headers()["location"],
        "/auth/login?return_to=%2Fprivate%3Fx%3D1"
    );
    fixture.close().await;
}

#[tokio::test]
async fn migrations_replay_and_prune_expired_attempts() {
    let fixture = Fixture::new().await;
    cja_passport::run_migrations(&fixture.state.pool)
        .await
        .unwrap();
    let (_, _, _) = fixture.start("/auth/login").await;
    sqlx::query("UPDATE passport_login_attempts SET expires_at = now() - interval '1 second'")
        .execute(&fixture.state.pool)
        .await
        .unwrap();
    assert_eq!(
        cja_passport::prune_auth_artifacts(&fixture.state.pool)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        cja_passport::prune_auth_artifacts(&fixture.state.pool)
            .await
            .unwrap(),
        0
    );
    fixture.close().await;
}

#[tokio::test]
async fn invalid_grant_deletes_session_and_next_navigation_redirects() {
    let fixture = Fixture::new().await;
    let (_, old_cookie, state) = fixture.start("/auth/login").await;
    let nonce = fixture.nonce().await;
    fixture
        .token_mock(
            jwt(&fixture.server.base_url(), &nonce, "userid:123"),
            "refresh-1",
        )
        .await;
    let callback = fixture.callback(&old_cookie, &state).await;
    assert_eq!(callback.status(), StatusCode::SEE_OTHER);
    let cookie = cookie(&callback);
    sqlx::query("UPDATE passport_sessions SET next_refresh_at = now() - interval '1 second'")
        .execute(&fixture.state.pool)
        .await
        .unwrap();
    let invalid = fixture
        .server
        .mock_async(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/token")
                .form_urlencoded_tuple("grant_type", "refresh_token")
                .form_urlencoded_tuple("refresh_token", "refresh-1");
            then.status(400)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({"error":"invalid_grant"}));
        })
        .await;
    let rejected = fixture
        .request("GET", "/private", Some(&cookie), None, None)
        .await;
    assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
    invalid.assert_calls_async(1).await;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM passport_sessions")
        .fetch_one(&fixture.state.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let response = fixture
        .router
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri("/private?next=1")
                .header("cookie", &cookie)
                .header("accept", "text/html")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        response.headers()["location"],
        "/auth/login?return_to=%2Fprivate%3Fnext%3D1"
    );
    fixture.close().await;
}

#[tokio::test]
async fn server_error_preserves_session_until_original_hard_cap() {
    let fixture = Fixture::new().await;
    let (_, old_cookie, state) = fixture.start("/auth/login").await;
    let nonce = fixture.nonce().await;
    fixture
        .token_mock(
            jwt(&fixture.server.base_url(), &nonce, "userid:123"),
            "refresh-1",
        )
        .await;
    let callback = fixture.callback(&old_cookie, &state).await;
    let cookie = cookie(&callback);
    sqlx::query("UPDATE passport_sessions SET next_refresh_at = now() - interval '1 second'")
        .execute(&fixture.state.pool)
        .await
        .unwrap();
    let outage = fixture
        .server
        .mock_async(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/token")
                .form_urlencoded_tuple("grant_type", "refresh_token");
            then.status(503).body("unavailable");
        })
        .await;
    let working = fixture
        .request("GET", "/private", Some(&cookie), None, None)
        .await;
    assert_eq!(working.status(), StatusCode::OK);
    outage.assert_calls_async(1).await;
    let deferred: bool =
        sqlx::query_scalar("SELECT next_refresh_at > now() FROM passport_sessions")
            .fetch_one(&fixture.state.pool)
            .await
            .unwrap();
    assert!(deferred);
    let still_working = fixture
        .request("GET", "/private", Some(&cookie), None, None)
        .await;
    assert_eq!(still_working.status(), StatusCode::OK);
    outage.assert_calls_async(1).await;
    sqlx::query("UPDATE passport_sessions SET authenticated_at = now() - interval '9 hours'")
        .execute(&fixture.state.pool)
        .await
        .unwrap();
    let expired = fixture
        .request("GET", "/private", Some(&cookie), None, None)
        .await;
    assert_eq!(expired.status(), StatusCode::UNAUTHORIZED);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM passport_sessions")
        .fetch_one(&fixture.state.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    fixture.close().await;
}

#[tokio::test]
async fn refresh_must_not_authenticate_after_hard_cap_passes_during_token_call() {
    let fixture = Fixture::new().await;
    let (_, old_cookie, state) = fixture.start("/auth/login").await;
    let nonce = fixture.nonce().await;
    fixture
        .token_mock(
            jwt(&fixture.server.base_url(), &nonce, "userid:123"),
            "refresh-1",
        )
        .await;
    let callback = fixture.callback(&old_cookie, &state).await;
    assert_eq!(callback.status(), StatusCode::SEE_OTHER);
    let cookie = cookie(&callback);
    let slow_outage = fixture
        .server
        .mock_async(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/token")
                .form_urlencoded_tuple("refresh_token", "refresh-1");
            then.status(503)
                .delay(std::time::Duration::from_secs(3))
                .body("unavailable");
        })
        .await;
    sqlx::query(
        "UPDATE passport_sessions SET authenticated_at = now() - interval '8 hours' + interval '2 seconds',
         next_refresh_at = now() - interval '1 second'",
    )
    .execute(&fixture.state.pool)
    .await
    .unwrap();

    let response = fixture
        .request("GET", "/private", Some(&cookie), None, None)
        .await;
    slow_outage.assert_calls_async(1).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM passport_sessions")
        .fetch_one(&fixture.state.pool)
        .await
        .unwrap();
    assert_eq!(remaining, 0);
    fixture.close().await;
}

#[tokio::test]
async fn refused_token_connection_defers_refresh_without_revoking_session() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let token_endpoint = format!("http://{}/token", listener.local_addr().unwrap());
    let fixture = Fixture::new_with_token_endpoint(Some(token_endpoint)).await;
    let (_, cookie, _) = fixture.start("/auth/login").await;
    let session_id: uuid::Uuid =
        sqlx::query_scalar("SELECT session_id FROM passport_login_attempts")
            .fetch_one(&fixture.state.pool)
            .await
            .unwrap();
    let user_id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO passport_users (issuer, subject) VALUES ($1, 'userid:123') RETURNING user_id",
    )
    .bind(fixture.server.base_url())
    .fetch_one(&fixture.state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO passport_sessions
         (session_id, user_id, authenticated_at, last_refresh_at, next_refresh_at, refresh_token)
         VALUES ($1, $2, now(), now() - interval '2 hours', now() - interval '1 second', 'refresh-1')",
    )
    .bind(session_id)
    .bind(user_id)
    .execute(&fixture.state.pool)
    .await
    .unwrap();
    drop(listener);

    let response = fixture
        .request("GET", "/private", Some(&cookie), None, None)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let row: (String, bool) = sqlx::query_as(
        "SELECT refresh_token, next_refresh_at > now() FROM passport_sessions WHERE session_id = $1",
    )
    .bind(session_id)
    .fetch_one(&fixture.state.pool)
    .await
    .unwrap();
    assert_eq!(row.0, "refresh-1");
    assert!(row.1);
    fixture.close().await;
}

#[tokio::test]
async fn rotated_refresh_token_is_persisted_and_used_next_time() {
    let fixture = Fixture::new().await;
    let (_, old_cookie, state) = fixture.start("/auth/login").await;
    let nonce = fixture.nonce().await;
    fixture
        .token_mock(
            jwt(&fixture.server.base_url(), &nonce, "userid:123"),
            "refresh-1",
        )
        .await;
    let callback = fixture.callback(&old_cookie, &state).await;
    let cookie = cookie(&callback);
    let first = fixture
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
    sqlx::query("UPDATE passport_sessions SET next_refresh_at = now() - interval '1 second'")
        .execute(&fixture.state.pool)
        .await
        .unwrap();
    assert_eq!(
        fixture
            .request("GET", "/private", Some(&cookie), None, None)
            .await
            .status(),
        StatusCode::OK
    );
    first.assert_calls_async(1).await;
    let stored: String = sqlx::query_scalar("SELECT refresh_token FROM passport_sessions")
        .fetch_one(&fixture.state.pool)
        .await
        .unwrap();
    assert_eq!(stored, "refresh-2");
    let second = fixture
        .server
        .mock_async(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/token")
                .form_urlencoded_tuple("refresh_token", "refresh-2");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({
                    "access_token":"b", "token_type":"Bearer", "refresh_token":"refresh-3"
                }));
        })
        .await;
    sqlx::query("UPDATE passport_sessions SET next_refresh_at = now() - interval '1 second'")
        .execute(&fixture.state.pool)
        .await
        .unwrap();
    assert_eq!(
        fixture
            .request("GET", "/private", Some(&cookie), None, None)
            .await
            .status(),
        StatusCode::OK
    );
    second.assert_calls_async(1).await;
    first.assert_calls_async(1).await;
    let stored: String = sqlx::query_scalar("SELECT refresh_token FROM passport_sessions")
        .fetch_one(&fixture.state.pool)
        .await
        .unwrap();
    assert_eq!(stored, "refresh-3");
    fixture.close().await;
}

use tower::ServiceExt as _;

#[tokio::test]
async fn concurrent_refresh_serves_committed_identity_without_second_token_call() {
    let fixture = Fixture::new().await;
    let (_, old_cookie, state) = fixture.start("/auth/login").await;
    let nonce = fixture.nonce().await;
    fixture
        .token_mock(
            jwt(&fixture.server.base_url(), &nonce, "userid:123"),
            "refresh-1",
        )
        .await;
    let callback = fixture.callback(&old_cookie, &state).await;
    let cookie = cookie(&callback);
    sqlx::query("UPDATE passport_sessions SET next_refresh_at = now() - interval '1 second'")
        .execute(&fixture.state.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE sessions SET updated_at = now() - interval '2 hours'")
        .execute(&fixture.state.pool)
        .await
        .unwrap();

    let refresh = fixture
        .server
        .mock_async(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/token")
                .form_urlencoded_tuple("refresh_token", "refresh-1");
            then.status(200)
                .delay(std::time::Duration::from_millis(800))
                .header("content-type", "application/json")
                .json_body(serde_json::json!({
                    "access_token":"a", "token_type":"Bearer", "refresh_token":"refresh-2"
                }));
        })
        .await;
    let first_router = fixture.router.clone();
    let first_cookie = cookie.clone();
    let first = tokio::spawn(async move {
        first_router
            .oneshot(
                axum::http::Request::builder()
                    .uri("/private")
                    .header("cookie", first_cookie)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while refresh.calls_async().await == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("refresh token request was sent");
    let locked = sqlx::query("SELECT session_id FROM passport_sessions FOR UPDATE NOWAIT")
        .fetch_one(&fixture.state.pool)
        .await
        .unwrap_err();
    assert_eq!(
        locked.as_database_error().unwrap().code().as_deref(),
        Some("55P03")
    );

    let second = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        fixture.request("GET", "/private", Some(&cookie), None, None),
    )
    .await
    .expect("second request must not wait for refresh");
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(first.await.unwrap().status(), StatusCode::OK);
    refresh.assert_calls_async(1).await;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM passport_sessions")
        .fetch_one(&fixture.state.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    fixture.close().await;
}

#[tokio::test]
async fn missing_rotated_token_ends_session_and_database_error_is_500() {
    let fixture = Fixture::new().await;
    let (_, old_cookie, state) = fixture.start("/auth/login").await;
    let nonce = fixture.nonce().await;
    fixture
        .token_mock(
            jwt(&fixture.server.base_url(), &nonce, "userid:123"),
            "refresh-1",
        )
        .await;
    let callback = fixture.callback(&old_cookie, &state).await;
    let cookie = cookie(&callback);
    sqlx::query("UPDATE passport_sessions SET next_refresh_at = now() - interval '1 second'")
        .execute(&fixture.state.pool)
        .await
        .unwrap();
    fixture
        .server
        .mock_async(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/token")
                .form_urlencoded_tuple("refresh_token", "refresh-1");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({
                    "access_token":"a", "token_type":"Bearer"
                }));
        })
        .await;
    assert_eq!(
        fixture
            .request("GET", "/private", Some(&cookie), None, None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM passport_sessions")
        .fetch_one(&fixture.state.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);

    sqlx::query("DROP TABLE passport_sessions CASCADE")
        .execute(&fixture.state.pool)
        .await
        .unwrap();
    let failed = fixture
        .request("GET", "/private", Some(&cookie), None, None)
        .await;
    assert_eq!(failed.status(), StatusCode::INTERNAL_SERVER_ERROR);
    fixture.close().await;
}

#[tokio::test]
async fn setec_secret_rotation_is_used_by_next_refresh() {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let fixture = Fixture::new().await;
    let (_, old_cookie, state) = fixture.start("/auth/login").await;
    let nonce = fixture.nonce().await;
    fixture
        .token_mock(
            jwt(&fixture.server.base_url(), &nonce, "userid:123"),
            "refresh-1",
        )
        .await;
    let callback = fixture.callback(&old_cookie, &state).await;
    let cookie = cookie(&callback);
    fixture
        .fake
        .rotate("attic/passport/test-client", b"new-secret");
    fixture.refresher.refresh().await.unwrap();
    sqlx::query("UPDATE passport_sessions SET next_refresh_at = now() - interval '1 second'")
        .execute(&fixture.state.pool)
        .await
        .unwrap();
    let expected_auth = format!("Basic {}", STANDARD.encode("test-client:new-secret"));
    let rotated = fixture
        .server
        .mock_async(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/token")
                .form_urlencoded_tuple("refresh_token", "refresh-1")
                .header("authorization", &expected_auth);
            then.status(200)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({
                    "access_token":"a", "token_type":"Bearer", "refresh_token":"refresh-2"
                }));
        })
        .await;
    let response = fixture
        .request("GET", "/private", Some(&cookie), None, None)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    rotated.assert_calls_async(1).await;
    fixture.close().await;
}

#[tokio::test]
async fn not_due_session_works_with_fresh_config_during_idp_outage() {
    let fixture = Fixture::new().await;
    let (_, old_cookie, state) = fixture.start("/auth/login").await;
    let nonce = fixture.nonce().await;
    fixture
        .token_mock(
            jwt(&fixture.server.base_url(), &nonce, "userid:123"),
            "refresh-1",
        )
        .await;
    let callback = fixture.callback(&old_cookie, &state).await;
    let cookie = cookie(&callback);
    fixture.server.reset_async().await;
    let previous = &fixture.state.config;
    let config = cja_passport::PassportConfig::new(
        previous.issuer.clone(),
        previous.callback.clone(),
        previous.client_id.clone(),
        previous.client_secret.clone(),
        previous.allowlist.clone(),
        previous.native_schemes.clone(),
        previous.timings,
    )
    .unwrap();
    let state = common::TestState {
        pool: fixture.state.pool.clone(),
        key: fixture.state.key.clone(),
        config,
        oauth: fixture.state.oauth.clone(),
    };
    async fn protected(cja_passport::CurrentUser(user): cja_passport::CurrentUser) -> String {
        user.user_id.to_string()
    }
    let router = axum::Router::new()
        .route("/private", axum::routing::get(protected))
        .layer(tower_cookies::CookieManagerLayer::new())
        .with_state(state);
    let response = router
        .oneshot(
            axum::http::Request::builder()
                .uri("/private")
                .header("cookie", cookie)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    fixture.close().await;
}

#[tokio::test]
async fn expired_and_concurrent_callbacks_consume_only_one_attempt() {
    let fixture = Fixture::new().await;
    let (_, cookie, state) = fixture.start("/auth/login").await;
    sqlx::query("UPDATE passport_login_attempts SET expires_at = now() - interval '1 second'")
        .execute(&fixture.state.pool)
        .await
        .unwrap();
    let expired = fixture.callback(&cookie, &state).await;
    assert_eq!(expired.status(), StatusCode::BAD_REQUEST);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM passport_sessions")
        .fetch_one(&fixture.state.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);

    let (_, cookie, state) = fixture.start("/auth/login").await;
    let nonce = fixture.nonce().await;
    let token = fixture
        .token_mock(
            jwt(&fixture.server.base_url(), &nonce, "userid:123"),
            "refresh-1",
        )
        .await;
    let (first, second) = tokio::join!(
        fixture.callback(&cookie, &state),
        fixture.callback(&cookie, &state)
    );
    let statuses = [first.status(), second.status()];
    assert!(statuses.contains(&StatusCode::SEE_OTHER));
    assert!(statuses.contains(&StatusCode::BAD_REQUEST));
    token.assert_calls_async(1).await;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM passport_sessions")
        .fetch_one(&fixture.state.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    fixture.close().await;
}

#[tokio::test]
async fn refresh_timeout_releases_row_lock_and_later_retry_uses_old_token() {
    let fixture = Fixture::new().await;
    let (_, old_cookie, state) = fixture.start("/auth/login").await;
    let nonce = fixture.nonce().await;
    fixture
        .token_mock(
            jwt(&fixture.server.base_url(), &nonce, "userid:123"),
            "refresh-1",
        )
        .await;
    let callback = fixture.callback(&old_cookie, &state).await;
    let cookie = cookie(&callback);
    sqlx::query("UPDATE passport_sessions SET next_refresh_at = now() - interval '1 second'")
        .execute(&fixture.state.pool)
        .await
        .unwrap();
    let slow = fixture
        .server
        .mock_async(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/token")
                .form_urlencoded_tuple("refresh_token", "refresh-1");
            then.status(200)
                .delay(std::time::Duration::from_secs(7))
                .header("content-type", "application/json")
                .json_body(serde_json::json!({
                    "access_token":"a", "token_type":"Bearer", "refresh_token":"refresh-2"
                }));
        })
        .await;
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(8),
        fixture.request("GET", "/private", Some(&cookie), None, None),
    )
    .await
    .expect("request must finish after token timeout");
    assert_eq!(response.status(), StatusCode::OK);
    slow.assert_calls_async(1).await;
    let lockable = sqlx::query("SELECT session_id FROM passport_sessions FOR UPDATE NOWAIT")
        .fetch_one(&fixture.state.pool)
        .await;
    assert!(
        lockable.is_ok(),
        "timeout must release the row lock: {lockable:?}"
    );
    let stored: String = sqlx::query_scalar("SELECT refresh_token FROM passport_sessions")
        .fetch_one(&fixture.state.pool)
        .await
        .unwrap();
    assert_eq!(stored, "refresh-1");
    slow.delete_async().await;
    let retry = fixture
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
    sqlx::query("UPDATE passport_sessions SET next_refresh_at = now() - interval '1 second'")
        .execute(&fixture.state.pool)
        .await
        .unwrap();
    assert_eq!(
        fixture
            .request("GET", "/private", Some(&cookie), None, None)
            .await
            .status(),
        StatusCode::OK
    );
    retry.assert_calls_async(1).await;
    fixture.close().await;
}

#[tokio::test]
async fn same_issuer_and_subject_keep_stable_local_user_id() {
    let fixture = Fixture::new().await;
    let (_, first_cookie, first_state) = fixture.start("/auth/login").await;
    let first_nonce = fixture.nonce().await;
    let first_token = fixture
        .token_mock(
            jwt(&fixture.server.base_url(), &first_nonce, "userid:123"),
            "refresh-1",
        )
        .await;
    let first_callback = fixture.callback(&first_cookie, &first_state).await;
    assert_eq!(first_callback.status(), StatusCode::SEE_OTHER);
    first_token.delete_async().await;
    let first_cookie = cookie(&first_callback);
    let first_response = fixture
        .request("GET", "/private", Some(&first_cookie), None, None)
        .await;
    let first_id = axum::body::to_bytes(first_response.into_body(), 1024)
        .await
        .unwrap();

    let (_, second_cookie, second_state) = fixture.start("/auth/login").await;
    let second_nonce = fixture.nonce().await;
    fixture
        .token_mock(
            jwt(&fixture.server.base_url(), &second_nonce, "userid:123"),
            "refresh-2",
        )
        .await;
    let second_callback = fixture.callback(&second_cookie, &second_state).await;
    assert_eq!(second_callback.status(), StatusCode::SEE_OTHER);
    let second_cookie = cookie(&second_callback);
    let second_response = fixture
        .request("GET", "/private", Some(&second_cookie), None, None)
        .await;
    let second_id = axum::body::to_bytes(second_response.into_body(), 1024)
        .await
        .unwrap();
    assert_eq!(first_id, second_id);
    let users: i64 = sqlx::query_scalar("SELECT count(*) FROM passport_users")
        .fetch_one(&fixture.state.pool)
        .await
        .unwrap();
    assert_eq!(users, 1);
    let sessions: i64 = sqlx::query_scalar("SELECT count(*) FROM passport_sessions")
        .fetch_one(&fixture.state.pool)
        .await
        .unwrap();
    assert_eq!(sessions, 2);
    fixture.close().await;
}

#[tokio::test]
async fn allowlist_is_rechecked_on_authenticated_request() {
    let fixture = Fixture::new().await;
    let (_, old_cookie, state) = fixture.start("/auth/login").await;
    let nonce = fixture.nonce().await;
    fixture
        .token_mock(
            jwt(&fixture.server.base_url(), &nonce, "userid:123"),
            "refresh-1",
        )
        .await;
    let callback = fixture.callback(&old_cookie, &state).await;
    let cookie = cookie(&callback);
    assert_eq!(
        fixture
            .request("GET", "/private", Some(&cookie), None, None)
            .await
            .status(),
        StatusCode::OK
    );

    let original = &fixture.state.config;
    let config = cja_passport::PassportConfig::new(
        original.issuer.clone(),
        original.callback.clone(),
        original.client_id.clone(),
        original.client_secret.clone(),
        std::collections::HashSet::from([cja_passport::AllowedSubject::new("userid:999").unwrap()]),
        original.native_schemes.clone(),
        original.timings,
    )
    .unwrap();
    let state = common::TestState {
        pool: fixture.state.pool.clone(),
        key: fixture.state.key.clone(),
        config,
        oauth: fixture.state.oauth.clone(),
    };
    async fn protected(cja_passport::CurrentUser(user): cja_passport::CurrentUser) -> String {
        user.user_id.to_string()
    }
    let router = axum::Router::new()
        .route("/private", axum::routing::get(protected))
        .layer(tower_cookies::CookieManagerLayer::new())
        .with_state(state);
    let rejected = router
        .oneshot(
            axum::http::Request::builder()
                .uri("/private")
                .header("cookie", cookie)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
    fixture.close().await;
}
