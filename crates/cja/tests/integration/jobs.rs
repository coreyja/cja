use cja::jobs::Job;
use serde::{Deserialize, Serialize};
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

#[tokio::test]
async fn test_job_enqueue() {
    let (pool, _guard) = crate::common::db::setup_test_db().await.unwrap();
    let app_state = crate::common::app::TestAppState::new(pool.clone());

    let job = TestJob {
        id: "test-1".to_string(),
        value: 42,
    };

    // Enqueue the job
    let result = job.enqueue(app_state, "test-context".to_string()).await;
    assert!(result.is_ok());

    // Verify job is in database
    let client = pool.get().await.unwrap();
    let row = client
        .query_one(
            "SELECT COUNT(*) as count FROM jobs WHERE name = $1",
            &[&<TestJob as Job<crate::common::app::TestAppState>>::NAME],
        )
        .await
        .unwrap();

    assert_eq!(row.get::<_, Option<i64>>(0).unwrap(), 1);
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
        job.enqueue(app_state.clone(), format!("priority-{i}"))
            .await
            .unwrap();
    }

    // Verify all jobs are enqueued
    let client = pool.get().await.unwrap();
    let row = client
        .query_one("SELECT COUNT(*) as count FROM jobs", &[])
        .await
        .unwrap();

    assert_eq!(row.get::<_, Option<i64>>(0).unwrap(), 3);
}

#[tokio::test]
async fn test_job_payload_serialization() {
    let (pool, _guard) = crate::common::db::setup_test_db().await.unwrap();
    let app_state = crate::common::app::TestAppState::new(pool.clone());

    let job = TestJob {
        id: "serialization-test".to_string(),
        value: 123,
    };

    job.enqueue(app_state, "serialization-context".to_string())
        .await
        .unwrap();

    // Retrieve and verify payload
    let client = pool.get().await.unwrap();
    let row = client
        .query_one(
            "SELECT payload FROM jobs WHERE name = $1",
            &[&<TestJob as Job<crate::common::app::TestAppState>>::NAME],
        )
        .await
        .unwrap();

    let payload: serde_json::Value = row.get(0);
    let deserialized: TestJob = serde_json::from_value(payload).unwrap();
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

    job.enqueue(app_state, "lock-context".to_string())
        .await
        .unwrap();

    // Get the job_id
    let client = pool.get().await.unwrap();
    let job_row = client
        .query_one(
            "SELECT job_id FROM jobs WHERE name = $1",
            &[&<TestJob as Job<crate::common::app::TestAppState>>::NAME],
        )
        .await
        .unwrap();

    let job_id: Uuid = job_row.get(0);

    // Lock the job
    let worker_id = "test-worker-1";
    let locked = client
        .execute(
            "UPDATE jobs SET locked_at = NOW(), locked_by = $1
             WHERE job_id = $2 AND locked_at IS NULL",
            &[&worker_id, &job_id],
        )
        .await
        .unwrap();

    assert_eq!(locked, 1);

    // Try to lock again (should fail)
    let locked_again = client
        .execute(
            "UPDATE jobs SET locked_at = NOW(), locked_by = $1
             WHERE job_id = $2 AND locked_at IS NULL",
            &[&"another-worker", &job_id],
        )
        .await
        .unwrap();

    assert_eq!(locked_again, 0);
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
            job.enqueue(state, format!("concurrent-context-{i}")).await
        });
        handles.push(handle);
    }

    // Wait for all to complete
    for handle in handles {
        let result = handle.await.unwrap();
        assert!(result.is_ok());
    }

    // Verify all jobs were enqueued
    let client = pool.get().await.unwrap();
    let row = client
        .query_one("SELECT COUNT(*) as count FROM jobs", &[])
        .await
        .unwrap();

    assert_eq!(row.get::<_, Option<i64>>(0).unwrap(), 10);
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
    job.enqueue(app_state, context.to_string()).await.unwrap();

    // Verify context is stored
    let client = pool.get().await.unwrap();
    let row = client
        .query_one(
            "SELECT context FROM jobs WHERE name = $1",
            &[&<TestJob as Job<crate::common::app::TestAppState>>::NAME],
        )
        .await
        .unwrap();

    assert_eq!(row.get::<_, String>(0), context);
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
    let result = job.enqueue(app_state, "test-failing".to_string()).await;
    assert!(result.is_ok());

    // Verify job is in database with initial error_count of 0
    let client = pool.get().await.unwrap();
    let row = client
        .query_one(
            "SELECT name, error_count, last_error_message, last_failed_at FROM jobs WHERE name = $1",
            &[&<FailingJob as Job<crate::common::app::TestAppState>>::NAME],
        )
        .await
        .unwrap();

    assert_eq!(row.get::<_, String>(0), "FailingJob");
    assert_eq!(row.get::<_, i32>(1), 0);
    assert!(row.get::<_, Option<String>>(2).is_none());
    assert!(
        row.get::<_, Option<chrono::DateTime<chrono::Utc>>>(3)
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

    job.enqueue(app_state, "test-context".to_string())
        .await
        .unwrap();

    // Verify error tracking fields exist and have correct defaults
    let client = pool.get().await.unwrap();
    let row = client
        .query_one(
            "SELECT error_count, last_error_message, last_failed_at FROM jobs WHERE name = $1",
            &[&<TestJob as Job<crate::common::app::TestAppState>>::NAME],
        )
        .await
        .unwrap();

    assert_eq!(row.get::<_, i32>(0), 0);
    assert!(row.get::<_, Option<String>>(1).is_none());
    assert!(
        row.get::<_, Option<chrono::DateTime<chrono::Utc>>>(2)
            .is_none()
    );
}

