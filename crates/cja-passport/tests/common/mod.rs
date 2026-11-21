#[cfg(feature = "tailnet")]
use std::sync::Arc;
use std::{collections::HashSet, sync::OnceLock};

use axum::{
    Router,
    http::{Request, Response, StatusCode},
    routing::get,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use cja::server::{cookies::CookieKey, session::SESSION_COOKIE_NAME};
use cja_passport::{
    AllowedSubject, BearerUser, CurrentUser, HasMcpOAuthConfig, HasPassportConfig, McpOAuthConfig,
    McpUser, OAuthOptions, OptionalUser, PassportConfig, SessionTimings, oauth_router,
    passport_router,
};
use cja_secret::{FakeSetec, Secret, Secrets};
use httpmock::{
    Method::{GET, POST},
    MockServer,
};
use openidconnect::{ClientId, IssuerUrl, RedirectUrl};
use rsa::{
    RsaPrivateKey, RsaPublicKey,
    pkcs1v15::SigningKey,
    signature::{SignatureEncoding as _, Signer as _},
    traits::PublicKeyParts as _,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt as _;

#[derive(Clone)]
pub struct TestState {
    pub pool: PgPool,
    pub key: CookieKey,
    pub config: PassportConfig,
    pub oauth: McpOAuthConfig,
}
impl cja::app_state::AppState for TestState {
    fn version(&self) -> &'static str {
        "test"
    }
    fn db(&self) -> &PgPool {
        &self.pool
    }
    fn cookie_key(&self) -> &CookieKey {
        &self.key
    }
}
impl HasPassportConfig for TestState {
    fn passport_config(&self) -> &PassportConfig {
        &self.config
    }
}
impl HasMcpOAuthConfig for TestState {
    fn mcp_oauth_config(&self) -> &McpOAuthConfig {
        &self.oauth
    }
}

#[derive(Secrets)]
struct TestSecrets {
    #[secret("attic/passport/test-client")]
    client: Secret<String>,
}

pub struct Fixture {
    pub state: TestState,
    pub router: Router,
    pub server: MockServer,
    pub guard: cja::testing::test_db::TestDatabaseGuard,
    pub fake: FakeSetec,
    pub refresher: cja_secret::Refresher,
    pub issuer: String,
}

fn key() -> &'static RsaPrivateKey {
    static KEY: OnceLock<RsaPrivateKey> = OnceLock::new();
    KEY.get_or_init(|| RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).unwrap())
}

pub fn jwt(issuer: &str, nonce: &str, subject: &str) -> String {
    let now = jiff::Timestamp::now().as_second();
    let claims = json!({
        "iss": issuer, "sub": subject, "aud": "test-client", "exp": now + 300,
        "iat": now, "nbf": now - 1, "jti": "test-jti", "nonce": nonce,
        "email": "user@example.test", "preferred_username": "user",
    });
    jwt_claims(&claims)
}

pub fn jwt_claims(claims: &Value) -> String {
    let header = json!({"alg":"RS256","typ":"JWT","kid":"test-key"});
    let signing_input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap()),
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap()),
    );
    let signer = SigningKey::<sha2::Sha256>::new(key().clone());
    let sig = signer.sign(signing_input.as_bytes());
    format!(
        "{}.{}",
        signing_input,
        URL_SAFE_NO_PAD.encode(sig.to_bytes())
    )
}

pub fn jwks() -> Value {
    let public = RsaPublicKey::from(key());
    json!({"keys":[{
        "kty":"RSA","use":"sig","alg":"RS256","kid":"test-key",
        "n":URL_SAFE_NO_PAD.encode(public.n().to_bytes_be()),
        "e":URL_SAFE_NO_PAD.encode(public.e().to_bytes_be())
    }]})
}

async fn protected(CurrentUser(user): CurrentUser) -> String {
    user.user_id.to_string()
}

async fn api(BearerUser(user): BearerUser) -> String {
    user.user_id.to_string()
}

async fn optional(OptionalUser(user): OptionalUser) -> String {
    user.map_or_else(|| "none".to_owned(), |user| user.user_id.to_string())
}
async fn mcp(McpUser(user): McpUser) -> String {
    user.user_id.to_string()
}

