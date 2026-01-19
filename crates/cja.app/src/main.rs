use axum::response::IntoResponse;
use cja::{
    app_state::DbPool,
    color_eyre::{
        self,
        eyre::Context as _,
    },
    server::{
        cookies::CookieKey,
        run_server,
        session::{AppSession, CJASession, Session},
    },
    setup::{setup_sentry, setup_tracing},
};
use maud::html;
use tracing::info;

#[derive(Clone)]
struct AppState {
    db: DbPool,
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

    fn db(&self) -> &DbPool {
        &self.db
    }

    fn cookie_key(&self) -> &CookieKey {
        &self.cookie_key
    }
}

fn main() -> color_eyre::Result<()> {
    // Initialize Sentry for error tracking
    let _sentry_guard = setup_sentry();

    // Create and run the tokio runtime
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()?
        .block_on(async { run_application().await })
}

#[tracing::instrument(err)]
pub async fn setup_db_pool() -> cja::Result<DbPool> {
    use deadpool_postgres::{Config, Runtime};
    use tokio_postgres::NoTls;

    let database_url = std::env::var("DATABASE_URL").wrap_err("DATABASE_URL must be set")?;

    // Parse the database URL and create a deadpool config
    let config = database_url
        .parse::<tokio_postgres::Config>()
        .wrap_err("Failed to parse DATABASE_URL")?;

    let mut cfg = Config::new();
    cfg.dbname = config.get_dbname().map(String::from);
    cfg.host = config.get_hosts().first().map(|h| match h {
        tokio_postgres::config::Host::Tcp(s) => s.clone(),
        tokio_postgres::config::Host::Unix(p) => p.to_string_lossy().into_owned(),
    });
    cfg.port = config.get_ports().first().copied();
    cfg.user = config.get_user().map(String::from);
    cfg.password = config
        .get_password()
        .map(|p| String::from_utf8_lossy(p).into_owned());

    let pool = cfg
        .create_pool(Some(Runtime::Tokio1), NoTls)
        .wrap_err("Failed to create database pool")?;

    // Run migrations using sqlx-cli externally
    // For now, just log that migrations should be run
    tracing::warn!("Migrations should be run via sqlx-cli or refinery");

    Ok(pool)
}

async fn run_application() -> cja::Result<()> {
    // Initialize tracing
    setup_tracing("cja-site")?;

    let app_state = AppState::from_env().await?;

    // Create shutdown token for graceful shutdown
    use cja::jobs::CancellationToken;
    let shutdown_token = CancellationToken::new();

    // Spawn application tasks
    info!("Spawning application tasks");
    let futures = spawn_application_tasks(&app_state, &shutdown_token);

    // Set up signal handlers for graceful shutdown
    let shutdown_handle = tokio::spawn(async move {
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

        shutdown_token.cancel();
    });

    // Wait for all tasks to complete
    let result = futures::future::try_join_all(futures).await;

    // Cancel signal handler if tasks complete first
    shutdown_handle.abort();

    result?;
    Ok(())
}

fn routes(app_state: AppState) -> axum::Router {
    axum::Router::new()
        .route("/", axum::routing::get(root))
        .with_state(app_state)
}

struct SiteSession {
    inner: CJASession,
}

