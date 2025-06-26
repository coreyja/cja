use cja::db::run_migrations;
use sqlx::PgPool;
use uuid::Uuid;

pub async fn setup_test_db() -> cja::Result<(PgPool, super::TestDbGuard)> {
    let db_name = format!("cja_test_{}", Uuid::new_v4().to_string().replace("-", ""));
    let pool = super::ensure_test_db(&db_name).await?;
    
    // Run migrations
    run_migrations(&pool).await?;
    
    let guard = super::cleanup_on_drop(db_name);
    
    Ok((pool, guard))
}

pub async fn seed_test_session(pool: &PgPool) -> cja::Result<Uuid> {
    let session_id = Uuid::new_v4();
    
    sqlx::query!(
        "INSERT INTO sessions (session_id) VALUES ($1)",
        session_id
    )
    .execute(pool)
    .await?;
    
    Ok(session_id)
}

pub async fn seed_test_job(pool: &PgPool, job_name: &str, job_data: serde_json::Value, context: &str) -> cja::Result<Uuid> {
    let job_id = Uuid::new_v4();
    
    sqlx::query!(
        "INSERT INTO jobs (job_id, name, payload, priority, run_at, created_at, context) 
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
        job_id,
        job_name,
        job_data,
        0,
        chrono::Utc::now(),
        chrono::Utc::now(),
        context
    )
    .execute(pool)
    .await?;
    
    Ok(job_id)
}

pub async fn get_job_by_id(pool: &PgPool, job_id: Uuid) -> cja::Result<Option<JobInfo>> {
    let row = sqlx::query!(
        "SELECT job_id, name, payload, locked_at, locked_by FROM jobs WHERE job_id = $1",
        job_id
    )
    .fetch_optional(pool)
    .await?;
    
    Ok(row.map(|r| JobInfo {
        job_id: r.job_id,
        name: r.name,
        payload: r.payload,
        locked_at: r.locked_at,
        locked_by: r.locked_by,
    }))
}

pub async fn count_unlocked_jobs(pool: &PgPool) -> cja::Result<i64> {
    let row = sqlx::query!(
        "SELECT COUNT(*) as count FROM jobs WHERE locked_at IS NULL"
    )
    .fetch_one(pool)
    .await?;
    
    Ok(row.count.unwrap_or(0))
}

pub struct JobInfo {
    pub job_id: Uuid,
    pub name: String,
    pub payload: serde_json::Value,
    pub locked_at: Option<chrono::DateTime<chrono::Utc>>,
    pub locked_by: Option<String>,
}