impl Fixture {
    pub async fn new() -> Self {
        Self::new_with_settings(None, OAuthOptions::default(), &["userid:123"]).await
    }

    pub async fn new_with_token_endpoint(token_endpoint: Option<String>) -> Self {
        Self::new_with_settings(token_endpoint, OAuthOptions::default(), &["userid:123"]).await
    }

    pub async fn new_with_oauth_options(options: OAuthOptions) -> Self {
        Self::new_with_settings(None, options, &["userid:123"]).await
    }

    pub async fn new_with_allowed_subjects(subjects: &[&str]) -> Self {
        Self::new_with_settings(None, OAuthOptions::default(), subjects).await
    }

    async fn new_with_settings(
        token_endpoint: Option<String>,
        options: OAuthOptions,
        subjects: &[&str],
    ) -> Self {
        Self::new_with_transport(token_endpoint, options, subjects, None, None).await
    }

    #[cfg(feature = "tailnet")]
    pub async fn with_dialer(issuer: String, dialer: Arc<dyn setec::Dialer>) -> Self {
        Self::new_with_transport(
            None,
            OAuthOptions::default(),
            &["userid:123"],
            Some(issuer),
            Some(dialer),
        )
        .await
    }

    #[cfg(feature = "tailnet")]
    pub async fn with_dialer_and_token_endpoint(
        issuer: String,
        token_endpoint: String,
        dialer: Arc<dyn setec::Dialer>,
    ) -> Self {
        Self::new_with_transport(
            Some(token_endpoint),
            OAuthOptions::default(),
            &["userid:123"],
            Some(issuer),
            Some(dialer),
        )
        .await
    }

    async fn new_with_transport(
        token_endpoint: Option<String>,
        options: OAuthOptions,
        subjects: &[&str],
        issuer_override: Option<String>,
        #[cfg(feature = "tailnet")] dialer: Option<Arc<dyn setec::Dialer>>,
        #[cfg(not(feature = "tailnet"))] _dialer: Option<()>,
    ) -> Self {
        let (pool, guard) = cja::testing::test_db::create_test_database("cja_passport_test_")
            .await
            .unwrap();
        cja_passport::run_migrations(&pool).await.unwrap();
        let server = MockServer::start_async().await;
        let issuer = issuer_override.unwrap_or_else(|| server.base_url());
        let token_endpoint = token_endpoint.unwrap_or_else(|| format!("{issuer}/token"));
        server
            .mock_async(|when, then| {
                when.method(GET).path("/.well-known/openid-configuration");
                then.status(200).json_body(json!({
                    "issuer": issuer,
                    "authorization_endpoint": format!("{issuer}/authorize"),
                    "token_endpoint": token_endpoint,
                    "jwks_uri": format!("{issuer}/jwks"),
                    "response_types_supported": ["code"],
                    "subject_types_supported": ["public"],
                    "id_token_signing_alg_values_supported": ["RS256"],
                    "token_endpoint_auth_methods_supported": ["client_secret_basic"]
                }));
            })
            .await;
        server
            .mock_async(|when, then| {
                when.method(GET).path("/jwks");
                then.status(200).json_body(jwks());
            })
            .await;
        let fake = FakeSetec::new([("attic/passport/test-client", b"secret".as_slice())]);
        let (secrets, refresher) = fake.store().load::<TestSecrets>().await.unwrap();
        let config = PassportConfig::new(
            IssuerUrl::new(issuer.clone()).unwrap(),
            RedirectUrl::new("https://app.example.test/auth/callback".into()).unwrap(),
            ClientId::new("test-client".into()),
            secrets.client.watch(),
            subjects
                .iter()
                .map(|subject| AllowedSubject::new(*subject).unwrap())
                .collect(),
            HashSet::from(["testapp".to_owned()]),
            SessionTimings::default(),
        )
        .unwrap();
        #[cfg(feature = "tailnet")]
        let config = match dialer {
            Some(dialer) => config.dialer(dialer).unwrap(),
            None => config,
        };
        let state = TestState {
            pool,
            key: CookieKey::generate(),
            config,
            oauth: McpOAuthConfig::new(
                url::Url::parse("https://app.example.test/").unwrap(),
                url::Url::parse("https://app.example.test/mcp").unwrap(),
                options,
            )
            .unwrap(),
        };
        let router = Router::new()
            .route("/private", get(protected))
            .route("/api", get(api))
            .route("/optional", get(optional))
            .route("/mcp", get(mcp))
            .merge(passport_router::<TestState>())
            .merge(oauth_router::<TestState>())
            .layer(tower_cookies::CookieManagerLayer::new())
            .with_state(state.clone());
        Self {
            state,
            router,
            server,
            guard,
            fake,
            refresher,
            issuer,
        }
    }

