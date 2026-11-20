pub mod common;
use axum::{body::to_bytes, http::StatusCode};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use common::{Fixture, jwt};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

const CALLBACK: &str = "https://claude.ai/api/mcp/auth_callback";
const RESOURCE: &str = "https://app.example.test/mcp";
const VERIFIER: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-._~";

async fn body(response: axum::response::Response) -> String {
    String::from_utf8(
        to_bytes(response.into_body(), 1_000_000)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}
async fn register(f: &Fixture, name: &str) -> String {
    let response = f.request("POST", "/oauth/register", None, None,
        Some(&json!({"redirect_uris":[CALLBACK],"client_name":name,"token_endpoint_auth_method":"none"}).to_string())).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let value: Value = serde_json::from_str(&body(response).await).unwrap();
    assert!(value.get("client_secret").is_none());
    value["client_id"].as_str().unwrap().to_owned()
}
fn authorize_uri(client: &str, challenge: &str) -> String {
    let mut url = url::Url::parse("https://app.example.test/oauth/authorize").unwrap();
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client)
        .append_pair("redirect_uri", CALLBACK)
        .append_pair("scope", "mcp offline_access")
        .append_pair("state", "original-state")
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("resource", RESOURCE);
    format!("{}?{}", url.path(), url.query().unwrap())
}
fn field<'a>(html: &'a str, name: &str) -> &'a str {
    let marker = format!("name=\"{name}\" value=\"");
    html.split(&marker)
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
}
async fn first_consent(f: &Fixture, client: &str) -> (String, String, String, String) {
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(VERIFIER.as_bytes()));
    let (_, cookie, state) = f.start(&authorize_uri(client, &challenge)).await;
    let nonce = f.nonce().await;
    let _mock = f
        .token_mock(
            jwt(&f.server.base_url(), &nonce, "userid:123"),
            "upstream-1",
        )
        .await;
    let response = f.callback(&cookie, &state).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-frame-options"], "DENY");
    assert_eq!(
        response.headers()["content-security-policy"],
        "frame-ancestors 'none'"
    );
    assert_eq!(response.headers()["cache-control"], "no-store");
    let html = body(response).await;
    assert!(html.contains("self-reported"));
    assert!(html.contains(CALLBACK));
    assert!(html.contains(RESOURCE));
    assert!(html.contains("mcp offline_access"));
    (
        cookie,
        field(&html, "pending_id").to_owned(),
        field(&html, "csrf_token").to_owned(),
        html,
    )
}
async fn approve(f: &Fixture, cookie: &str, pending: &str, csrf: &str) -> String {
    let response = f
        .form(
            "/oauth/consent",
            Some(cookie),
            &[
                ("pending_id", pending),
                ("csrf_token", csrf),
                ("action", "allow"),
            ],
        )
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let location = response.headers()["location"].to_str().unwrap();
    let url = url::Url::parse(location).unwrap();
    assert_eq!(
        url.query_pairs().find(|(k, _)| k == "state").unwrap().1,
        "original-state"
    );
    url.query_pairs()
        .find(|(k, _)| k == "code")
        .unwrap()
        .1
        .to_string()
}
#[tokio::test]
async fn metadata_and_challenge() {
    let f = Fixture::new().await;
    for path in [
        "/.well-known/oauth-authorization-server",
        "/.well-known/oauth-authorization-server/mcp",
    ] {
        let response = f.request("GET", path, None, None, None).await;
        assert_eq!(response.status(), StatusCode::OK);
        let json: Value = serde_json::from_str(&body(response).await).unwrap();
        assert_eq!(
            json["authorization_endpoint"],
            "https://app.example.test/oauth/authorize"
        );
        assert_eq!(
            json["token_endpoint_auth_methods_supported"],
            json!(["none"])
        );
    }
    for path in [
        "/.well-known/oauth-protected-resource",
        "/.well-known/oauth-protected-resource/mcp",
    ] {
        let response = f.request("GET", path, None, None, None).await;
        let json: Value = serde_json::from_str(&body(response).await).unwrap();
        assert_eq!(json["resource"], RESOURCE);
    }
    let unauth = f.request("GET", "/mcp", None, None, None).await;
    assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        unauth.headers()["www-authenticate"],
        "Bearer resource_metadata=\"https://app.example.test/.well-known/oauth-protected-resource/mcp\""
    );
    f.close().await;
}