#[tokio::test]
async fn test_job_error_count_increment() {
    let (pool, _guard) = crate::common::db::setup_test_db().await.unwrap();

    // Directly insert a job and simulate a failure update
    let job_id = uuid::Uuid::new_v4();
    let worker_id = "test-worker";
    let now = chrono::Utc::now();

    let client = pool.get().await.unwrap();
    client
        .execute(
            "INSERT INTO jobs (job_id, name, payload, priority, run_at, created_at, context, error_count, locked_by, locked_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
            &[
                &job_id,
                &"TestJob",
                &serde_json::json!({"id": "test", "value": 1}),
                &0i32,
                &now,
                &now,
                &"test",
                &0i32,
                &worker_id,
                &now,
            ],
        )
        .await
        .unwrap();

    // Simulate job failure with exponential backoff (like the worker does)
    let error_message = "Test error occurred";
    client
        .execute(
            "UPDATE jobs
             SET locked_by = NULL,
                 locked_at = NULL,
                 error_count = error_count + 1,
                 last_error_message = $3,
                 last_failed_at = NOW(),
                 run_at = NOW() + (POWER(2, error_count + 1)) * interval '1 second'
             WHERE job_id = $1 AND locked_by = $2",
            &[&job_id, &worker_id, &error_message],
        )
        .await
        .unwrap();

    // Verify error tracking was updated
    let row = client
        .query_one(
            "SELECT error_count, last_error_message, last_failed_at, run_at FROM jobs WHERE job_id = $1",
            &[&job_id],
        )
        .await
        .unwrap();

    assert_eq!(row.get::<_, i32>(0), 1);
    assert_eq!(
        row.get::<_, Option<String>>(1),
        Some(error_message.to_string())
    );
    assert!(
        row.get::<_, Option<chrono::DateTime<chrono::Utc>>>(2)
            .is_some()
    );

    // Verify run_at was pushed forward (should be at least 2 seconds in future)
    let run_at: chrono::DateTime<chrono::Utc> = row.get(3);
    assert!(run_at > chrono::Utc::now());
}

