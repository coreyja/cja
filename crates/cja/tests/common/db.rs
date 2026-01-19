use cja::db::Migrator;
use cja::deadpool_postgres::{Config, Pool, Runtime};
use cja::tokio_postgres::NoTls;
use uuid::Uuid;

pub async fn setup_test_db() -> cja::Result<(Pool, super::TestDbGuard)> {
    let db_name = format!("cja_test_{}", Uuid::new_v4().to_string().replace('-', ""));
    let pool = super::ensure_test_db(&db_name).await?;

    // Run migrations
    let migrator = Migrator::from_path("./migrations").expect("failed to load migrations");
    let client = pool.get().await.map_err(|e| cja::color_eyre::eyre::eyre!("Pool error: {e}"))?;
    migrator
        .run(&client)
        .await
        .map_err(|e| cja::color_eyre::eyre::eyre!("Migration error: {e}"))?;

    let guard = super::cleanup_on_drop(db_name);

    Ok((pool, guard))
}

#[allow(dead_code)]
pub async fn seed_test_session(pool: &Pool) -> cja::Result<Uuid> {
    let session_id = Uuid::new_v4();

    let client = pool.get().await.map_err(|e| cja::color_eyre::eyre::eyre!("Pool error: {e}"))?;
    client
        .execute(
            "INSERT INTO sessions (session_id) VALUES ($1)",
            &[&session_id],
        )
        .await?;

    Ok(session_id)
}

#[allow(dead_code)]
pub async fn seed_test_job(
    pool: &Pool,
    job_name: &str,
    job_data: serde_json::Value,
    context: &str,
) -> cja::Result<Uuid> {
    let job_id = Uuid::new_v4();
    let now = chrono::Utc::now();

    let client = pool.get().await.map_err(|e| cja::color_eyre::eyre::eyre!("Pool error: {e}"))?;
    client
        .execute(
            "INSERT INTO jobs (job_id, name, payload, priority, run_at, created_at, context, error_count)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            &[&job_id, &job_name, &job_data, &0i32, &now, &now, &context, &0i32],
        )
        .await?;

    Ok(job_id)
}

#[allow(dead_code)]
pub async fn get_job_by_id(pool: &Pool, job_id: Uuid) -> cja::Result<Option<JobInfo>> {
    let client = pool.get().await.map_err(|e| cja::color_eyre::eyre::eyre!("Pool error: {e}"))?;
    let row = client
        .query_opt(
            "SELECT job_id, name, payload, locked_at, locked_by, error_count, last_error_message, last_failed_at
             FROM jobs WHERE job_id = $1",
            &[&job_id],
        )
        .await?;

    Ok(row.map(|r| JobInfo {
        job_id: r.get(0),
        name: r.get(1),
        payload: r.get(2),
        locked_at: r.get(3),
        locked_by: r.get(4),
        error_count: r.get(5),
        last_error_message: r.get(6),
        last_failed_at: r.get(7),
    }))
}

#[allow(dead_code)]
pub async fn count_unlocked_jobs(pool: &Pool) -> cja::Result<i64> {
    let client = pool.get().await.map_err(|e| cja::color_eyre::eyre::eyre!("Pool error: {e}"))?;
    let row = client
        .query_one("SELECT COUNT(*) as count FROM jobs WHERE locked_at IS NULL", &[])
        .await?;

    Ok(row.get::<_, Option<i64>>(0).unwrap_or(0))
}

#[allow(dead_code)]
pub struct JobInfo {
    pub job_id: Uuid,
    pub name: String,
    pub payload: serde_json::Value,
    pub locked_at: Option<chrono::DateTime<chrono::Utc>>,
    pub locked_by: Option<String>,
    pub error_count: i32,
    pub last_error_message: Option<String>,
    pub last_failed_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Create a pool for a specific database URL.
pub async fn create_pool(db_url: &str) -> cja::Result<Pool> {
    let config = db_url
        .parse::<cja::tokio_postgres::Config>()
        .map_err(|e| cja::color_eyre::eyre::eyre!("Failed to parse DATABASE_URL: {e}"))?;

    let mut cfg = Config::new();
    cfg.dbname = config.get_dbname().map(String::from);
    cfg.host = config.get_hosts().first().map(|h| match h {
        cja::tokio_postgres::config::Host::Tcp(s) => s.clone(),
        cja::tokio_postgres::config::Host::Unix(p) => p.to_string_lossy().into_owned(),
    });
    cfg.port = config.get_ports().first().copied();
    cfg.user = config.get_user().map(String::from);
    cfg.password = config
        .get_password()
        .map(|p| String::from_utf8_lossy(p).into_owned());

    cfg.create_pool(Some(Runtime::Tokio1), NoTls)
        .map_err(|e| cja::color_eyre::eyre::eyre!("Failed to create pool: {e}"))
}