#[tokio::test]
async fn authorize_consent_and_exchange() {
    let f = Fixture::new().await;
    let client = register(&f, "<script>alert(1)</script>").await;
    let (cookie, pending, csrf, html) = first_consent(&f, &client).await;
    assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(!html.contains("<script>"));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM passport_oauth_grants")
            .fetch_one(&f.state.pool)
            .await
            .unwrap(),
        0
    );
    let wrong = f
        .form(
            "/oauth/consent",
            Some(&cookie),
            &[
                ("pending_id", &pending),
                ("csrf_token", "wrong"),
                ("action", "allow"),
            ],
        )
        .await;
    assert_eq!(wrong.status(), StatusCode::BAD_REQUEST);
    let code = approve(&f, &cookie, &pending, &csrf).await;
    let reused = f
        .form(
            "/oauth/consent",
            Some(&cookie),
            &[
                ("pending_id", &pending),
                ("csrf_token", &csrf),
                ("action", "allow"),
            ],
        )
        .await;
    assert_eq!(reused.status(), StatusCode::BAD_REQUEST);
    let success = f
        .form(
            "/oauth/token",
            None,
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("client_id", &client),
                ("redirect_uri", CALLBACK),
                ("resource", RESOURCE),
                ("code_verifier", VERIFIER),
            ],
        )
        .await;
    assert_eq!(success.status(), StatusCode::OK);
    assert_eq!(success.headers()["cache-control"], "no-store");
    assert_eq!(success.headers()["pragma"], "no-cache");
    let tokens: Value = serde_json::from_str(&body(success).await).unwrap();
    assert_eq!(tokens["expires_in"], 3600);
    let access = tokens["access_token"].as_str().unwrap();
    let ok = f
        .request("GET", "/mcp", None, Some(&format!("Bearer {access}")), None)
        .await;
    assert_eq!(ok.status(), StatusCode::OK);
    let replay = f
        .form(
            "/oauth/token",
            None,
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("client_id", &client),
                ("redirect_uri", CALLBACK),
                ("resource", RESOURCE),
                ("code_verifier", VERIFIER),
            ],
        )
        .await;
    assert_eq!(replay.status(), StatusCode::BAD_REQUEST);
    f.close().await;
}

#[tokio::test]
async fn dcr_allowlist_and_deny_are_enforced() {
    let f = Fixture::new().await;
    for metadata in [
        json!({}),
        json!({"redirect_uris":"wrong type"}),
        json!({"redirect_uris":["https://evil.example/callback"]}),
        json!({"redirect_uris":[CALLBACK,CALLBACK]}),
        json!({"redirect_uris":[CALLBACK],"token_endpoint_auth_method":"client_secret_basic"}),
        json!({"redirect_uris":[CALLBACK],"grant_types":["client_credentials"]}),
    ] {
        let response = f
            .request(
                "POST",
                "/oauth/register",
                None,
                None,
                Some(&metadata.to_string()),
            )
            .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(body(response).await.contains("invalid_client_metadata"));
    }
    let malformed = f
        .request("POST", "/oauth/register", None, None, Some("{"))
        .await;
    assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
    assert!(body(malformed).await.contains("invalid_client_metadata"));
    let client = register(&f, "Claude").await;
    let (cookie, pending, csrf, _) = first_consent(&f, &client).await;
    let response = f
        .form(
            "/oauth/consent",
            Some(&cookie),
            &[
                ("pending_id", &pending),
                ("csrf_token", &csrf),
                ("action", "deny"),
            ],
        )
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let location = response.headers()["location"].to_str().unwrap();
    assert!(location.starts_with(CALLBACK));
    assert!(location.contains("error=access_denied"));
    assert!(location.contains("state=original-state"));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM passport_oauth_grants")
            .fetch_one(&f.state.pool)
            .await
            .unwrap(),
        0
    );
    f.close().await;
}

async fn authorized_tokens(f: &Fixture) -> (String, Value) {
    let client = register(f, "Claude").await;
    let (cookie, pending, csrf, _) = first_consent(f, &client).await;
    let code = approve(f, &cookie, &pending, &csrf).await;
    let response = f
        .form(
            "/oauth/token",
            None,
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("client_id", &client),
                ("redirect_uri", CALLBACK),
                ("resource", RESOURCE),
                ("code_verifier", VERIFIER),
            ],
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    (client, serde_json::from_str(&body(response).await).unwrap())
}