    pub async fn request(
        &self,
        method: &str,
        uri: &str,
        cookie: Option<&str>,
        auth: Option<&str>,
        body: Option<&str>,
    ) -> Response<axum::body::Body> {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(cookie) = cookie {
            builder = builder.header("cookie", cookie);
        }
        if let Some(auth) = auth {
            builder = builder.header("authorization", auth);
        }
        if body.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        self.router
            .clone()
            .oneshot(
                builder
                    .body(axum::body::Body::from(body.unwrap_or_default().to_owned()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    pub async fn form(
        &self,
        uri: &str,
        cookie: Option<&str>,
        fields: &[(&str, &str)],
    ) -> Response<axum::body::Body> {
        let mut serializer = url::form_urlencoded::Serializer::new(String::new());
        serializer.extend_pairs(fields.iter().copied());
        let mut builder = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/x-www-form-urlencoded");
        if let Some(cookie) = cookie {
            builder = builder.header("cookie", cookie);
        }
        self.router
            .clone()
            .oneshot(
                builder
                    .body(axum::body::Body::from(serializer.finish()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    pub async fn start(&self, uri: &str) -> (String, String, String) {
        let response = self.request("GET", uri, None, None, None).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let auth_url = response.headers()["location"].to_str().unwrap().to_owned();
        let cookie = response.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let parsed = url::Url::parse(&auth_url).unwrap();
        let state = parsed
            .query_pairs()
            .find(|(k, _)| k == "state")
            .unwrap()
            .1
            .into_owned();
        (auth_url, cookie, state)
    }

    pub async fn nonce(&self) -> String {
        sqlx::query_scalar(
            "SELECT nonce FROM passport_login_attempts ORDER BY expires_at DESC LIMIT 1",
        )
        .fetch_one(&self.state.pool)
        .await
        .unwrap()
    }

    pub async fn token_mock(&self, id_token: String, refresh: &str) -> httpmock::Mock<'_> {
        self.server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/token")
                    .form_urlencoded_tuple("grant_type", "authorization_code");
                then.status(200)
                    .header("content-type", "application/json")
                    .json_body(json!({
                        "access_token":"0123456789abcdef0123456789abcdef",
                        "token_type":"Bearer", "expires_in":300,
                        "refresh_token":refresh, "id_token":id_token
                    }));
            })
            .await
    }

    pub async fn callback(&self, cookie: &str, state: &str) -> Response<axum::body::Body> {
        let mut uri = url::Url::parse("https://app.example.test/auth/callback").unwrap();
        uri.query_pairs_mut()
            .append_pair("code", "test-code")
            .append_pair("state", state);
        self.request(
            "GET",
            &format!("{}?{}", uri.path(), uri.query().unwrap()),
            Some(cookie),
            None,
            None,
        )
        .await
    }

    pub async fn close(self) {
        self.state.pool.close().await;
        drop(self.guard);
    }
}

pub fn cookie(response: &Response<axum::body::Body>) -> String {
    response.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned()
}

pub fn mint_cookie(id: uuid::Uuid, key: &CookieKey) -> String {
    let mut jar = tower_cookies::cookie::CookieJar::new();
    jar.private_mut(&key.0)
        .add(tower_cookies::cookie::Cookie::new(
            SESSION_COOKIE_NAME,
            id.to_string(),
        ));
    jar.get(SESSION_COOKIE_NAME).unwrap().to_string()
}
