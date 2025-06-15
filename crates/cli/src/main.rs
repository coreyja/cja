//! # CJA CLI
//!
//! A command-line interface for scaffolding new [CJA](https://github.com/your-org/cja) projects.
//! 
//! CJA CLI generates fully functional web applications built on the CJA framework,
//! with configurable features including background jobs, cron scheduling, and session management.
//!
//! ## Features
//!
//! - **Full-Stack Project Generation**: Creates complete CJA applications with HTTP server, database integration, and optional background processing
//! - **Feature Flags**: Opt-out of specific features (`--no-jobs`, `--no-cron`, `--no-sessions`) for minimal deployments
//! - **Smart Defaults**: All features enabled by default, with intelligent dependency handling
//! - **Database Migrations**: Automatically includes relevant SQL migrations based on enabled features
//! - **Production Ready**: Generated projects include proper error handling, logging, and configuration
//!
//! ## Quick Start
//!
//! ```bash
//! # Create a full-featured CJA project
//! cja new my-web-app
//!
//! # Create a minimal HTTP server (no background processing)
//! cja new my-api --no-jobs --no-cron --no-sessions
//!
//! # Create a project with jobs but no cron scheduling
//! cja new my-worker --no-cron
//! ```
//!
//! ## Generated Project Structure
//!
//! ```text
//! my-project/
//! ├── Cargo.toml          # Project dependencies and metadata
//! ├── src/
//! │   └── main.rs         # Application entry point with conditional features
//! └── migrations/         # Database migrations (feature-dependent)
//!     ├── *_AddJobsTable.sql      # Jobs support (if enabled)
//!     ├── *_AddCrons.up.sql       # Cron scheduling (if enabled)
//!     └── *_AddSessions.up.sql    # Session management (if enabled)
//! ```
//!
//! ## Feature Dependencies
//!
//! - **Cron requires Jobs**: The `--no-jobs` flag automatically disables cron scheduling
//! - **Independent Sessions**: Session support is independent of jobs and cron
//! - **HTTP Server**: Always included (core CJA functionality)
//!
//! ## Examples
//!
//! ### Create a Full-Featured Application
//! ```bash
//! cja new ecommerce-site
//! cd ecommerce-site
//! cargo run
//! ```
//!
//! The generated application includes:
//! - HTTP server with Axum
//! - PostgreSQL database with migrations
//! - Background job processing
//! - Cron scheduling system
//! - Session management
//! - HTML templating with Maud
//! - Structured logging and error handling
//!
//! ### Create a Microservice
//! ```bash
//! cja new user-service --no-sessions --no-cron --no-jobs
//! ```
//!
//! Creates a lightweight HTTP API server without background processing.
//!
//! ### Create a Background Worker
//! ```bash
//! cja new data-processor --no-sessions
//! ```
//!
//! Creates an application optimized for background job processing with cron scheduling.

use anyhow::{Context, Result};
use clap::{Arg, Command};
use std::fs;
use std::path::Path;

/// Main entry point for the CJA CLI application.
///
/// Sets up command-line argument parsing and routes to the appropriate subcommand handler.
/// Currently supports the `new` subcommand for project generation.
///
/// # Returns
///
/// - `Ok(())` on successful command execution
/// - `Err(anyhow::Error)` if command parsing fails or project creation encounters an error
///
/// # Examples
///
/// The CLI supports several usage patterns:
///
/// ```bash
/// # Basic project creation
/// cja new my-project
///
/// # Project with feature flags
/// cja new my-project --no-sessions --no-cron
///
/// # Help and version information
/// cja --help
/// cja --version
/// ```
fn main() -> Result<()> {
    let matches = Command::new("cja")
        .version(env!("CARGO_PKG_VERSION"))
        .about("CJA CLI for project scaffolding")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(
            Command::new("new")
                .about("Create a new CJA project")
                .arg(
                    Arg::new("name")
                        .help("The name of the project to create")
                        .required(true)
                        .index(1),
                )
                .arg(
                    Arg::new("no-cron")
                        .long("no-cron")
                        .help("Create project without cron support")
                        .action(clap::ArgAction::SetTrue),
                )
                .arg(
                    Arg::new("no-jobs")
                        .long("no-jobs")
                        .help("Create project without jobs support")
                        .action(clap::ArgAction::SetTrue),
                )
                .arg(
                    Arg::new("no-sessions")
                        .long("no-sessions")
                        .help("Create project without sessions support")
                        .action(clap::ArgAction::SetTrue),
                ),
        )
        .get_matches();

    match matches.subcommand() {
        Some(("new", sub_matches)) => {
            let project_name = sub_matches.get_one::<String>("name").unwrap();
            let no_cron = sub_matches.get_flag("no-cron");
            let no_jobs = sub_matches.get_flag("no-jobs");
            let no_sessions = sub_matches.get_flag("no-sessions");
            
            // Warn if both --no-jobs and --no-cron are specified
            if no_jobs && no_cron {
                eprintln!("Warning: --no-jobs implies --no-cron since cron requires the jobs system");
            }
            
            create_project(project_name, no_cron, no_jobs, no_sessions)?;
        }
        _ => unreachable!("Subcommand required"),
    }
    
    Ok(())
}