async fn refresh_form(f: &Fixture, client: &str, raw: &str) -> axum::response::Response {
    f.form(
        "/oauth/token",
        None,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", raw),
            ("client_id", client),
            ("resource", RESOURCE),
        ],
    )
    .await
}

#[tokio::test]
async fn refresh_rotates_and_later_reuse_revokes_the_grant() {
    let f = Fixture::new().await;
    let (client, first) = authorized_tokens(&f).await;
    let old = first["refresh_token"].as_str().unwrap();
    let upstream = f
        .server
        .mock_async(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/token")
                .form_urlencoded_tuple("grant_type", "refresh_token");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "access_token":"0123456789abcdef0123456789abcdef", "token_type":"Bearer",
                    "expires_in":300, "refresh_token":"upstream-2"
                }));
        })
        .await;
    let response = f
        .form(
            "/oauth/token",
            None,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", old),
                ("client_id", &client),
                ("resource", RESOURCE),
            ],
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let second: Value = serde_json::from_str(&body(response).await).unwrap();
    assert_ne!(second["refresh_token"], first["refresh_token"]);
    assert_ne!(second["access_token"], first["access_token"]);
    assert_eq!(upstream.calls_async().await, 1);
    let new_access = second["access_token"].as_str().unwrap();
    assert_eq!(
        f.request(
            "GET",
            "/mcp",
            None,
            Some(&format!("Bearer {new_access}")),
            None
        )
        .await
        .status(),
        StatusCode::OK
    );
    let reused = f
        .form(
            "/oauth/token",
            None,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", old),
                ("client_id", &client),
                ("resource", RESOURCE),
            ],
        )
        .await;
    assert_eq!(reused.status(), StatusCode::BAD_REQUEST);
    assert_eq!(upstream.calls_async().await, 1);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM passport_oauth_grants")
            .fetch_one(&f.state.pool)
            .await
            .unwrap(),
        0
    );
    let revoked = f
        .request(
            "GET",
            "/mcp",
            None,
            Some(&format!("Bearer {new_access}")),
            None,
        )
        .await;
    assert_eq!(revoked.status(), StatusCode::UNAUTHORIZED);
    assert!(revoked.headers().contains_key("www-authenticate"));
    f.close().await;
}

#[tokio::test]
async fn nonallowlisted_and_tagged_subjects_create_no_grant() {
    for subject in ["userid:999", "node:tagged"] {
        let f = Fixture::new().await;
        let client = register(&f, "Claude").await;
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(VERIFIER.as_bytes()));
        let (_, cookie, state) = f.start(&authorize_uri(&client, &challenge)).await;
        let nonce = f.nonce().await;
        let _mock = f
            .token_mock(jwt(&f.server.base_url(), &nonce, subject), "upstream-1")
            .await;
        let response = f.callback(&cookie, &state).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM passport_oauth_pending_consents")
                .fetch_one(&f.state.pool)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM passport_oauth_grants")
                .fetch_one(&f.state.pool)
                .await
                .unwrap(),
            0
        );
        f.close().await;
    }
}

#[tokio::test]
async fn dcr_rate_limit_and_global_cap_with_pressure_prune() {
    let mut options = cja_passport::OAuthOptions {
        dcr_per_minute: 1,
        ..Default::default()
    };
    let f = Fixture::new_with_oauth_options(options.clone()).await;
    let _ = register(&f, "Claude").await;
    let response = f
        .request("POST", "/oauth/register", None, None, Some("not json"))
        .await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()["retry-after"], "60");
    assert!(body(response).await.contains("temporarily_unavailable"));
    f.close().await;

    options.dcr_per_minute = 10;
    options.client_cap = 1;
    let f = Fixture::new_with_oauth_options(options).await;
    let first = register(&f, "Claude").await;
    let full = f
        .request(
            "POST",
            "/oauth/register",
            None,
            None,
            Some(&json!({"redirect_uris":[CALLBACK]}).to_string()),
        )
        .await;
    assert_eq!(full.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(body(full).await.contains("temporarily_unavailable"));
    sqlx::query(
        "UPDATE passport_oauth_clients SET created_at=now()-interval '2 hours' WHERE client_id=$1",
    )
    .bind(&first)
    .execute(&f.state.pool)
    .await
    .unwrap();
    let second = register(&f, "Claude").await;
    assert_ne!(first, second);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM passport_oauth_clients")
            .fetch_one(&f.state.pool)
            .await
            .unwrap(),
        1
    );
    f.close().await;
}