#[tokio::test]
async fn test_exponential_backoff_calculation() {
    let (pool, _guard) = crate::common::db::setup_test_db().await.unwrap();

    // Test that exponential backoff follows 2^(error_count + 1) formula
    let job_id = uuid::Uuid::new_v4();
    let worker_id = "test-worker";
    let now = chrono::Utc::now();

    // Start with error_count = 5 to test higher backoff values
    let client = pool.get().await.unwrap();
    client
        .execute(
            "INSERT INTO jobs (job_id, name, payload, priority, run_at, created_at, context, error_count, locked_by, locked_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
            &[
                &job_id,
                &"TestJob",
                &serde_json::json!({"id": "test", "value": 1}),
                &0i32,
                &now,
                &now,
                &"test",
                &5i32, // error_count = 5
                &worker_id,
                &now,
            ],
        )
        .await
        .unwrap();

    let before_update = chrono::Utc::now();

    // Simulate failure - should set run_at to NOW() + 2^6 seconds = 64 seconds
    client
        .execute(
            "UPDATE jobs
             SET locked_by = NULL,
                 locked_at = NULL,
                 error_count = error_count + 1,
                 last_error_message = $3,
                 last_failed_at = NOW(),
                 run_at = NOW() + (POWER(2, error_count + 1)) * interval '1 second'
             WHERE job_id = $1 AND locked_by = $2",
            &[&job_id, &worker_id, &"test error"],
        )
        .await
        .unwrap();

    let row = client
        .query_one(
            "SELECT error_count, run_at FROM jobs WHERE job_id = $1",
            &[&job_id],
        )
        .await
        .unwrap();

    assert_eq!(row.get::<_, i32>(0), 6);

    // run_at should be approximately 64 seconds (2^6) in the future
    // Allow some tolerance for test execution time
    let run_at: chrono::DateTime<chrono::Utc> = row.get(1);
    let actual_delay = run_at - before_update;

    // Should be between 63 and 66 seconds to account for timing
    assert!(
        actual_delay >= chrono::Duration::seconds(63)
            && actual_delay <= chrono::Duration::seconds(66),
        "Expected delay ~64 seconds, got {actual_delay:?}"
    );
}

#[tokio::test]
async fn test_max_retries_exceeded_deletes_job() {
    let (pool, _guard) = crate::common::db::setup_test_db().await.unwrap();

    let job_id = uuid::Uuid::new_v4();
    let worker_id = "test-worker";
    let max_retries = 20;
    let now = chrono::Utc::now();

    // Insert a job that has already hit max retries
    let client = pool.get().await.unwrap();
    client
        .execute(
            "INSERT INTO jobs (job_id, name, payload, priority, run_at, created_at, context, error_count, locked_by, locked_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
            &[
                &job_id,
                &"TestJob",
                &serde_json::json!({"id": "test", "value": 1}),
                &0i32,
                &now,
                &now,
                &"test",
                &max_retries, // At max retries
                &worker_id,
                &now,
            ],
        )
        .await
        .unwrap();

    // Verify job exists
    let count_before = client
        .query_one(
            "SELECT COUNT(*) as count FROM jobs WHERE job_id = $1",
            &[&job_id],
        )
        .await
        .unwrap()
        .get::<_, Option<i64>>(0)
        .unwrap();
    assert_eq!(count_before, 1);

    // Simulate the worker deleting the job after max retries
    client
        .execute(
            "DELETE FROM jobs WHERE job_id = $1 AND locked_by = $2",
            &[&job_id, &worker_id],
        )
        .await
        .unwrap();

    // Verify job was deleted
    let count_after = client
        .query_one(
            "SELECT COUNT(*) as count FROM jobs WHERE job_id = $1",
            &[&job_id],
        )
        .await
        .unwrap()
        .get::<_, Option<i64>>(0)
        .unwrap();
    assert_eq!(count_after, 0);
}
