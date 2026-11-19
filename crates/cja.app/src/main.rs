use cja::{
    color_eyre::{
        self,
        eyre::{Context as _, eyre},
    },
    server::{cookies::CookieKey, run_server_until},
    setup::TracingConfig,
    tasks::{ShutdownBudget, Supervisor},
};
use sqlx::{PgPool, postgres::PgPoolOptions};
use tracing::info;

mod heartbeat;
mod routes;
mod templates;
cja::include_assets!(site_assets);

#[derive(Clone)]
pub(crate) struct AppState {
    db: sqlx::PgPool,
    cookie_key: CookieKey,
    heartbeat_schedule: cja::cron::Schedule,
}

impl AppState {
    async fn from_env(heartbeat_schedule: cja::cron::Schedule) -> color_eyre::Result<Self> {
        let db = setup_db_pool().await?;
        let cookie_key = CookieKey::from_env_or_generate()?;

        Ok(Self {
            db,
            cookie_key,
            heartbeat_schedule,
        })
    }
}

cja::impl_job_registry!(AppState, heartbeat::HeartbeatJob);

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

    // Advisory locks belong to a session: take and release this one on the
    // same connection, while the migrators use other pool connections.
    let mut lock_conn = pool.acquire().await?;
    sqlx::query!("SELECT pg_advisory_lock($1)", MIGRATION_LOCK_ID)
        .execute(&mut *lock_conn)
        .await?;

    migrate(&pool).await?;

    let unlock_result = sqlx::query!("SELECT pg_advisory_unlock($1)", MIGRATION_LOCK_ID)
        .fetch_one(&mut *lock_conn)
        .await?
        .pg_advisory_unlock;
    drop(lock_conn);

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

/// Runs cja's framework migrations, then this app's own. Both migrators share
/// `_sqlx_migrations`, so each ignores versions it does not know.
pub async fn migrate(pool: &PgPool) -> cja::Result<()> {
    cja::db::run_migrations(pool).await?;

    let mut migrations = sqlx::migrate!();
    migrations.set_ignore_missing(true);
    migrations.run(pool).await?;

    Ok(())
}

async fn run_application() -> cja::Result<()> {
    let telemetry_handle = TracingConfig::new("cja-site")
        .version(env!("CARGO_PKG_VERSION"))
        .init()?;

    // Hold the outcome while telemetry flushes, so an early `?` inside
    // `run_tasks` (a boot failure) still gets its last events out.
    let result = run_tasks().await;
    telemetry_handle.shutdown().await?;
    result
}

async fn run_tasks() -> cja::Result<()> {
    let cron_registry = heartbeat::cron_registry()?;
    let app_state = AppState::from_env(heartbeat::schedule(&cron_registry)).await?;

    // This process has the complete cron and job registries; emit before the
    // registry moves into the worker. Other roles must not send partial snapshots.
    cja::posthog_manifest::emit_boot_manifest::<Jobs, AppState>(
        &app_state,
        &cja::posthog_manifest::BootManifest::new(
            "cja-site",
            &cron_registry,
            &heartbeat::TIMEZONE,
            &[],
            &[],
        ),
    )?;

    info!("Spawning application tasks");
    let mut supervisor = Supervisor::new(ShutdownBudget::from_env())?;

    if is_feature_enabled("SERVER") {
        info!("Server Enabled");
        let router = routes::router(app_state.clone())?;
        supervisor.spawn(
            "server",
            run_server_until(router, supervisor.shutdown_token().cancelled_owned()),
        );
    } else {
        info!("Server Disabled");
    }

    if is_feature_enabled("JOBS") {
        info!("Jobs Enabled");
        supervisor.spawn(
            "jobs",
            cja::jobs::worker::job_worker(
                app_state.clone(),
                Jobs,
                std::time::Duration::from_secs(5),
                cja::jobs::DEFAULT_MAX_RETRIES,
                supervisor.shutdown_token(),
                cja::jobs::worker::JobWorkerConfig::default(),
            ),
        );
    } else {
        info!("Jobs Disabled");
    }

    if is_feature_enabled("CRON") {
        info!("Cron Enabled");
        let worker = cja::cron::Worker::new_with_timezone(
            app_state.clone(),
            cron_registry,
            heartbeat::TIMEZONE.clone(),
            std::time::Duration::from_secs(30),
        );
        let token = supervisor.shutdown_token();
        supervisor.spawn("cron", async move {
            worker.run(token).await;
            Ok(())
        });
    } else {
        info!("Cron Disabled");
    }

    supervisor.run().await
}

fn is_feature_enabled(feature: &str) -> bool {
    let env_var_name = format!("{feature}_DISABLED");
    let value = std::env::var(&env_var_name).unwrap_or_else(|_| "false".to_string());

    value != "true"
}