#[tokio::test]
async fn consent_csrf_is_bound_to_one_session_and_expiry_denies() {
    let f = Fixture::new().await;
    let client = register(&f, "Claude").await;
    let (cookie, pending, csrf, _) = first_consent(&f, &client).await;
    let other_cookie = common::mint_cookie(uuid::Uuid::new_v4(), &f.state.key);
    for bad_cookie in [None, Some(other_cookie.as_str())] {
        let response = f
            .form(
                "/oauth/consent",
                bad_cookie,
                &[
                    ("pending_id", &pending),
                    ("csrf_token", &csrf),
                    ("action", "allow"),
                ],
            )
            .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(response.headers()["cache-control"], "no-store");
    }
    sqlx::query("UPDATE passport_oauth_pending_consents SET expires_at=now()-interval '1 second' WHERE id=$1")
        .bind(uuid::Uuid::parse_str(&pending).unwrap()).execute(&f.state.pool).await.unwrap();
    let expired = f
        .form(
            "/oauth/consent",
            Some(&cookie),
            &[
                ("pending_id", &pending),
                ("csrf_token", &csrf),
                ("action", "allow"),
            ],
        )
        .await;
    assert_eq!(expired.status(), StatusCode::SEE_OTHER);
    assert!(
        expired.headers()["location"]
            .to_str()
            .unwrap()
            .contains("error=access_denied")
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM passport_oauth_pending_consents")
            .fetch_one(&f.state.pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM passport_oauth_grants")
            .fetch_one(&f.state.pool)
            .await
            .unwrap(),
        0
    );
    f.close().await;
}

#[tokio::test]
async fn repeated_authorize_with_consent_stays_silent_and_replaces_grant() {
    let f = Fixture::new().await;
    let client = register(&f, "Claude").await;
    let (cookie, pending, csrf, _) = first_consent(&f, &client).await;
    let _code = approve(&f, &cookie, &pending, &csrf).await;
    let first_grant: uuid::Uuid = sqlx::query_scalar("SELECT id FROM passport_oauth_grants")
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    f.server.reset_async().await;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(VERIFIER.as_bytes()));
    let (_, second_cookie, second_state) = f.start(&authorize_uri(&client, &challenge)).await;
    let nonce = f.nonce().await;
    let _mock = f
        .token_mock(
            jwt(&f.server.base_url(), &nonce, "userid:123"),
            "upstream-2",
        )
        .await;
    let response = f.callback(&second_cookie, &second_state).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert!(!response.headers().contains_key("set-cookie"));
    let location = response.headers()["location"].to_str().unwrap();
    assert!(location.starts_with(CALLBACK));
    assert!(location.contains("code="));
    assert!(location.contains("state=original-state"));
    let second_grant: uuid::Uuid = sqlx::query_scalar("SELECT id FROM passport_oauth_grants")
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    assert_ne!(first_grant, second_grant);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM passport_oauth_pending_consents")
            .fetch_one(&f.state.pool)
            .await
            .unwrap(),
        0
    );
    f.close().await;
}

#[tokio::test]
async fn transient_refresh_honors_last_upstream_check_cap() {
    let f = Fixture::new().await;
    let (client, first) = authorized_tokens(&f).await;
    let transient = f
        .server
        .mock_async(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/token")
                .form_urlencoded_tuple("grant_type", "refresh_token");
            then.status(503).body("unavailable");
        })
        .await;
    let old = first["refresh_token"].as_str().unwrap();
    let response = f
        .form(
            "/oauth/token",
            None,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", old),
                ("client_id", &client),
                ("resource", RESOURCE),
            ],
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let second: Value = serde_json::from_str(&body(response).await).unwrap();
    assert_ne!(second["refresh_token"], first["refresh_token"]);
    let upstream: String =
        sqlx::query_scalar("SELECT upstream_refresh_token FROM passport_oauth_grants")
            .fetch_one(&f.state.pool)
            .await
            .unwrap();
    assert_eq!(upstream, "upstream-1");
    sqlx::query("UPDATE passport_oauth_grants SET last_upstream_check_at=now()-interval '9 hours'")
        .execute(&f.state.pool)
        .await
        .unwrap();
    let response = f
        .form(
            "/oauth/token",
            None,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", second["refresh_token"].as_str().unwrap()),
                ("client_id", &client),
                ("resource", RESOURCE),
            ],
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body(response).await.contains("invalid_grant"));
    assert_eq!(transient.calls_async().await, 2);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM passport_oauth_grants")
            .fetch_one(&f.state.pool)
            .await
            .unwrap(),
        0
    );
    f.close().await;
}