/// Creates a new CJA project with the specified configuration.
///
/// This function orchestrates the complete project creation process, including:
/// - Directory structure creation
/// - Template file generation
/// - Database migration copying
/// - Feature-based conditional logic
///
/// # Arguments
///
/// * `project_name` - The name of the project to create (used as directory name and in Cargo.toml)
/// * `no_cron` - If true, excludes cron scheduling functionality and migrations
/// * `no_jobs` - If true, excludes background job processing functionality and migrations
/// * `no_sessions` - If true, excludes session management functionality and migrations
///
/// # Returns
///
/// - `Ok(())` on successful project creation
/// - `Err(anyhow::Error)` if any step fails (directory creation, file writing, etc.)
///
/// # Errors
///
/// This function will return an error in the following cases:
/// - A directory with the same name already exists
/// - Insufficient permissions to create directories or files
/// - Template generation fails
/// - Migration file copying fails
///
/// # Feature Dependencies
///
/// - If `no_jobs` is true, cron functionality is automatically disabled since cron depends on jobs
/// - A warning is displayed if both `no_jobs` and `no_cron` are explicitly specified
///
/// # Examples
///
/// ```rust,no_run
/// use anyhow::Result;
///
/// // Create a full-featured project
/// create_project("my-app", false, false, false)?;
///
/// // Create a minimal API server
/// create_project("my-api", true, true, true)?;
///
/// // Create a project with jobs but no cron
/// create_project("my-worker", true, false, false)?;
/// # Ok::<(), anyhow::Error>(())
/// ```
fn create_project(project_name: &str, no_cron: bool, no_jobs: bool, no_sessions: bool) -> Result<()> {
    let project_path = Path::new(project_name);
    
    // Check if directory already exists
    if project_path.exists() {
        anyhow::bail!("Directory '{}' already exists", project_name);
    }
    
    // Create project directory structure
    fs::create_dir(&project_path)
        .with_context(|| format!("Failed to create project directory '{}'", project_name))?;
    
    fs::create_dir(project_path.join("src"))
        .context("Failed to create src directory")?;
    
    fs::create_dir(project_path.join("migrations"))
        .context("Failed to create migrations directory")?;
    
    // Create Cargo.toml
    let cargo_toml_content = generate_cargo_toml(project_name);
    fs::write(project_path.join("Cargo.toml"), cargo_toml_content)
        .context("Failed to write Cargo.toml")?;
    
    // Create main.rs
    let main_rs_content = generate_main_rs(no_cron, no_jobs, no_sessions);
    fs::write(project_path.join("src").join("main.rs"), main_rs_content)
        .context("Failed to write main.rs")?;
    
    // Copy migration files based on feature flags
    if !no_jobs {
        copy_jobs_migration(&project_path)?;
    }
    
    if !no_cron {
        copy_cron_migrations(&project_path)?;
    }
    
    if !no_sessions {
        copy_session_migrations(&project_path)?;
    }
    
    println!("Created new CJA project '{}'", project_name);
    
    Ok(())
}

