//! Test support for cja applications.
//!
//! This module provides utilities for writing database tests, including
//! isolated test databases and automatic migrations.
//!
//! # Example
//!
//! ```rust,ignore
//! use cja::testing::test;
//! use deadpool_postgres::Pool;
//!
//! #[cja::test]
//! async fn test_create_user(pool: Pool) {
//!     let client = pool.get().await.unwrap();
//!     client.execute("INSERT INTO users (name) VALUES ($1)", &[&"Alice"]).await.unwrap();
//!     // ...
//! }
//! ```

use std::future::Future;
use std::sync::Arc;

use deadpool_postgres::{Config, Pool, Runtime};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tokio_postgres::NoTls;

use crate::db::Migrator;

/// Static mutex to coordinate test database creation.
static TEST_DB_MUTEX: std::sync::LazyLock<Arc<Mutex<()>>> =
    std::sync::LazyLock::new(|| Arc::new(Mutex::new(())));

/// Generate a unique database name from the test path.
fn db_name_for_test(test_path: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(test_path.as_bytes());
    let hash = hasher.finalize();
    let hash_str = hex::encode(&hash[..16]); // Use first 16 bytes (32 hex chars)
    format!("_cja_test_{hash_str}")
}

/// Get the base database URL for creating test databases.
fn base_db_url() -> String {
    std::env::var("DATABASE_URL").unwrap_or_else(|_| "postgres://localhost/postgres".to_string())
}

/// Parse the base URL and return the host/connection portion without a database.
fn url_without_db(url: &str) -> String {
    if let Some(idx) = url.rfind('/') {
        url[..idx].to_string()
    } else {
        url.to_string()
    }
}

/// Create a pool connected to a specific database.
fn create_pool(db_url: &str) -> Result<Pool, deadpool_postgres::CreatePoolError> {
    let config = db_url
        .parse::<tokio_postgres::Config>()
        .expect("failed to parse DATABASE_URL");

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

    cfg.create_pool(Some(Runtime::Tokio1), NoTls)
}

/// Context for a running test, used for cleanup.
pub struct TestContext {
    db_name: String,
    base_url: String,
}

impl TestContext {
    /// Clean up the test database.
    pub async fn cleanup(&self) {
        let pool = match create_pool(&self.base_url) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("Failed to connect for cleanup: {e}");
                return;
            }
        };

        let client = match pool.get().await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Failed to get connection for cleanup: {e}");
                return;
            }
        };

        // Terminate existing connections to the test database
        let terminate_sql = format!(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{}'",
            self.db_name
        );
        let _ = client.execute(&terminate_sql, &[]).await;

        // Drop the database
        let drop_sql = format!("DROP DATABASE IF EXISTS \"{}\"", self.db_name);
        if let Err(e) = client.execute(&drop_sql, &[]).await {
            eprintln!("Failed to drop test database {}: {e}", self.db_name);
        }
    }
}

/// Set up an isolated test database.
async fn setup_test_db(
    test_path: &str,
    migrator: &Migrator,
) -> Result<(Pool, TestContext), Box<dyn std::error::Error + Send + Sync>> {
    let _lock = TEST_DB_MUTEX.lock().await;

    let db_name = db_name_for_test(test_path);
    let base_url = base_db_url();
    let base_url_without_db = url_without_db(&base_url);

    // Connect to the base database to create our test database
    let base_pool = create_pool(&base_url)?;
    let base_client = base_pool.get().await?;

    // Drop existing test database if it exists (from a failed previous run)
    let drop_sql = format!("DROP DATABASE IF EXISTS \"{db_name}\"");
    base_client.execute(&drop_sql, &[]).await?;

    // Create the test database
    let create_sql = format!("CREATE DATABASE \"{db_name}\"");
    base_client.execute(&create_sql, &[]).await?;

    drop(base_client);

    // Connect to the new test database
    let test_db_url = format!("{base_url_without_db}/{db_name}");
    let test_pool = create_pool(&test_db_url)?;

    // Run migrations
    let client = test_pool.get().await?;
    migrator.run(&client).await?;
    drop(client);

    let context = TestContext { db_name, base_url };

    Ok((test_pool, context))
}

/// Run a test with an isolated database.
///
/// This function is called by the `#[cja::test]` macro.
pub fn run_test<F, Fut>(test_path: &str, migrator: &Migrator, test_fn: F)
where
    F: FnOnce(Pool) -> Fut,
    Fut: Future<Output = ()>,
{
    // Create a new runtime for this test
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to create tokio runtime");

    // Set up the test database
    let (pool, context) = rt.block_on(async {
        match setup_test_db(test_path, migrator).await {
            Ok(result) => result,
            Err(e) => {
                panic!("Failed to set up test database: {e}");
            }
        }
    });

    // Run the test with panic catching
    let test_pool = pool.clone();
    let test_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rt.block_on(test_fn(test_pool));
    }));

    // Close the pool
    pool.close();

    // Clean up based on result
    if test_result.is_ok() {
        rt.block_on(context.cleanup());
    } else {
        eprintln!(
            "Test failed, leaving database '{}' for inspection",
            context.db_name
        );
    }

    // Re-panic if the test failed
    if let Err(e) = test_result {
        std::panic::resume_unwind(e);
    }
}

/// Helper to run migrations against a pool.
///
/// Useful for manually setting up test databases without the macro.
pub async fn run_migrations(
    pool: &Pool,
    migrator: &Migrator,
) -> Result<(), crate::db::MigrationError> {
    let client = pool
        .get()
        .await
        .map_err(|e| crate::db::MigrationError::MigrationFailed {
            version: 0,
            name: "connection".to_string(),
            message: e.to_string(),
        })?;
    migrator.run(&client).await
}
