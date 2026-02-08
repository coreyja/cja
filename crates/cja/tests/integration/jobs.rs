use cja::jobs::Job;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize, Clone)]
struct TestJob {
    id: String,
    value: i32,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct FailingJob {
    id: String,
    should_fail: bool,
}

// Track job executions for testing
static JOB_EXECUTIONS: std::sync::LazyLock<Arc<Mutex<Vec<String>>>> =
    std::sync::LazyLock::new(|| Arc::new(Mutex::new(Vec::new())));

#[async_trait::async_trait]
impl<AS: cja::app_state::AppState> Job<AS> for TestJob {
    const NAME: &'static str = "TestJob";

    async fn run(&self, _app_state: AS) -> color_eyre::Result<()> {
        let mut executions = JOB_EXECUTIONS.lock().await;
        executions.push(format!("TestJob-{}", self.id));
        println!(
            "Executing TestJob with id: {} and value: {}",
            self.id, self.value
        );
        Ok(())
    }
}

#[async_trait::async_trait]
impl<AS: cja::app_state::AppState> Job<AS> for FailingJob {
    const NAME: &'static str = "FailingJob";

    async fn run(&self, _app_state: AS) -> color_eyre::Result<()> {
        let mut executions = JOB_EXECUTIONS.lock().await;
        executions.push(format!("FailingJob-{}", self.id));

        if self.should_fail {
            color_eyre::eyre::bail!("Job failed as requested");
        }
        Ok(())
    }
}

// For now, comment out the registry macro since it has type inference issues in tests
// We'll test the jobs directly without the registry
// impl_job_registry!(crate::common::app::TestAppState, TestJob, FailingJob);

#[tokio::test]
async fn test_job_enqueue() {
    let (pool, _guard) = crate::common::db::setup_test_db().await.unwrap();
    let app_state = crate::common::app::TestAppState::new(pool.clone());

    let job = TestJob {
        id: "test-1".to_string(),
        value: 42,
    };

    // Enqueue the job
    let result = job
        .enqueue(app_state, "test-context".to_string(), None)
        .await;
    assert!(result.is_ok());

    // Verify job is in database
    let count = sqlx::query("SELECT COUNT(*) as count FROM jobs WHERE name = $1")
        .bind(<TestJob as Job<crate::common::app::TestAppState>>::NAME)
        .fetch_one(&pool)
        .await
        .unwrap();

    assert_eq!(count.get::<Option<i64>, _>("count").unwrap(), 1);
}

#[tokio::test]
async fn test_job_with_priority() {
    let (pool, _guard) = crate::common::db::setup_test_db().await.unwrap();
    let app_state = crate::common::app::TestAppState::new(pool.clone());

    // Enqueue multiple jobs
    for i in 0..3 {
        let job = TestJob {
            id: format!("priority-test-{i}"),
            value: i,
        };
        job.enqueue(app_state.clone(), format!("priority-{i}"), Some(i))
            .await
            .unwrap();
    }

    // Verify all jobs are enqueued with correct priorities
    let rows = sqlx::query("SELECT priority FROM jobs ORDER BY priority ASC")
        .fetch_all(&pool)
        .await
        .unwrap();

    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].get::<i32, _>("priority"), 0);
    assert_eq!(rows[1].get::<i32, _>("priority"), 1);
    assert_eq!(rows[2].get::<i32, _>("priority"), 2);
}

#[tokio::test]
async fn test_enqueue_priority_none_defaults_to_zero() {
    let (pool, _guard) = crate::common::db::setup_test_db().await.unwrap();
    let app_state = crate::common::app::TestAppState::new(pool.clone());

    let job = TestJob {
        id: "default-priority".to_string(),
        value: 1,
    };

    job.enqueue(app_state, "test".to_string(), None)
        .await
        .unwrap();

    let row = sqlx::query("SELECT priority FROM jobs")
        .fetch_one(&pool)
        .await
        .unwrap();

    assert_eq!(row.get::<i32, _>("priority"), 0);
}