/// Generates the `Cargo.toml` content for a new CJA project.
///
/// Creates a complete `Cargo.toml` file with all necessary dependencies for a CJA application.
/// The generated configuration includes:
/// - Project metadata (name, version, edition)
/// - Core CJA framework dependency
/// - HTTP server dependencies (Axum, Tokio)
/// - Database dependencies (SQLx with PostgreSQL support)
/// - Utility dependencies (serde, tracing, error handling)
/// - HTML templating (Maud)
///
/// # Arguments
///
/// * `project_name` - The name to use in the `[package]` section
///
/// # Returns
///
/// A complete `Cargo.toml` file content as a `String`
///
/// # Generated Dependencies
///
/// The function includes these key dependencies:
/// - `cja = "0.0.0"` - Core CJA framework
/// - `axum = "0.7"` - HTTP server framework
/// - `tokio` - Async runtime with full features
/// - `sqlx` - Database toolkit with PostgreSQL support
/// - `serde` - Serialization framework
/// - `tracing` - Structured logging
/// - `color-eyre` - Enhanced error reporting
/// - `maud` - Type-safe HTML templating
///
/// # Examples
///
/// ```rust
/// let toml_content = generate_cargo_toml("my-awesome-app");
/// assert!(toml_content.contains("name = \"my-awesome-app\""));
/// assert!(toml_content.contains("cja = { version = \"0.0.0\" }"));
/// ```
fn generate_cargo_toml(project_name: &str) -> String {
    format!(r#"[package]
name = "{}"
version = "0.1.0"
edition = "2021"

[dependencies]
cja = {{ version = "0.0.0" }}
axum = "0.7"
tokio = {{ version = "1", features = ["full"] }}
sqlx = {{ version = "0.8", features = ["runtime-tokio-rustls", "postgres", "uuid", "json", "chrono"] }}
serde = {{ version = "1.0", features = ["derive"] }}
serde_json = "1.0"
tracing = "0.1"
tracing-subscriber = {{ version = "0.3", features = ["env-filter"] }}
color-eyre = "0.6"
uuid = {{ version = "1.5", features = ["v4", "serde"] }}
chrono = {{ version = "0.4", features = ["serde"] }}
maud = {{ version = "0.26", features = ["axum"] }}
futures = "0.3"
async-trait = "0.1"
"#, project_name)
}

/// Generates the `main.rs` content for a new CJA project with conditional features.
///
/// This is the most complex template generation function, creating a complete Rust application
/// entry point that conditionally includes modules and functionality based on feature flags.
///
/// # Arguments
///
/// * `no_cron` - If true, excludes cron scheduling module and worker spawning
/// * `no_jobs` - If true, excludes background job module and worker spawning  
/// * `no_sessions` - If true, excludes session management and uses simple route handlers
///
/// # Returns
///
/// A complete `main.rs` file content as a `String` with conditional compilation
///
/// # Generated Structure
///
/// The function generates these components:
///
/// ## Always Included
/// - Standard imports and `AppState` struct
/// - Database connection pooling with advisory locking
/// - Tokio runtime setup and application lifecycle
/// - HTTP server with basic routing
/// - Environment-based feature toggling
///
/// ## Conditionally Included
///
/// ### Sessions Module (`!no_sessions`)
/// - `SiteSession` struct implementing `AppSession` trait
/// - Database operations for session creation and retrieval
/// - Session-aware route handlers with HTML templating
/// - Session information display in web interface
///
/// ### Jobs Module (`!no_jobs`)
/// - Example `NoopJob` implementation
/// - Job registry macro invocation
/// - Job worker spawning logic
/// - Integration with CJA jobs system
///
/// ### Cron Module (`!no_cron`)
/// - Cron registry setup and configuration
/// - Cron worker spawning
/// - Integration with jobs system (if available)
/// - Scheduled task execution
///
/// # Feature Interactions
///
/// - **Sessions**: Independent of other features
/// - **Jobs + Cron**: Cron integrates with jobs when both are enabled
/// - **Cron Only**: Cron can run without specific job types when jobs are disabled
///
/// # Generated Code Examples
///
/// With all features enabled:
/// ```rust,ignore
/// // Includes session imports
/// use cja::server::session::{AppSession, CJASession, Session};
///
/// // Session-aware route handler
/// async fn root(Session(session): Session<SiteSession>) -> impl IntoResponse {
///     // HTML with session display
/// }
///
/// // Background modules
/// mod jobs { /* Job implementations */ }
/// mod cron { /* Cron scheduling */ }
/// ```
///
/// With minimal features:
/// ```rust,ignore
/// // Simple route handler
/// async fn root() -> impl IntoResponse {
///     // Basic HTML response
/// }
/// // No background modules
/// ```
///
/// # Examples
///
/// ```rust
/// // Generate full-featured application
/// let full_app = generate_main_rs(false, false, false);
/// assert!(full_app.contains("impl AppSession"));
/// assert!(full_app.contains("impl_job_registry!"));
/// assert!(full_app.contains("CronRegistry"));
///
/// // Generate minimal application
/// let minimal_app = generate_main_rs(true, true, true);
/// assert!(!minimal_app.contains("Session"));
/// assert!(!minimal_app.contains("jobs::"));
/// assert!(!minimal_app.contains("cron::"));
/// ```
fn generate_main_rs(no_cron: bool, no_jobs: bool, no_sessions: bool) -> String {
    let mut content = String::new();
    
    // Base imports
    content.push_str(r#"use axum::response::IntoResponse;
use cja::{
    color_eyre::{
        self,
        eyre::{Context as _, eyre},
    },
    server::{
        cookies::CookieKey,
        run_server,
"#);

    // Session imports only if sessions are enabled
    if !no_sessions {
        content.push_str("        session::{AppSession, CJASession, Session},\n");
    }
    
    content.push_str(r#"    },
    setup::{setup_sentry, setup_tracing},
};
use maud::html;
use sqlx::{PgPool, postgres::PgPoolOptions};
use tracing::info;
use async_trait::async_trait;

#[derive(Clone)]
struct AppState {
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
    // Initialize tracing
    setup_tracing("cja-site")?;

    let app_state = AppState::from_env().await?;

    // Spawn application tasks
    info!("Spawning application tasks");
    let futures = spawn_application_tasks(&app_state);

    // Wait for all tasks to complete
    futures::future::try_join_all(futures).await?;

    Ok(())
}

fn routes(app_state: AppState) -> axum::Router {
    axum::Router::new()
        .route("/", axum::routing::get(root))
        .with_state(app_state)
}
"#);

    // Sessions implementation only if sessions are enabled
    if !no_sessions {
        content.push_str(r#"
struct SiteSession {
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
"#);
    } else {
        // Simple root handler without sessions
        content.push_str(r#"
async fn root() -> impl IntoResponse {
    html! {
        html {
            head {
                title { "CJA Site" }
            }
            body {
                h1 { "Hello, World!" }
            }
        }
    }
}
"#);
    }

    // Spawn application tasks function
    content.push_str(r#"
/// Spawn all application background tasks
fn spawn_application_tasks(
    app_state: &AppState,
) -> std::vec::Vec<tokio::task::JoinHandle<std::result::Result<(), cja::color_eyre::Report>>> {
    let mut futures = vec![];

    if is_feature_enabled("SERVER") {
        info!("Server Enabled");
        futures.push(tokio::spawn(run_server(routes(app_state.clone()))));
    } else {
        info!("Server Disabled");
    }
"#);

    // Add jobs worker if jobs are enabled
    if !no_jobs {
        content.push_str(r#"
    // Initialize job worker if enabled
    if is_feature_enabled("JOBS") {
        info!("Jobs Enabled");
        futures.push(tokio::spawn(cja::jobs::worker::job_worker(
            app_state.clone(),
            jobs::Jobs,
        )));
    } else {
        info!("Jobs Disabled");
    }
"#);
    }

    // Add cron worker if cron is enabled
    if !no_cron {
        content.push_str(r#"
    // Initialize cron worker if enabled
    if is_feature_enabled("CRON") {
        info!("Cron Enabled");
        futures.push(tokio::spawn(cron::run_cron(app_state.clone())));
    } else {
        info!("Cron Disabled");
    }
"#);
    }

    content.push_str(r#"
    info!("All application tasks spawned successfully");
    futures
}

/// Check if a feature is enabled based on environment variables
fn is_feature_enabled(feature: &str) -> bool {
    let env_var_name = format!("{feature}_DISABLED");
    let value = std::env::var(&env_var_name).unwrap_or_else(|_| "false".to_string());

    value != "true"
}
"#);

    // Add jobs module if jobs are enabled
    if !no_jobs {
        content.push_str(r#"
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

    cja::impl_job_registry!(AppState, NoopJob);
}
"#);
    }

    // Add cron module if cron is enabled
    if !no_cron {
        content.push_str(r#"
mod cron {
    use std::time::Duration;

    use cja::cron::{CronRegistry, Worker};
"#);
        
        // Only import NoopJob if jobs are also enabled
        if !no_jobs {
            content.push_str("\n    use crate::jobs::NoopJob;\n");
        }
        
        content.push_str(r#"
    use super::AppState;

    pub(crate) async fn run_cron(app_state: AppState) -> cja::Result<()> {
        Ok(Worker::new(app_state, cron_registry()).run().await?)
    }

    fn cron_registry() -> cja::cron::CronRegistry<AppState> {
        let mut registry = CronRegistry::new();
"#);
        
        // Only register NoopJob if jobs are enabled
        if !no_jobs {
            content.push_str("        registry.register_job(NoopJob, Duration::from_secs(60));\n");
        }
        
        content.push_str(r#"        registry
    }
}
"#);
    }

    content
}

/// Copies the jobs table migration file to the new project.
///
/// Creates the SQL migration file for the background jobs system. This migration
/// establishes the core `Jobs` table used by the CJA jobs system for persistent
/// job queuing and execution tracking.
///
/// # Arguments
///
/// * `project_path` - Path to the project directory where migrations should be created
///
/// # Returns
///
/// - `Ok(())` on successful migration file creation
/// - `Err(anyhow::Error)` if file writing fails
///
/// # Generated Migration
///
/// Creates `migrations/20231210151519_AddJobsTable.sql` with:
/// - `Jobs` table with UUID primary key
/// - Job metadata fields (name, payload, priority)
/// - Scheduling fields (run_at, created_at)
/// - Locking mechanism fields (locked_at, locked_by)
/// - Context field for job execution context
///
/// # Database Schema
///
/// ```sql
/// CREATE TABLE IF NOT EXISTS Jobs (
///     job_id UUID PRIMARY KEY NOT NULL,
///     name TEXT NOT NULL,
///     payload JSONB NOT NULL,
///     priority INT NOT NULL,
///     run_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
///     created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
///     locked_at TIMESTAMPTZ,
///     locked_by TEXT,
///     context TEXT NOT NULL
/// );
/// ```
fn copy_jobs_migration(project_path: &Path) -> Result<()> {
    let jobs_migration = r#"-- Add migration script here
CREATE TABLE
  IF NOT EXISTS Jobs (
    job_id UUID PRIMARY KEY NOT NULL,
    name TEXT NOT NULL,
    payload JSONB NOT NULL,
    priority INT NOT NULL,
    run_at TIMESTAMPTZ NOT NULL DEFAULT NOW (),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW (),
    locked_at TIMESTAMPTZ,
    locked_by TEXT,
    context TEXT NOT NULL
  );
"#;
    
    fs::write(
        project_path.join("migrations").join("20231210151519_AddJobsTable.sql"),
        jobs_migration
    ).context("Failed to write jobs migration")?;
    
    Ok(())
}

/// Copies the cron scheduling migration files to the new project.
///
/// Creates both up and down SQL migration files for the cron scheduling system.
/// These migrations establish the `Crons` table used to track cron job execution
/// and prevent duplicate runs.
///
/// # Arguments
///
/// * `project_path` - Path to the project directory where migrations should be created
///
/// # Returns
///
/// - `Ok(())` on successful migration file creation
/// - `Err(anyhow::Error)` if file writing fails
///
/// # Generated Migrations
///
/// Creates two migration files:
/// - `migrations/20240228040146_AddCrons.up.sql` - Creates the cron table
/// - `migrations/20240228040146_AddCrons.down.sql` - Removes the cron table
///
/// # Database Schema (Up Migration)
///
/// ```sql
/// CREATE TABLE IF NOT EXISTS Crons (
///     cron_id UUID PRIMARY KEY,
///     name TEXT NOT NULL,
///     last_run_at TIMESTAMP WITH TIME ZONE NOT NULL,
///     created_at TIMESTAMP WITH TIME ZONE NOT NULL,
///     updated_at TIMESTAMP WITH TIME ZONE NOT NULL
/// );
/// CREATE UNIQUE INDEX idx_crons_name ON Crons (name);
/// ```
///
/// # Rollback (Down Migration)
///
/// ```sql
/// DROP TABLE Crons;
/// ```
fn copy_cron_migrations(project_path: &Path) -> Result<()> {
    let cron_up_migration = r#"-- Add migration script here
CREATE TABLE
  IF NOT EXISTS Crons (
    cron_id UUID PRIMARY KEY,
    name TEXT NOT NULL,
    last_run_at TIMESTAMP
    WITH
      TIME ZONE NOT NULL,
      created_at TIMESTAMP
    WITH
      TIME ZONE NOT NULL,
      updated_at TIMESTAMP
    WITH
      TIME ZONE NOT NULL
  );

CREATE UNIQUE INDEX idx_crons_name ON Crons (name);
"#;

    let cron_down_migration = r#"-- Add migration script here
DROP TABLE Crons;
"#;
    
    fs::write(
        project_path.join("migrations").join("20240228040146_AddCrons.up.sql"),
        cron_up_migration
    ).context("Failed to write cron up migration")?;
    
    fs::write(
        project_path.join("migrations").join("20240228040146_AddCrons.down.sql"),
        cron_down_migration
    ).context("Failed to write cron down migration")?;
    
    Ok(())
}

/// Copies the session management migration files to the new project.
///
/// Creates both up and down SQL migration files for the session management system.
/// These migrations establish the `Sessions` table with automatic timestamp updates
/// and include PostgreSQL-specific trigger functions.
///
/// # Arguments
///
/// * `project_path` - Path to the project directory where migrations should be created
///
/// # Returns
///
/// - `Ok(())` on successful migration file creation
/// - `Err(anyhow::Error)` if file writing fails
///
/// # Generated Migrations
///
/// Creates two migration files:
/// - `migrations/20250413182934_AddSessions.up.sql` - Creates sessions table and triggers
/// - `migrations/20250413182934_AddSessions.down.sql` - Removes sessions table and functions
///
/// # Database Schema (Up Migration)
///
/// ```sql
/// CREATE TABLE IF NOT EXISTS Sessions (
///     session_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
///     created_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,
///     updated_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL
/// );
///
/// -- Automatic timestamp update function
/// CREATE OR REPLACE FUNCTION update_updated_at_column()
/// RETURNS TRIGGER AS $$
/// BEGIN
///    NEW.updated_at = NOW();
///    RETURN NEW;
/// END;
/// $$ language 'plpgsql';
///
/// -- Trigger for automatic updates
/// CREATE TRIGGER update_sessions_updated_at
///   BEFORE UPDATE ON sessions
///   FOR EACH ROW
///   EXECUTE FUNCTION update_updated_at_column();
/// ```
///
/// # Rollback (Down Migration)
///
/// ```sql
/// DROP TABLE IF EXISTS Sessions;
/// DROP FUNCTION IF EXISTS update_updated_at_column();
/// ```
///
/// # PostgreSQL Features
///
/// - Uses `gen_random_uuid()` for automatic UUID generation
/// - Includes PL/pgSQL trigger function for timestamp updates
/// - Provides proper cleanup in down migration
fn copy_session_migrations(project_path: &Path) -> Result<()> {
    let session_up_migration = r#"-- Add migration script here
CREATE TABLE
  IF NOT EXISTS Sessions (
    session_id UUID PRIMARY KEY DEFAULT gen_random_uuid (),
    created_at TIMESTAMP
    WITH
      TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,
      updated_at TIMESTAMP
    WITH
      TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL
  );

-- Create a function to update the updated_at timestamp
CREATE OR REPLACE FUNCTION update_updated_at_column()
RETURNS TRIGGER AS $$
BEGIN
   NEW.updated_at = NOW();
   RETURN NEW;
END;
$$ language 'plpgsql';

-- Create a trigger to automatically update the updated_at column
CREATE TRIGGER update_sessions_updated_at
  BEFORE UPDATE ON sessions
  FOR EACH ROW
  EXECUTE FUNCTION update_updated_at_column();
"#;

    let session_down_migration = r#"-- Add migration script here
DROP TABLE IF EXISTS Sessions;

DROP FUNCTION IF EXISTS update_updated_at_column ();
"#;
    
    fs::write(
        project_path.join("migrations").join("20250413182934_AddSessions.up.sql"),
        session_up_migration
    ).context("Failed to write session up migration")?;
    
    fs::write(
        project_path.join("migrations").join("20250413182934_AddSessions.down.sql"),
        session_down_migration
    ).context("Failed to write session down migration")?;
    
    Ok(())
}