#[tokio::test]
async fn concurrent_refresh_uses_one_upstream_rotation_and_keeps_grant() {
    let f = Fixture::new().await;
    let (client, first) = authorized_tokens(&f).await;
    let upstream = f.server.mock_async(|when,then| {
        when.method(httpmock::Method::POST).path("/token").form_urlencoded_tuple("grant_type","refresh_token");
        then.status(200).header("content-type","application/json")
            .json_body(json!({"access_token":"0123456789abcdef0123456789abcdef","token_type":"Bearer","expires_in":300,"refresh_token":"upstream-2"}))
            .delay(std::time::Duration::from_millis(750));
    }).await;
    let old = first["refresh_token"].as_str().unwrap();
    let (a, b) = tokio::join!(
        refresh_form(&f, &client, old),
        refresh_form(&f, &client, old)
    );
    let statuses = [a.status(), b.status()];
    assert!(statuses.contains(&StatusCode::OK));
    assert!(statuses.contains(&StatusCode::BAD_REQUEST));
    let (ok, bad) = if a.status() == StatusCode::OK {
        (a, b)
    } else {
        (b, a)
    };
    assert!(body(bad).await.contains("invalid_grant"));
    let tokens: Value = serde_json::from_str(&body(ok).await).unwrap();
    assert_eq!(upstream.calls_async().await, 1);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM passport_oauth_grants")
            .fetch_one(&f.state.pool)
            .await
            .unwrap(),
        1
    );
    let access = tokens["access_token"].as_str().unwrap();
    assert_eq!(
        f.request("GET", "/mcp", None, Some(&format!("Bearer {access}")), None)
            .await
            .status(),
        StatusCode::OK
    );
    f.close().await;
}

#[tokio::test]
async fn code_exchange_rejects_mismatches_without_consuming_code() {
    let f = Fixture::new().await;
    let client = register(&f, "Claude").await;
    let (cookie, pending, csrf, _) = first_consent(&f, &client).await;
    let code = approve(&f, &cookie, &pending, &csrf).await;
    for (cid, uri, resource, verifier) in [
        ("wrong", CALLBACK, RESOURCE, VERIFIER),
        (
            client.as_str(),
            "https://claude.com/api/mcp/auth_callback",
            RESOURCE,
            VERIFIER,
        ),
        (
            client.as_str(),
            CALLBACK,
            "https://app.example.test/other",
            VERIFIER,
        ),
        (
            client.as_str(),
            CALLBACK,
            RESOURCE,
            "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-._x",
        ),
    ] {
        let response = f
            .form(
                "/oauth/token",
                None,
                &[
                    ("grant_type", "authorization_code"),
                    ("code", &code),
                    ("client_id", cid),
                    ("redirect_uri", uri),
                    ("resource", resource),
                    ("code_verifier", verifier),
                ],
            )
            .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert!(body(response).await.contains("invalid_grant"));
    }
    let response = f
        .form(
            "/oauth/token",
            None,
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("client_id", &client),
                ("redirect_uri", CALLBACK),
                ("resource", RESOURCE),
                ("code_verifier", VERIFIER),
            ],
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let tokens: Value = serde_json::from_str(&body(response).await).unwrap();
    let access = tokens["access_token"].as_str().unwrap();
    let stored: Vec<u8> = sqlx::query_scalar("SELECT token_hash FROM passport_oauth_access_tokens")
        .fetch_one(&f.state.pool)
        .await
        .unwrap();
    assert_eq!(stored, Sha256::digest(access.as_bytes()).to_vec());
    assert_ne!(stored, access.as_bytes());
    sqlx::query("UPDATE passport_oauth_access_tokens SET expires_at=now()-interval '1 second'")
        .execute(&f.state.pool)
        .await
        .unwrap();
    let expired = f
        .request("GET", "/mcp", None, Some(&format!("Bearer {access}")), None)
        .await;
    assert_eq!(expired.status(), StatusCode::UNAUTHORIZED);
    assert!(expired.headers().contains_key("www-authenticate"));
    let malformed = f
        .form(
            "/oauth/token",
            None,
            &[("grant_type", "authorization_code")],
        )
        .await;
    assert_eq!(malformed.headers()["cache-control"], "no-store");
    f.close().await;
}