#[tokio::test]
async fn test_enqueue_with_negative_priority() {
    let (pool, _guard) = crate::common::db::setup_test_db().await.unwrap();
    let app_state = crate::common::app::TestAppState::new(pool.clone());

    let job = TestJob {
        id: "low-priority".to_string(),
        value: 1,
    };

    job.enqueue(app_state, "background-work".to_string(), Some(-10))
        .await
        .unwrap();

    let row = sqlx::query("SELECT priority FROM jobs")
        .fetch_one(&pool)
        .await
        .unwrap();

    assert_eq!(row.get::<i32, _>("priority"), -10);
}

#[tokio::test]
async fn test_job_payload_serialization() {
    let (pool, _guard) = crate::common::db::setup_test_db().await.unwrap();
    let app_state = crate::common::app::TestAppState::new(pool.clone());

    let job = TestJob {
        id: "serialization-test".to_string(),
        value: 123,
    };

    job.enqueue(app_state, "serialization-context".to_string(), None)
        .await
        .unwrap();

    // Retrieve and verify payload
    let row = sqlx::query("SELECT payload FROM jobs WHERE name = $1")
        .bind(<TestJob as Job<crate::common::app::TestAppState>>::NAME)
        .fetch_one(&pool)
        .await
        .unwrap();

    let deserialized: TestJob = serde_json::from_value(row.get("payload")).unwrap();
    assert_eq!(deserialized.id, "serialization-test");
    assert_eq!(deserialized.value, 123);
}

#[tokio::test]
async fn test_job_locking() {
    let (pool, _guard) = crate::common::db::setup_test_db().await.unwrap();
    let app_state = crate::common::app::TestAppState::new(pool.clone());

    let job = TestJob {
        id: "lock-test".to_string(),
        value: 999,
    };

    job.enqueue(app_state, "lock-context".to_string(), None)
        .await
        .unwrap();

    // Get the job_id
    let job_row = sqlx::query("SELECT job_id FROM jobs WHERE name = $1")
        .bind(<TestJob as Job<crate::common::app::TestAppState>>::NAME)
        .fetch_one(&pool)
        .await
        .unwrap();

    // Lock the job
    let worker_id = "test-worker-1";
    let locked = sqlx::query(
        "UPDATE jobs SET locked_at = NOW(), locked_by = $1
         WHERE job_id = $2 AND locked_at IS NULL",
    )
    .bind(worker_id)
    .bind(job_row.get::<Uuid, _>("job_id"))
    .execute(&pool)
    .await
    .unwrap();

    assert_eq!(locked.rows_affected(), 1);

    // Try to lock again (should fail)
    let locked_again = sqlx::query(
        "UPDATE jobs SET locked_at = NOW(), locked_by = $1
         WHERE job_id = $2 AND locked_at IS NULL",
    )
    .bind("another-worker")
    .bind(job_row.get::<Uuid, _>("job_id"))
    .execute(&pool)
    .await
    .unwrap();

    assert_eq!(locked_again.rows_affected(), 0);
}

#[tokio::test]
async fn test_concurrent_job_enqueue() {
    let (pool, _guard) = crate::common::db::setup_test_db().await.unwrap();
    let app_state = crate::common::app::TestAppState::new(pool.clone());

    let mut handles = vec![];

    for i in 0..10 {
        let state = app_state.clone();
        let handle = tokio::spawn(async move {
            let job = TestJob {
                id: format!("concurrent-{i}"),
                value: i,
            };
            job.enqueue(state, format!("concurrent-context-{i}"), None)
                .await
        });
        handles.push(handle);
    }

    // Wait for all to complete
    for handle in handles {
        let result = handle.await.unwrap();
        assert!(result.is_ok());
    }

    // Verify all jobs were enqueued
    let count = sqlx::query("SELECT COUNT(*) as count FROM jobs")
        .fetch_one(&pool)
        .await
        .unwrap();

    assert_eq!(count.get::<Option<i64>, _>("count").unwrap(), 10);
}

