pub mod app;
pub mod db;

use cja::deadpool_postgres::Pool;
use std::sync::Arc;
use tokio::sync::Mutex;

pub static DB_MUTEX: std::sync::LazyLock<Arc<Mutex<()>>> =
    std::sync::LazyLock::new(|| Arc::new(Mutex::new(())));

pub fn test_db_url(db_name: &str) -> String {
    let base_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://localhost/postgres".to_string());

    // Remove any existing database name from the URL and append our test db name
    let base_url = if let Some(idx) = base_url.rfind('/') {
        let (url_without_db, _) = base_url.split_at(idx);
        url_without_db.to_string()
    } else {
        base_url
    };

    format!("{base_url}/{db_name}")
}

pub async fn ensure_test_db(db_name: &str) -> cja::Result<Pool> {
    let _lock = DB_MUTEX.lock().await;

    let base_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://localhost/postgres".to_string());

    let base_pool = db::create_pool(&base_url)?;
    let base_client = base_pool
        .get()
        .await
        .map_err(|e| cja::color_eyre::eyre::eyre!("Pool error: {e}"))?;

    // Drop database if exists
    let _ = base_client
        .execute(&format!("DROP DATABASE IF EXISTS \"{db_name}\""), &[])
        .await;

    // Create fresh database
    base_client
        .execute(&format!("CREATE DATABASE \"{db_name}\""), &[])
        .await?;

    drop(base_client);
    base_pool.close();

    // Connect to the new test database
    let test_db_url = test_db_url(db_name);
    let pool = db::create_pool(&test_db_url)?;

    Ok(pool)
}

pub fn cleanup_on_drop(db_name: String) -> TestDbGuard {
    TestDbGuard { db_name }
}

pub struct TestDbGuard {
    db_name: String,
}

impl Drop for TestDbGuard {
    fn drop(&mut self) {
        let db_name = self.db_name.clone();
        // We'll use std::thread::spawn to avoid the runtime-in-runtime issue
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let base_url = std::env::var("DATABASE_URL")
                    .unwrap_or_else(|_| "postgres://localhost/postgres".to_string());

                if let Ok(base_pool) = db::create_pool(&base_url) {
                    if let Ok(client) = base_pool.get().await {
                        let _ = client
                            .execute(&format!("DROP DATABASE IF EXISTS \"{db_name}\""), &[])
                            .await;
                    }
                    base_pool.close();
                }
            });
        })
        .join();
    }
}