#[tokio::test]
async fn upstream_invalid_grant_ends_connector_grant() {
    let f = Fixture::new().await;
    let (client, tokens) = authorized_tokens(&f).await;
    let upstream = f
        .server
        .mock_async(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/token")
                .form_urlencoded_tuple("grant_type", "refresh_token");
            then.status(400)
                .header("content-type", "application/json")
                .json_body(json!({"error":"invalid_grant"}));
        })
        .await;
    let response = refresh_form(&f, &client, tokens["refresh_token"].as_str().unwrap()).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body(response).await.contains("invalid_grant"));
    assert_eq!(upstream.calls_async().await, 1);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM passport_oauth_grants")
            .fetch_one(&f.state.pool)
            .await
            .unwrap(),
        0
    );
    let old_access = tokens["access_token"].as_str().unwrap();
    assert_eq!(
        f.request(
            "GET",
            "/mcp",
            None,
            Some(&format!("Bearer {old_access}")),
            None
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    f.close().await;
}

#[tokio::test]
async fn invalid_authorize_does_not_create_login_attempt() {
    let f = Fixture::new().await;
    let client = register(&f, "Claude").await;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(VERIFIER.as_bytes()));
    for (key, value) in [
        ("resource", "https://app.example.test/other"),
        ("redirect_uri", "https://evil.example/callback"),
        ("scope", "offline_access"),
        ("code_challenge", "bad"),
        ("code_challenge_method", "plain"),
        ("state", ""),
    ] {
        let mut uri = url::Url::parse(&format!(
            "https://app.example.test{}",
            authorize_uri(&client, &challenge)
        ))
        .unwrap();
        let pairs: Vec<(String, String)> = uri
            .query_pairs()
            .map(|(k, v)| {
                let matches = k == key;
                (
                    k.into_owned(),
                    if matches {
                        value.to_owned()
                    } else {
                        v.into_owned()
                    },
                )
            })
            .collect();
        uri.query_pairs_mut().clear().extend_pairs(pairs);
        let response = f
            .request(
                "GET",
                &format!("{}?{}", uri.path(), uri.query().unwrap()),
                None,
                None,
                None,
            )
            .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{key}");
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM passport_login_attempts")
            .fetch_one(&f.state.pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM passport_oauth_grants")
            .fetch_one(&f.state.pool)
            .await
            .unwrap(),
        0
    );
    f.close().await;
}

#[tokio::test]
async fn registration_uri_and_body_boundaries() {
    let short: Vec<String> = (0..11)
        .map(|n| format!("https://claude.ai/callback/{n}"))
        .collect();
    let prefix = "https://claude.ai/";
    let exact = format!("{prefix}{}", "a".repeat(2048 - prefix.len()));
    let too_long = format!("{exact}a");
    let mut options = cja_passport::OAuthOptions::default();
    options.allowed_redirect_uris.extend(short.iter().cloned());
    options.allowed_redirect_uris.insert(exact.clone());
    options.allowed_redirect_uris.insert(too_long.clone());
    let f = Fixture::new_with_oauth_options(options).await;
    let accepted = f
        .request(
            "POST",
            "/oauth/register",
            None,
            None,
            Some(&json!({"redirect_uris":short[..10]}).to_string()),
        )
        .await;
    assert_eq!(accepted.status(), StatusCode::CREATED);
    let eleven = f
        .request(
            "POST",
            "/oauth/register",
            None,
            None,
            Some(&json!({"redirect_uris":short}).to_string()),
        )
        .await;
    assert_eq!(eleven.status(), StatusCode::BAD_REQUEST);
    let exact_response = f
        .request(
            "POST",
            "/oauth/register",
            None,
            None,
            Some(&json!({"redirect_uris":[exact]}).to_string()),
        )
        .await;
    assert_eq!(exact_response.status(), StatusCode::CREATED);
    let over = f
        .request(
            "POST",
            "/oauth/register",
            None,
            None,
            Some(&json!({"redirect_uris":[too_long]}).to_string()),
        )
        .await;
    assert_eq!(over.status(), StatusCode::BAD_REQUEST);
    let oversized = f
        .request(
            "POST",
            "/oauth/register",
            None,
            None,
            Some(&json!({"redirect_uris":[CALLBACK],"client_name":"a".repeat(23_000)}).to_string()),
        )
        .await;
    assert_eq!(oversized.status(), StatusCode::PAYLOAD_TOO_LARGE);
    f.close().await;
}

#[tokio::test]
async fn changed_human_allowlist_revokes_without_calling_upstream() {
    use tower::ServiceExt as _;
    let f = Fixture::new().await;
    let (client, tokens) = authorized_tokens(&f).await;
    let upstream = f.server.mock_async(|when,then| {
        when.method(httpmock::Method::POST).path("/token").form_urlencoded_tuple("grant_type","refresh_token");
        then.status(200).header("content-type","application/json")
            .json_body(json!({"access_token":"0123456789abcdef0123456789abcdef","token_type":"Bearer","expires_in":300,"refresh_token":"upstream-2"}));
    }).await;
    let previous = &f.state.config;
    let config = cja_passport::PassportConfig::new(
        previous.issuer.clone(),
        previous.callback.clone(),
        previous.client_id.clone(),
        previous.client_secret.clone(),
        std::collections::HashSet::from([cja_passport::AllowedSubject::new("userid:999").unwrap()]),
        previous.native_schemes.clone(),
        previous.timings,
    )
    .unwrap();
    let state = common::TestState {
        pool: f.state.pool.clone(),
        key: f.state.key.clone(),
        config,
        oauth: f.state.oauth.clone(),
    };
    let router = axum::Router::new()
        .merge(cja_passport::oauth_router::<common::TestState>())
        .layer(tower_cookies::CookieManagerLayer::new())
        .with_state(state);
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    serializer.extend_pairs([
        ("grant_type", "refresh_token"),
        ("refresh_token", tokens["refresh_token"].as_str().unwrap()),
        ("client_id", &client),
        ("resource", RESOURCE),
    ]);
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/oauth/token")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(axum::body::Body::from(serializer.finish()))
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body(response).await.contains("invalid_grant"));
    assert_eq!(upstream.calls_async().await, 0);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM passport_oauth_grants")
            .fetch_one(&f.state.pool)
            .await
            .unwrap(),
        0
    );
    f.close().await;
}