#[tokio::test]
async fn test_job_context() {
    let (pool, _guard) = crate::common::db::setup_test_db().await.unwrap();
    let app_state = crate::common::app::TestAppState::new(pool.clone());

    let job = TestJob {
        id: "context-test".to_string(),
        value: 42,
    };

    let context = "user-requested-action";
    job.enqueue(app_state, context.to_string(), None)
        .await
        .unwrap();

    // Verify context is stored
    let row = sqlx::query("SELECT context FROM jobs WHERE name = $1")
        .bind(<TestJob as Job<crate::common::app::TestAppState>>::NAME)
        .fetch_one(&pool)
        .await
        .unwrap();

    assert_eq!(row.get::<String, _>("context"), context);
}

#[tokio::test]
async fn test_failing_job_enqueue() {
    let (pool, _guard) = crate::common::db::setup_test_db().await.unwrap();
    let app_state = crate::common::app::TestAppState::new(pool.clone());

    let job = FailingJob {
        id: "fail-test-1".to_string(),
        should_fail: true,
    };

    // Enqueue the job
    let result = job
        .enqueue(app_state, "test-failing".to_string(), None)
        .await;
    assert!(result.is_ok());

    // Verify job is in database with initial error_count of 0
    let row = sqlx::query(
        "SELECT name, error_count, last_error_message, last_failed_at FROM jobs WHERE name = $1",
    )
    .bind(<FailingJob as Job<crate::common::app::TestAppState>>::NAME)
    .fetch_one(&pool)
    .await
    .unwrap();

    assert_eq!(row.get::<String, _>("name"), "FailingJob");
    assert_eq!(row.get::<i32, _>("error_count"), 0);
    assert!(row.get::<Option<String>, _>("last_error_message").is_none());
    assert!(
        row.get::<Option<chrono::DateTime<chrono::Utc>>, _>("last_failed_at")
            .is_none()
    );
}

#[tokio::test]
async fn test_error_tracking_fields_present() {
    let (pool, _guard) = crate::common::db::setup_test_db().await.unwrap();
    let app_state = crate::common::app::TestAppState::new(pool.clone());

    let job = TestJob {
        id: "error-fields-test".to_string(),
        value: 42,
    };

    job.enqueue(app_state, "test-context".to_string(), None)
        .await
        .unwrap();

    // Verify error tracking fields exist and have correct defaults
    let row = sqlx::query(
        "SELECT error_count, last_error_message, last_failed_at FROM jobs WHERE name = $1",
    )
    .bind(<TestJob as Job<crate::common::app::TestAppState>>::NAME)
    .fetch_one(&pool)
    .await
    .unwrap();

    assert_eq!(row.get::<i32, _>("error_count"), 0);
    assert!(row.get::<Option<String>, _>("last_error_message").is_none());
    assert!(
        row.get::<Option<chrono::DateTime<chrono::Utc>>, _>("last_failed_at")
            .is_none()
    );
}

