use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cja::sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use cja::uuid::Uuid;

struct ChildGuard(Option<std::process::Child>);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn options() -> PgConnectOptions {
    PgConnectOptions::from_str(
        &std::env::var("DATABASE_URL").unwrap_or_else(|_| "postgres:///postgres".into()),
    )
    .unwrap()
}

async fn admin_pool() -> cja::sqlx::PgPool {
    PgPoolOptions::new()
        .max_connections(4)
        .connect_with(options().database("postgres"))
        .await
        .unwrap()
}

fn unique_prefix() -> String {
    format!("t{}_", &Uuid::new_v4().simple().to_string()[..8])
}

fn named_database(prefix: &str, epoch: u64) -> String {
    format!("{prefix}{epoch}_{}", Uuid::new_v4().simple())
}

async fn create_database(admin: &cja::sqlx::PgPool, name: &str) {
    cja::sqlx::query(&format!(r#"CREATE DATABASE "{name}""#))
        .execute(admin)
        .await
        .unwrap();
}

async fn drop_database(admin: &cja::sqlx::PgPool, name: &str) {
    let _ = cja::sqlx::query(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = $1 AND pid <> pg_backend_pid()",
    )
    .bind(name)
    .execute(admin)
    .await;
    let _ = cja::sqlx::query(&format!(r#"DROP DATABASE IF EXISTS "{name}""#))
        .execute(admin)
        .await;
}

async fn database_exists(admin: &cja::sqlx::PgPool, name: &str) -> bool {
    cja::sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = $1)",
    )
    .bind(name)
    .fetch_one(admin)
    .await
    .unwrap()
}

#[tokio::test]
async fn reaps_stale_disconnected_database() {
    let admin = admin_pool().await;
    let prefix = unique_prefix();
    let name = named_database(&prefix, 1);
    create_database(&admin, &name).await;

    let dropped = cja::testing::test_db::reap_stale_test_databases(
        &admin,
        &prefix,
        SystemTime::now(),
        Duration::from_secs(1),
    )
    .await
    .unwrap();

    assert_eq!(dropped, vec![name.clone()]);
    assert!(!database_exists(&admin, &name).await);
}

#[tokio::test]
async fn preserves_live_then_reaps_disconnected_database() {
    let admin = admin_pool().await;
    let prefix = unique_prefix();
    let name = named_database(&prefix, 1);
    create_database(&admin, &name).await;
    let live = PgPoolOptions::new()
        .min_connections(1)
        .idle_timeout(None)
        .connect_with(options().database(&name))
        .await
        .unwrap();
    drop(live.acquire().await.unwrap());

    let first = cja::testing::test_db::reap_stale_test_databases(
        &admin,
        &prefix,
        SystemTime::now(),
        Duration::ZERO,
    )
    .await
    .unwrap();
    assert!(first.is_empty());
    assert!(database_exists(&admin, &name).await);

    live.close().await;
    let second = cja::testing::test_db::reap_stale_test_databases(
        &admin,
        &prefix,
        SystemTime::now(),
        Duration::ZERO,
    )
    .await
    .unwrap();
    assert_eq!(second, vec![name]);
}

#[tokio::test]
async fn preserves_fresh_and_legacy_databases() {
    let admin = admin_pool().await;
    let prefix = unique_prefix();
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let fresh = named_database(&prefix, epoch);
    let legacy = format!("{prefix}{}", Uuid::new_v4().simple());
    create_database(&admin, &fresh).await;
    create_database(&admin, &legacy).await;

    let dropped = cja::testing::test_db::reap_stale_test_databases(
        &admin,
        &prefix,
        SystemTime::now(),
        Duration::from_hours(1),
    )
    .await
    .unwrap();
    assert!(dropped.is_empty());
    assert!(database_exists(&admin, &fresh).await);
    assert!(database_exists(&admin, &legacy).await);
    drop_database(&admin, &fresh).await;
    drop_database(&admin, &legacy).await;
}

#[tokio::test]
async fn concurrent_reapers_tolerate_the_same_candidate() {
    let admin = admin_pool().await;
    let prefix = unique_prefix();
    let name = named_database(&prefix, 1);
    create_database(&admin, &name).await;
    let now = SystemTime::now();

    let (left, right) = tokio::join!(
        cja::testing::test_db::reap_stale_test_databases(&admin, &prefix, now, Duration::ZERO),
        cja::testing::test_db::reap_stale_test_databases(&admin, &prefix, now, Duration::ZERO)
    );
    let count = left.unwrap().len() + right.unwrap().len();
    assert!(count <= 1);
    assert!(!database_exists(&admin, &name).await);
}

#[test]
#[ignore = "cross-process helper"]
fn cross_process_liveness_helper() {
    if std::env::var_os("CJA_TEST_DB_CHILD").is_none() {
        return;
    }
    let prefix = std::env::var("CJA_TEST_DB_CHILD_PREFIX").unwrap();
    let ready = PathBuf::from(std::env::var_os("CJA_TEST_DB_READY").unwrap());
    let release = PathBuf::from(std::env::var_os("CJA_TEST_DB_RELEASE").unwrap());
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let (_pool, _guard) = cja::testing::test_db::create_test_database(&prefix)
            .await
            .unwrap();
        std::fs::write(&ready, b"ready").unwrap();
        while !release.exists() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    });
}

#[tokio::test]
async fn preserves_database_owned_by_another_process() {
    let prefix = unique_prefix();
    let directory = tempfile::tempdir().unwrap();
    let ready = directory.path().join("ready");
    let release = directory.path().join("release");
    let executable = std::env::current_exe().unwrap();
    let child = Command::new(executable)
        .args([
            "--exact",
            "integration::test_db::cross_process_liveness_helper",
            "--ignored",
            "--nocapture",
        ])
        .env("CJA_TEST_DB_CHILD", "1")
        .env("CJA_TEST_DB_CHILD_PREFIX", &prefix)
        .env("CJA_TEST_DB_READY", &ready)
        .env("CJA_TEST_DB_RELEASE", &release)
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    let mut child = ChildGuard(Some(child));

    async {
        tokio::time::timeout(Duration::from_secs(20), async {
            while !ready.exists() {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap();
        let admin = admin_pool().await;
        let names: Vec<String> = cja::sqlx::query_scalar(
            "SELECT datname FROM pg_database WHERE datname LIKE $1 ORDER BY datname",
        )
        .bind(format!("{prefix}%"))
        .fetch_all(&admin)
        .await
        .unwrap();
        assert_eq!(names.len(), 1);
        let dropped = cja::testing::test_db::reap_stale_test_databases(
            &admin,
            &prefix,
            SystemTime::now(),
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert!(dropped.is_empty());
        std::fs::write(&release, b"release").unwrap();
        let status = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Some(status) = child.0.as_mut().unwrap().try_wait().unwrap() {
                    break status;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap();
        assert!(status.success());
        child.0.take();
        assert!(!database_exists(&admin, &names[0]).await);
    }
    .await;
}