#[tokio::test]
async fn connection_refusal_is_transient_until_outage_cap() {
    use std::sync::Arc;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
    let token_body = Arc::new(tokio::sync::Mutex::new(String::new()));
    let shared = token_body.clone();
    let app = axum::Router::new().route(
        "/token",
        axum::routing::post(move || {
            let shared = shared.clone();
            async move {
                (
                    [("content-type", "application/json")],
                    shared.lock().await.clone(),
                )
            }
        }),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let f = Fixture::new_with_token_endpoint(Some(endpoint)).await;
    let client = register(&f, "Claude").await;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(VERIFIER.as_bytes()));
    let (_, cookie, state) = f.start(&authorize_uri(&client, &challenge)).await;
    let nonce = f.nonce().await;
    *token_body.lock().await = json!({"access_token":"0123456789abcdef0123456789abcdef","token_type":"Bearer","expires_in":300,"refresh_token":"upstream-1","id_token":jwt(&f.server.base_url(),&nonce,"userid:123")}).to_string();
    let callback = f.callback(&cookie, &state).await;
    assert_eq!(callback.status(), StatusCode::OK);
    let html = body(callback).await;
    let code = approve(
        &f,
        &cookie,
        field(&html, "pending_id"),
        field(&html, "csrf_token"),
    )
    .await;
    let exchange = f
        .form(
            "/oauth/token",
            None,
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("client_id", &client),
                ("redirect_uri", CALLBACK),
                ("resource", RESOURCE),
                ("code_verifier", VERIFIER),
            ],
        )
        .await;
    assert_eq!(exchange.status(), StatusCode::OK);
    let tokens: Value = serde_json::from_str(&body(exchange).await).unwrap();
    server.abort();
    let _ = server.await;
    let first = refresh_form(&f, &client, tokens["refresh_token"].as_str().unwrap()).await;
    assert_eq!(first.status(), StatusCode::OK);
    let next: Value = serde_json::from_str(&body(first).await).unwrap();
    sqlx::query("UPDATE passport_oauth_grants SET last_upstream_check_at=now()-interval '9 hours'")
        .execute(&f.state.pool)
        .await
        .unwrap();
    let expired = refresh_form(&f, &client, next["refresh_token"].as_str().unwrap()).await;
    assert_eq!(expired.status(), StatusCode::BAD_REQUEST);
    assert!(body(expired).await.contains("invalid_grant"));
    f.close().await;
}
