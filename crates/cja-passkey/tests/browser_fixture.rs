//! Real Chromium `WebAuthn` fixture. Run explicitly with `-- --ignored`.

use std::process::Command;

use axum::extract::{Json, State};
use axum::routing::{get, post};
use cja::app_state::AppState;
use cja::server::session::{AppSession, Session};
use cja_passkey::config::HasPasskeyConfig;
use cja_passkey::{CurrentUser, PasskeyConfig, PasskeySession};
use sqlx::Row;
use sqlx::postgres::PgPoolOptions;
use url::Url;
use uuid::Uuid;

fn database_url(name: &str) -> String {
    let base = std::env::var("DATABASE_URL").expect("DATABASE_URL is required");
    let (prefix, _) = base
        .rsplit_once('/')
        .expect("DATABASE_URL must name a database");
    format!("{prefix}/{name}")
}

struct TestDbGuard(String);

impl Drop for TestDbGuard {
    fn drop(&mut self) {
        let name = self.0.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("cleanup runtime");
            rt.block_on(async {
                let pool = PgPoolOptions::new()
                    .max_connections(1)
                    .connect(&database_url("postgres"))
                    .await
                    .expect("connect for cleanup");
                sqlx::query("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = $1 AND pid <> pg_backend_pid()")
                    .bind(&name)
                    .execute(&pool)
                    .await
                    .expect("terminate fixture connections");
                sqlx::query(&format!("DROP DATABASE IF EXISTS \"{name}\""))
                    .execute(&pool)
                    .await
                    .expect("drop fixture database");
            });
        })
        .join()
        .expect("cleanup thread");
    }
}

#[derive(Clone)]
struct TestState {
    db: sqlx::PgPool,
    cookie_key: cja::server::cookies::CookieKey,
    passkey: PasskeyConfig,
}

impl AppState for TestState {
    fn version(&self) -> &'static str {
        "test"
    }
    fn db(&self) -> &sqlx::PgPool {
        &self.db
    }
    fn cookie_key(&self) -> &cja::server::cookies::CookieKey {
        &self.cookie_key
    }
}

impl HasPasskeyConfig for TestState {
    fn passkey_config(&self) -> &PasskeyConfig {
        &self.passkey
    }
}

async fn me(user: CurrentUser) -> String {
    user.0.username
}

async fn state_snapshot(
    State(state): State<TestState>,
    Session(session): Session<PasskeySession>,
) -> Json<serde_json::Value> {
    let row = sqlx::query("SELECT user_id, challenge_state FROM sessions WHERE session_id = $1")
        .bind(session.session_id())
        .fetch_one(&state.db)
        .await
        .unwrap();
    let user_id: Option<Uuid> = row.try_get("user_id").unwrap();
    let challenge: Option<serde_json::Value> = row.try_get("challenge_state").unwrap();
    let credentials = sqlx::query(
        "SELECT u.username, u.user_id, c.credential_id_pk, c.credential_json, c.last_used_at \
         FROM passkey_users u JOIN passkey_credentials c USING (user_id) ORDER BY u.username",
    )
    .fetch_all(&state.db)
    .await
    .unwrap()
    .into_iter()
    .map(|r| serde_json::json!({
        "username": r.try_get::<String, _>("username").unwrap(),
        "user_id": r.try_get::<Uuid, _>("user_id").unwrap(),
        "credential_id_pk": r.try_get::<Uuid, _>("credential_id_pk").unwrap(),
        "credential_json": r.try_get::<serde_json::Value, _>("credential_json").unwrap(),
        "last_used_at": r.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("last_used_at").unwrap(),
    }))
    .collect::<Vec<_>>();
    Json(serde_json::json!({
        "session_id": session.session_id(),
        "user_id": user_id,
        "challenge": challenge,
        "credentials": credentials
    }))
}

#[derive(serde::Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Mutation {
    DeleteUser {
        username: String,
    },
    ChangeOwner {
        credential_owner: String,
        new_owner: String,
    },
}