#[async_trait::async_trait]
impl AppSession for SiteSession {
    async fn from_db(pool: &DbPool, session_id: uuid::Uuid) -> cja::Result<Self> {
        let client = pool.get().await?;
        let row = sql_check_macros::query!(
            "SELECT session_id, updated_at, created_at FROM sessions WHERE session_id = $1",
            session_id
        )
        .fetch_one(&*client)
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

    async fn create(pool: &DbPool) -> cja::Result<Self> {
        let client = pool.get().await?;
        let row = sql_check_macros::query!(
            "INSERT INTO sessions DEFAULT VALUES RETURNING session_id, updated_at, created_at"
        )
        .fetch_one(&*client)
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

async fn root(Session(session): Session<SiteSession>) -> impl IntoResponse {
    html! {
        html {
            head {
                title { "CJA Site" }
            }
            body {
                h1 { "Hello, World!" }

                h4 { "Session" }
                p { "Session ID: " (session.session_id()) }
                p { "Created At: " (session.created_at()) }
                p { "Updated At: " (session.updated_at()) }
            }
        }
    }
}

/// Spawn all application background tasks
fn spawn_application_tasks(
    app_state: &AppState,
    #[allow(unused_variables)] shutdown_token: &cja::jobs::CancellationToken,
) -> std::vec::Vec<tokio::task::JoinHandle<std::result::Result<(), cja::color_eyre::Report>>> {
    let mut futures = vec![];

    if is_feature_enabled("SERVER") {
        info!("Server Enabled");
        futures.push(tokio::spawn(run_server(routes(app_state.clone()))));
    } else {
        info!("Server Disabled");
    }

    // Initialize job worker if enabled
    #[cfg(feature = "jobs")]
    if is_feature_enabled("JOBS") {
        use std::time::Duration;

        info!("Jobs Enabled");
        futures.push(tokio::spawn(cja::jobs::worker::job_worker(
            app_state.clone(),
            jobs::Jobs,
            Duration::from_secs(60),
            cja::jobs::DEFAULT_MAX_RETRIES,
            shutdown_token.clone(),
            cja::jobs::DEFAULT_LOCK_TIMEOUT,
        )));
    } else {
        info!("Jobs Disabled");
    }

    #[cfg(not(feature = "jobs"))]
    {
        info!("Jobs feature not compiled in");
    }

    // Initialize cron worker if enabled
    #[cfg(feature = "cron")]
    if is_feature_enabled("CRON") {
        info!("Cron Enabled");
        futures.push(tokio::spawn(cron::run_cron(
            app_state.clone(),
            shutdown_token.clone(),
        )));
    } else {
        info!("Cron Disabled");
    }

    #[cfg(not(feature = "cron"))]
    {
        info!("Cron feature not compiled in");
    }

    info!("All application tasks spawned successfully");
    futures
}

/// Check if a feature is enabled based on environment variables
fn is_feature_enabled(feature: &str) -> bool {
    let env_var_name = format!("{feature}_DISABLED");
    let value = std::env::var(&env_var_name).unwrap_or_else(|_| "false".to_string());

    value != "true"
}

#[cfg(feature = "jobs")]
mod jobs {
    use serde::{Deserialize, Serialize};

    use super::AppState;

    #[derive(Debug, Serialize, Deserialize, Clone)]
    pub struct NoopJob;

    #[async_trait::async_trait]
    impl cja::jobs::Job<AppState> for NoopJob {
        const NAME: &'static str = "NoopJob";

        async fn run(&self, _app_state: AppState) -> cja::Result<()> {
            Ok(())
        }
    }

    /// Demo job that shows how to use the cancellation token for graceful shutdown.
    ///
    /// This job simulates a long-running task by looping and sleeping, checking
    /// the cancellation token periodically to exit early during shutdown.
    #[derive(Debug, Serialize, Deserialize, Clone)]
    pub struct CancellableDemoJob {
        /// Number of iterations to perform (each takes ~1 second)
        pub iterations: u32,
    }

    #[async_trait::async_trait]
    impl cja::jobs::Job<AppState> for CancellableDemoJob {
        const NAME: &'static str = "CancellableDemoJob";

        async fn run(&self, _app_state: AppState) -> cja::Result<()> {
            // Simple implementation without cancellation support
            for i in 0..self.iterations {
                tracing::info!(
                    iteration = i,
                    total = self.iterations,
                    "CancellableDemoJob: processing iteration (no cancellation support)"
                );
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            tracing::info!("CancellableDemoJob: completed all iterations");
            Ok(())
        }

        async fn run_with_cancellation(
            &self,
            _app_state: AppState,
            cancellation_token: cja::jobs::CancellationToken,
        ) -> cja::Result<()> {
            tracing::info!(
                iterations = self.iterations,
                "CancellableDemoJob: starting with cancellation support"
            );

            for i in 0..self.iterations {
                // Check if shutdown was requested
                if cancellation_token.is_cancelled() {
                    tracing::warn!(
                        iteration = i,
                        total = self.iterations,
                        "CancellableDemoJob: cancelled during shutdown, exiting early"
                    );
                    return Err(cja::color_eyre::eyre::eyre!(
                        "Job cancelled at iteration {}/{} during shutdown",
                        i,
                        self.iterations
                    ));
                }

                tracing::info!(
                    iteration = i,
                    total = self.iterations,
                    "CancellableDemoJob: processing iteration"
                );

                // Simulate work by sleeping for 1 second
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }

            tracing::info!(
                iterations = self.iterations,
                "CancellableDemoJob: completed all iterations successfully"
            );
            Ok(())
        }
    }

    cja::impl_job_registry!(AppState, NoopJob, CancellableDemoJob);
}

#[cfg(feature = "cron")]
mod cron {
    use chrono_tz::US::Eastern;
    use cja::cron::{CancellationToken, CronRegistry, Worker};
    use std::time::Duration;

    #[cfg(feature = "jobs")]
    use crate::jobs::NoopJob;

    use super::AppState;

    pub(crate) async fn run_cron(
        app_state: AppState,
        shutdown_token: CancellationToken,
    ) -> cja::Result<()> {
        Ok(
            Worker::new_with_timezone(app_state, cron_registry(), Eastern, Duration::from_secs(60))
                .run(shutdown_token)
                .await?,
        )
    }

    fn cron_registry() -> cja::cron::CronRegistry<AppState> {
        #[cfg_attr(not(feature = "jobs"), allow(unused_mut))]
        let mut registry = CronRegistry::new();
        #[cfg(feature = "jobs")]
        registry.register_job(NoopJob, Duration::from_secs(60));
        registry
    }
}
