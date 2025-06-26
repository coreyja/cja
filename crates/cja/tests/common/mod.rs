pub mod app;
pub mod db;

use once_cell::sync::Lazy;
use sqlx::postgres::PgPoolOptions;
use std::sync::Arc;
use tokio::sync::Mutex;

pub static DB_MUTEX: Lazy<Arc<Mutex<()>>> = Lazy::new(|| Arc::new(Mutex::new(())));

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
    
    format!("{}/{}", base_url, db_name)
}

pub async fn ensure_test_db(db_name: &str) -> sqlx::Result<sqlx::PgPool> {
    let _lock = DB_MUTEX.lock().await;
    
    let base_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://localhost/postgres".to_string());
    
    let base_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&base_url)
        .await?;
    
    // Drop database if exists
    let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS \"{}\"", db_name))
        .execute(&base_pool)
        .await;
    
    // Create fresh database
    sqlx::query(&format!("CREATE DATABASE \"{}\"", db_name))
        .execute(&base_pool)
        .await?;
    
    base_pool.close().await;
    
    // Connect to the new test database
    let test_db_url = test_db_url(db_name);
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&test_db_url)
        .await?;
    
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
        // We'll use tokio::task::spawn_blocking to avoid the runtime-in-runtime issue
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let base_url = std::env::var("DATABASE_URL")
                    .unwrap_or_else(|_| "postgres://localhost/postgres".to_string());
                
                if let Ok(base_pool) = PgPoolOptions::new()
                    .max_connections(1)
                    .connect(&base_url)
                    .await
                {
                    let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS \"{}\"", db_name))
                        .execute(&base_pool)
                        .await;
                    base_pool.close().await;
                }
            });
        }).join();
    }
}