async fn mutate(
    State(state): State<TestState>,
    Json(action): Json<Mutation>,
) -> axum::http::StatusCode {
    match action {
        Mutation::DeleteUser { username } => {
            let row = sqlx::query("SELECT user_id FROM passkey_users WHERE username = $1")
                .bind(username)
                .fetch_one(&state.db)
                .await
                .unwrap();
            let user_id: Uuid = row.try_get("user_id").unwrap();
            sqlx::query("UPDATE sessions SET user_id = NULL WHERE user_id = $1")
                .bind(user_id)
                .execute(&state.db)
                .await
                .unwrap();
            sqlx::query("DELETE FROM passkey_users WHERE user_id = $1")
                .bind(user_id)
                .execute(&state.db)
                .await
                .unwrap();
        }
        Mutation::ChangeOwner {
            credential_owner,
            new_owner,
        } => {
            sqlx::query(
                "UPDATE passkey_credentials SET user_id = \
                 (SELECT user_id FROM passkey_users WHERE username = $1) \
                 WHERE user_id = (SELECT user_id FROM passkey_users WHERE username = $2)",
            )
            .bind(new_owner)
            .bind(credential_owner)
            .execute(&state.db)
            .await
            .unwrap();
        }
    }
    axum::http::StatusCode::NO_CONTENT
}

async fn page() -> axum::response::Html<&'static str> {
    axum::response::Html(
        r#"<!doctype html><html><body>
<button id="register" onclick="run('register')">Register</button>
<button id="login" onclick="run('login')">Login</button>
<button id="named" onclick="run('loginWithUsername')">Named login</button>
<script src="/passkey-client.js"></script>
<script>
const client = cjaPasskey.createPasskeyClient();
window.run = async (method) => {
  try {
    const result = method === 'register'
      ? await client.register({username: window.username})
      : method === 'loginWithUsername'
        ? await client.loginWithUsername({username: window.username})
        : await client.login();
    window.outcome = {ok: true, result};
  } catch (e) { window.outcome = {ok: false, error: String(e)}; }
};
</script></body></html>"#,
    )
}

#[tokio::test]
#[ignore = "requires PostgreSQL, pnpm, and Chromium"]
async fn discoverable_browser_round_trip() {
    assert!(
        cja_passkey::passkey_js().contains("loginWithUsername")
            && cja_passkey::passkey_js().contains("/auth/discoverable/start"),
        "embedded JS is a placeholder or lacks the discoverable API"
    );
    let name = format!("cja_passkey_browser_{}", Uuid::new_v4().simple());
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url("postgres"))
        .await
        .expect("connect master database");
    sqlx::query(&format!("CREATE DATABASE \"{name}\""))
        .execute(&admin)
        .await
        .expect("create fixture database");
    admin.close().await;
    let guard = TestDbGuard(name.clone());
    let db = PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url(&name))
        .await
        .expect("connect fixture database");
    sqlx::migrate!("../cja/migrations")
        .run(&db)
        .await
        .expect("cja migrations");
    cja_passkey::run_migrations(&db)
        .await
        .expect("passkey migrations");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let origin = format!("http://localhost:{}", address.port());
    let state = TestState {
        db: db.clone(),
        cookie_key: cja::server::cookies::CookieKey::generate(),
        passkey: PasskeyConfig::new("localhost", &Url::parse(&origin).unwrap()).unwrap(),
    };
    let app = axum::Router::new()
        .merge(cja_passkey::passkey_router::<TestState>())
        .route("/", get(page))
        .route("/me", get(me))
        .route("/test/state", get(state_snapshot))
        .route("/test/mutate", post(mutate))
        .layer(tower_cookies::CookieManagerLayer::new())
        .with_state(state);
    let server = tokio::spawn(async move { axum::serve(listener, app).await });

    let e2e = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("e2e");
    let output = tokio::task::spawn_blocking(move || {
        Command::new("pnpm")
            .args([
                "exec",
                "playwright",
                "test",
                "discoverable.spec.ts",
                "--reporter=line",
                "--workers=1",
            ])
            .current_dir(e2e)
            .env("CJA_PASSKEY_TEST_BASE_URL", origin)
            .output()
    })
    .await
    .unwrap()
    .expect("pnpm/Playwright must be installed");
    server.abort();
    let _ = server.await;
    db.close().await;
    drop(guard);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "Playwright failed:\n{stdout}\n{stderr}"
    );
    assert!(
        stdout.contains("1 passed"),
        "Playwright ran no passing browser test:\n{stdout}\n{stderr}"
    );
}