#[tokio::test]
async fn test_job_error_count_increment() {
    let (pool, _guard) = crate::common::db::setup_test_db().await.unwrap();

    // Directly insert a job and simulate a failure update
    let job_id = uuid::Uuid::new_v4();
    let worker_id = "test-worker";

    sqlx::query(
        "INSERT INTO jobs (job_id, name, payload, priority, run_at, created_at, context, error_count, locked_by, locked_at)
         VALUES ($1, $2, $3, $4, NOW(), NOW(), $5, $6, $7, NOW())",
    )
    .bind(job_id)
    .bind("TestJob")
    .bind(serde_json::json!({"id": "test", "value": 1}))
    .bind(0)
    .bind("test")
    .bind(0)
    .bind(worker_id)
    .execute(&pool)
    .await
    .unwrap();

    // Simulate job failure with exponential backoff (like the worker does)
    let error_message = "Test error occurred";
    sqlx::query(
        "UPDATE jobs
         SET locked_by = NULL,
             locked_at = NULL,
             error_count = error_count + 1,
             last_error_message = $3,
             last_failed_at = NOW(),
             run_at = NOW() + (POWER(2, error_count + 1)) * interval '1 second'
         WHERE job_id = $1 AND locked_by = $2",
    )
    .bind(job_id)
    .bind(worker_id)
    .bind(error_message)
    .execute(&pool)
    .await
    .unwrap();

    // Verify error tracking was updated
    let row = sqlx::query(
        "SELECT error_count, last_error_message, last_failed_at, run_at FROM jobs WHERE job_id = $1",
    )
    .bind(job_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    assert_eq!(row.get::<i32, _>("error_count"), 1);
    assert_eq!(
        row.get::<Option<String>, _>("last_error_message"),
        Some(error_message.to_string())
    );
    assert!(
        row.get::<Option<chrono::DateTime<chrono::Utc>>, _>("last_failed_at")
            .is_some()
    );

    // Verify run_at was pushed forward (should be at least 2 seconds in future)
    let run_at = row.get::<chrono::DateTime<chrono::Utc>, _>("run_at");
    assert!(run_at > chrono::Utc::now());
}

#[tokio::test]
async fn test_exponential_backoff_calculation() {
    let (pool, _guard) = crate::common::db::setup_test_db().await.unwrap();

    // Test that exponential backoff follows 2^(error_count + 1) formula
    let job_id = uuid::Uuid::new_v4();
    let worker_id = "test-worker";

    // Start with error_count = 5 to test higher backoff values
    sqlx::query(
        "INSERT INTO jobs (job_id, name, payload, priority, run_at, created_at, context, error_count, locked_by, locked_at)
         VALUES ($1, $2, $3, $4, NOW(), NOW(), $5, $6, $7, NOW())",
    )
    .bind(job_id)
    .bind("TestJob")
    .bind(serde_json::json!({"id": "test", "value": 1}))
    .bind(0)
    .bind("test")
    .bind(5) // error_count = 5
    .bind(worker_id)
    .execute(&pool)
    .await
    .unwrap();

    let before_update = chrono::Utc::now();

    // Simulate failure - should set run_at to NOW() + 2^6 seconds = 64 seconds
    sqlx::query(
        "UPDATE jobs
         SET locked_by = NULL,
             locked_at = NULL,
             error_count = error_count + 1,
             last_error_message = $3,
             last_failed_at = NOW(),
             run_at = NOW() + (POWER(2, error_count + 1)) * interval '1 second'
         WHERE job_id = $1 AND locked_by = $2",
    )
    .bind(job_id)
    .bind(worker_id)
    .bind("test error")
    .execute(&pool)
    .await
    .unwrap();

    let row = sqlx::query("SELECT error_count, run_at FROM jobs WHERE job_id = $1")
        .bind(job_id)
        .fetch_one(&pool)
        .await
        .unwrap();

    assert_eq!(row.get::<i32, _>("error_count"), 6);

    // run_at should be approximately 64 seconds (2^6) in the future
    // Allow some tolerance for test execution time
    let run_at = row.get::<chrono::DateTime<chrono::Utc>, _>("run_at");
    let actual_delay = run_at - before_update;

    // Should be between 63 and 66 seconds to account for timing
    assert!(
        actual_delay >= chrono::Duration::seconds(63)
            && actual_delay <= chrono::Duration::seconds(66),
        "Expected delay ~64 seconds, got {actual_delay:?}"
    );
}

#[tokio::test]
async fn test_max_retries_exceeded_moves_to_dead_letter_queue() {
    let (pool, _guard) = crate::common::db::setup_test_db().await.unwrap();

    let job_id = uuid::Uuid::new_v4();
    let worker_id = "test-worker";
    let max_retries = 20;
    let error_message = "Final failure";
    let payload = serde_json::json!({"id": "test", "value": 1});

    // Insert a job that has already hit max retries
    sqlx::query(
        "INSERT INTO jobs (job_id, name, payload, priority, run_at, created_at, context, error_count, locked_by, locked_at)
         VALUES ($1, $2, $3, $4, NOW(), NOW(), $5, $6, $7, NOW())",
    )
    .bind(job_id)
    .bind("TestJob")
    .bind(&payload)
    .bind(0)
    .bind("test-context")
    .bind(max_retries)
    .bind(worker_id)
    .execute(&pool)
    .await
    .unwrap();

    // Simulate what the worker now does: insert into dead_letter_jobs, then delete from jobs
    let mut tx = pool.begin().await.unwrap();

    sqlx::query(
        "INSERT INTO dead_letter_jobs (original_job_id, name, payload, context, priority, error_count, last_error_message, created_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, NOW())",
    )
    .bind(job_id)
    .bind("TestJob")
    .bind(&payload)
    .bind("test-context")
    .bind(0)
    .bind(max_retries)
    .bind(error_message)
    .execute(&mut *tx)
    .await
    .unwrap();

    sqlx::query("DELETE FROM jobs WHERE job_id = $1 AND locked_by = $2")
        .bind(job_id)
        .bind(worker_id)
        .execute(&mut *tx)
        .await
        .unwrap();

    tx.commit().await.unwrap();

    // Verify job was removed from jobs table
    let jobs_count = sqlx::query("SELECT COUNT(*) as count FROM jobs WHERE job_id = $1")
        .bind(job_id)
        .fetch_one(&pool)
        .await
        .unwrap()
        .get::<Option<i64>, _>("count")
        .unwrap();
    assert_eq!(jobs_count, 0);

    // Verify job was added to dead_letter_jobs
    let dlq_row = sqlx::query(
        "SELECT original_job_id, name, payload, context, priority, error_count, last_error_message, failed_at
         FROM dead_letter_jobs WHERE original_job_id = $1",
    )
    .bind(job_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    assert_eq!(dlq_row.get::<Uuid, _>("original_job_id"), job_id);
    assert_eq!(dlq_row.get::<String, _>("name"), "TestJob");
    assert_eq!(dlq_row.get::<String, _>("context"), "test-context");
    assert_eq!(dlq_row.get::<i32, _>("priority"), 0);
    assert_eq!(dlq_row.get::<i32, _>("error_count"), max_retries);
    assert_eq!(
        dlq_row.get::<Option<String>, _>("last_error_message"),
        Some(error_message.to_string())
    );
    assert!(
        dlq_row
            .get::<Option<chrono::DateTime<chrono::Utc>>, _>("failed_at")
            .is_some()
    );

    // Verify payload was preserved correctly
    let dlq_payload: serde_json::Value = dlq_row.get("payload");
    assert_eq!(dlq_payload, payload);
}

#[tokio::test]
async fn test_dead_letter_queue_preserves_original_job_id() {
    let (pool, _guard) = crate::common::db::setup_test_db().await.unwrap();

    // Insert multiple failed jobs into dead letter queue
    let job_ids: Vec<Uuid> = (0..3).map(|_| uuid::Uuid::new_v4()).collect();

    for (i, job_id) in job_ids.iter().enumerate() {
        sqlx::query(
            "INSERT INTO dead_letter_jobs (original_job_id, name, payload, context, priority, error_count, last_error_message, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, NOW())",
        )
        .bind(job_id)
        .bind("TestJob")
        .bind(serde_json::json!({"id": format!("dead-{i}")}))
        .bind(format!("context-{i}"))
        .bind(0)
        .bind(20)
        .bind(format!("error-{i}"))
        .execute(&pool)
        .await
        .unwrap();
    }

    // Verify all three are in the dead letter queue
    let count = sqlx::query("SELECT COUNT(*) as count FROM dead_letter_jobs")
        .fetch_one(&pool)
        .await
        .unwrap()
        .get::<Option<i64>, _>("count")
        .unwrap();
    assert_eq!(count, 3);

    // Verify each job can be found by its original_job_id
    for job_id in &job_ids {
        let row =
            sqlx::query("SELECT original_job_id FROM dead_letter_jobs WHERE original_job_id = $1")
                .bind(job_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row.get::<Uuid, _>("original_job_id"), *job_id);
    }
}
