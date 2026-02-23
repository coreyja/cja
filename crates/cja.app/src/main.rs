use cja::{
    color_eyre::{
        self,
        eyre::{Context as _, eyre},
    },
    server::{
        cookies::CookieKey,
        run_server,
        session::{AppSession, CJASession},
    },
    setup::{setup_sentry, setup_tracing},
    tasks::NamedTask,
};
use sqlx::{PgPool, postgres::PgPoolOptions};
use tracing::info;

mod routes;
mod templates;

#[derive(Clone)]
pub(crate) struct AppState {
    db: sqlx::PgPool,
    cookie_key: CookieKey,
}

impl AppState {
    async fn from_env() -> color_eyre::Result<Self> {
        let db = setup_db_pool().await?;
        let cookie_key = CookieKey::from_env_or_generate()?;

        Ok(Self { db, cookie_key })
    }
}

impl cja::app_state::AppState for AppState {
    fn version(&self) -> &'static str {
        "unknown"
    }

    fn db(&self) -> &sqlx::PgPool {
        &self.db
    }

    fn cookie_key(&self) -> &CookieKey {
        &self.cookie_key
    }
}

fn main() -> color_eyre::Result<()> {
    let _sentry_guard = setup_sentry();

    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()?
        .block_on(async { run_application().await })
}

#[tracing::instrument(err)]
pub async fn setup_db_pool() -> cja::Result<PgPool> {
    const MIGRATION_LOCK_ID: i64 = 0xDB_DB_DB_DB_DB_DB_DB;

    let database_url = std::env::var("DATABASE_URL").wrap_err("DATABASE_URL must be set")?;
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await?;

    sqlx::query!("SELECT pg_advisory_lock($1)", MIGRATION_LOCK_ID)
        .execute(&pool)
        .await?;

    sqlx::migrate!().run(&pool).await?;

    let unlock_result = sqlx::query!("SELECT pg_advisory_unlock($1)", MIGRATION_LOCK_ID)
        .fetch_one(&pool)
        .await?
        .pg_advisory_unlock;

    match unlock_result {
        Some(b) => {
            if b {
                tracing::info!("Migration lock unlocked");
            } else {
                tracing::info!("Failed to unlock migration lock");
            }
        }
        None => return Err(eyre!("Failed to unlock migration lock")),
    }

    Ok(pool)
}

async fn run_application() -> cja::Result<()> {
    setup_tracing("cja-site")?;

    let app_state = AppState::from_env().await?;

    info!("Spawning application tasks");
    let mut tasks = vec![];

    if is_feature_enabled("SERVER") {
        info!("Server Enabled");
        tasks.push(NamedTask::spawn(
            "server",
            run_server(routes::router(app_state.clone())),
        ));
    } else {
        info!("Server Disabled");
    }

    tasks.push(NamedTask::spawn("signal-handler", async move {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to create SIGTERM handler");
        let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .expect("Failed to create SIGINT handler");

        tokio::select! {
            _ = sigterm.recv() => {
                info!("Received SIGTERM, initiating graceful shutdown");
            }
            _ = sigint.recv() => {
                info!("Received SIGINT, initiating graceful shutdown");
            }
        }

        Ok(())
    }));

    cja::tasks::wait_for_first_error(tasks).await
}

#[allow(dead_code)]
pub(crate) struct SiteSession {
    inner: CJASession,
}

#[async_trait::async_trait]
impl AppSession for SiteSession {
    async fn from_db(pool: &sqlx::PgPool, session_id: uuid::Uuid) -> cja::Result<Self> {
        let row = sqlx::query!(
            "SELECT session_id, created_at, updated_at FROM sessions WHERE session_id = $1",
            session_id
        )
        .fetch_one(pool)
        .await?;

        let session = SiteSession {
            inner: CJASession {
                session_id: row.session_id,
                created_at: row.created_at,
                updated_at: row.updated_at,
            },
        };

        Ok(session)
    }

    async fn create(pool: &sqlx::PgPool) -> cja::Result<Self> {
        let row = sqlx::query!(
            "INSERT INTO sessions DEFAULT VALUES RETURNING session_id, created_at, updated_at",
        )
        .fetch_one(pool)
        .await?;

        let inner = CJASession {
            session_id: row.session_id,
            created_at: row.created_at,
            updated_at: row.updated_at,
        };

        Ok(Self { inner })
    }

    fn from_inner(inner: CJASession) -> Self {
        Self { inner }
    }

    fn inner(&self) -> &CJASession {
        &self.inner
    }
}

fn is_feature_enabled(feature: &str) -> bool {
    let env_var_name = format!("{feature}_DISABLED");
    let value = std::env::var(&env_var_name).unwrap_or_else(|_| "false".to_string());

    value != "true"
}
