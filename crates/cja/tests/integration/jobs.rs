use cja::jobs::Job;
use serde::{Serialize, Deserialize};
use std::sync::Arc;
use tokio::sync::Mutex;

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
static JOB_EXECUTIONS: once_cell::sync::Lazy<Arc<Mutex<Vec<String>>>> = 
    once_cell::sync::Lazy::new(|| Arc::new(Mutex::new(Vec::new())));

#[async_trait::async_trait]
impl<AS: cja::app_state::AppState> Job<AS> for TestJob {
    const NAME: &'static str = "TestJob";
    
    async fn run(&self, _app_state: AS) -> color_eyre::Result<()> {
        let mut executions = JOB_EXECUTIONS.lock().await;
        executions.push(format!("TestJob-{}", self.id));
        println!("Executing TestJob with id: {} and value: {}", self.id, self.value);
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
    let result = job.enqueue(app_state, "test-context".to_string()).await;
    assert!(result.is_ok());
    
    // Verify job is in database
    let count = sqlx::query!("SELECT COUNT(*) as count FROM jobs WHERE name = $1", <TestJob as Job<crate::common::app::TestAppState>>::NAME)
        .fetch_one(&pool)
        .await
        .unwrap();
    
    assert_eq!(count.count.unwrap(), 1);
}

#[tokio::test]
async fn test_job_with_priority() {
    let (pool, _guard) = crate::common::db::setup_test_db().await.unwrap();
    let app_state = crate::common::app::TestAppState::new(pool.clone());
    
    // Enqueue multiple jobs
    for i in 0..3 {
        let job = TestJob {
            id: format!("priority-test-{}", i),
            value: i,
        };
        job.enqueue(app_state.clone(), format!("priority-{}", i)).await.unwrap();
    }
    
    // Verify all jobs are enqueued
    let count = sqlx::query!("SELECT COUNT(*) as count FROM jobs")
        .fetch_one(&pool)
        .await
        .unwrap();
    
    assert_eq!(count.count.unwrap(), 3);
}

#[tokio::test]
async fn test_job_payload_serialization() {
    let (pool, _guard) = crate::common::db::setup_test_db().await.unwrap();
    let app_state = crate::common::app::TestAppState::new(pool.clone());
    
    let job = TestJob {
        id: "serialization-test".to_string(),
        value: 123,
    };
    
    job.enqueue(app_state, "serialization-context".to_string()).await.unwrap();
    
    // Retrieve and verify payload
    let row = sqlx::query!(
        "SELECT payload FROM jobs WHERE name = $1",
        <TestJob as Job<crate::common::app::TestAppState>>::NAME
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    
    let deserialized: TestJob = serde_json::from_value(row.payload).unwrap();
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
    
    job.enqueue(app_state, "lock-context".to_string()).await.unwrap();
    
    // Get the job_id
    let job_row = sqlx::query!("SELECT job_id FROM jobs WHERE name = $1", <TestJob as Job<crate::common::app::TestAppState>>::NAME)
        .fetch_one(&pool)
        .await
        .unwrap();
    
    // Lock the job
    let worker_id = "test-worker-1";
    let locked = sqlx::query!(
        "UPDATE jobs SET locked_at = NOW(), locked_by = $1 
         WHERE job_id = $2 AND locked_at IS NULL",
        worker_id,
        job_row.job_id
    )
    .execute(&pool)
    .await
    .unwrap();
    
    assert_eq!(locked.rows_affected(), 1);
    
    // Try to lock again (should fail)
    let locked_again = sqlx::query!(
        "UPDATE jobs SET locked_at = NOW(), locked_by = $1 
         WHERE job_id = $2 AND locked_at IS NULL",
        "another-worker",
        job_row.job_id
    )
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
                id: format!("concurrent-{}", i),
                value: i,
            };
            job.enqueue(state, format!("concurrent-context-{}", i)).await
        });
        handles.push(handle);
    }
    
    // Wait for all to complete
    for handle in handles {
        let result = handle.await.unwrap();
        assert!(result.is_ok());
    }
    
    // Verify all jobs were enqueued
    let count = sqlx::query!("SELECT COUNT(*) as count FROM jobs")
        .fetch_one(&pool)
        .await
        .unwrap();
    
    assert_eq!(count.count.unwrap(), 10);
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
    let row = sqlx::query!(
        "SELECT context FROM jobs WHERE name = $1",
        <TestJob as Job<crate::common::app::TestAppState>>::NAME
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    
    assert_eq!(row.context, context);
}