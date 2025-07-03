use std::{collections::HashMap, error::Error, future::Future, pin::Pin, time::Duration};

use chrono::{OutOfRangeError, Utc};
use tracing::error;

use crate::app_state::AppState as AS;
#[cfg(feature = "jobs")]
use crate::jobs::Job;

pub struct CronRegistry<AppState: AS> {
    pub(super) jobs: HashMap<&'static str, CronJob<AppState>>,
}

#[async_trait::async_trait]
pub trait CronFn<AppState: AS> {
    // This collapses the error type to a string, right now thats because thats
    // what the only consumer really needs. As we add more error debugging we'll
    // need to change this.
    async fn run(&self, app_state: AppState, context: String) -> Result<(), String>;
}

pub struct CronFnClosure<
    AppState: AS,
    FnError: Error + Send + Sync + 'static,
    F: Fn(AppState, String) -> Pin<Box<dyn Future<Output = Result<(), FnError>> + Send>>
        + Send
        + Sync
        + 'static,
> {
    pub(super) func: F,
    _marker: std::marker::PhantomData<AppState>,
}

#[async_trait::async_trait]
impl<
    AppState: AS,
    FnError: Error + Send + Sync + 'static,
    F: Fn(AppState, String) -> Pin<Box<dyn Future<Output = Result<(), FnError>> + Send>>
        + Send
        + Sync
        + 'static,
> CronFn<AppState> for CronFnClosure<AppState, FnError, F>
{
    async fn run(&self, app_state: AppState, context: String) -> Result<(), String> {
        (self.func)(app_state, context)
            .await
            .map_err(|err| format!("{err:?}"))
    }
}

#[allow(clippy::type_complexity)]
pub(super) struct CronJob<AppState: AS> {
    name: &'static str,
    func: Box<dyn CronFn<AppState> + Send + Sync + 'static>,
    interval: Duration,
}

#[derive(Debug, thiserror::Error)]
#[error("TickError: {0}")]
pub enum TickError {
    JobError(String),
    SqlxError(sqlx::Error),
    NegativeDuration(OutOfRangeError),
}

impl<AppState: AS> CronJob<AppState> {
    #[tracing::instrument(
        name = "cron_job.tick",
        skip_all,
        fields(
            cron_job.name = self.name,
            cron_job.interval = ?self.interval
        )
    )]
    pub(crate) async fn tick(
        &self,
        app_state: AppState,
        last_enqueue_map: &HashMap<String, chrono::DateTime<Utc>>,
    ) -> Result<(), TickError> {
        let last_enqueue = last_enqueue_map.get(self.name);
        let context = format!("Cron@{}", app_state.version());
        let now = Utc::now();

        if let Some(last_enqueue) = last_enqueue {
            let elapsed = now - last_enqueue;
            let elapsed = elapsed.to_std().map_err(TickError::NegativeDuration)?;
            if elapsed <= self.interval {
                return Ok(());
            }
            tracing::info!(
                task_name = self.name,
                time_since_last_run =? elapsed,
                "Enqueuing Task"
            );
            (self.func)
                .run(app_state.clone(), context)
                .await
                .map_err(TickError::JobError)?;
        } else {
            tracing::info!(task_name = self.name, "Enqueuing Task for first time");
            (self.func)
                .run(app_state.clone(), context)
                .await
                .map_err(TickError::JobError)?;
        }
        sqlx::query!(
            "INSERT INTO Crons (cron_id, name, last_run_at, created_at, updated_at)
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (name)
            DO UPDATE SET
            last_run_at = $3",
            uuid::Uuid::new_v4(),
            self.name,
            now,
            now,
            now
        )
        .execute(app_state.db())
        .await
        .map_err(TickError::SqlxError)?;

        Ok(())
    }
}

impl<AppState: AS> CronRegistry<AppState> {
    pub fn new() -> Self {
        Self {
            jobs: HashMap::new(),
        }
    }

    #[tracing::instrument(name = "cron.register", skip_all, fields(cron_job.name = name, cron_job.interval = ?interval))]
    pub fn register<FnError: Error + Send + Sync + 'static>(
        &mut self,
        name: &'static str,
        interval: Duration,
        job: impl Fn(AppState, String) -> Pin<Box<dyn Future<Output = Result<(), FnError>> + Send>>
        + Send
        + Sync
        + 'static,
    ) {
        let cron_job = CronJob {
            name,
            func: Box::new(CronFnClosure {
                func: job,
                _marker: std::marker::PhantomData,
            }),
            interval,
        };
        self.jobs.insert(name, cron_job);
    }

    #[cfg(feature = "jobs")]
    #[tracing::instrument(name = "cron.register_job", skip_all, fields(cron_job.name = J::NAME, cron_job.interval = ?interval))]
    pub fn register_job<J: Job<AppState>>(&mut self, job: J, interval: Duration) {
        self.register(J::NAME, interval, move |app_state, context| {
            J::enqueue(job.clone(), app_state, context)
        });
    }
}

impl<AppState: AS> Default for CronRegistry<AppState> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod test {
    use crate::app_state::AppState;
    use crate::server::cookies::CookieKey;

    use super::*;

    #[derive(Clone)]
    struct TestAppState {
        db: sqlx::PgPool,
        cookie_key: CookieKey,
    }

    impl AppState for TestAppState {
        fn db(&self) -> &sqlx::PgPool {
            &self.db
        }

        fn version(&self) -> &'static str {
            "test"
        }

        fn cookie_key(&self) -> &CookieKey {
            &self.cookie_key
        }
    }

    #[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
    struct TestJob;

    #[async_trait::async_trait]
    impl Job<TestAppState> for TestJob {
        const NAME: &'static str = "test_job";

        async fn run(&self, _app_state: TestAppState) -> color_eyre::Result<()> {
            Ok(())
        }
    }

    #[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
    struct FailingJob;

    #[async_trait::async_trait]
    impl Job<TestAppState> for FailingJob {
        const NAME: &'static str = "failing_job";

        async fn run(&self, _app_state: TestAppState) -> color_eyre::Result<()> {
            Err(color_eyre::eyre::eyre!("Test error"))
        }
    }

    #[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
    struct SecondTestJob;

    #[async_trait::async_trait]
    impl Job<TestAppState> for SecondTestJob {
        const NAME: &'static str = "second_test_job";

        async fn run(&self, _app_state: TestAppState) -> color_eyre::Result<()> {
            Ok(())
        }
    }

    #[sqlx::test]
    async fn test_tick_creates_new_cron_record(db: sqlx::PgPool) {
        let app_state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };
        let mut registry = CronRegistry::new();
        registry.register_job(TestJob, Duration::from_secs(1));

        let cron_job = registry.jobs.get(TestJob::NAME).unwrap();
        assert_eq!(cron_job.name, TestJob::NAME);
        assert_eq!(cron_job.interval, Duration::from_secs(1));

        let worker = crate::cron::Worker::new(app_state.clone(), registry);

        let existing_record =
            sqlx::query!("SELECT cron_id FROM Crons where name = $1", TestJob::NAME)
                .fetch_optional(&app_state.db)
                .await
                .unwrap();
        assert!(
            existing_record.is_none(),
            "Record should not exist {}",
            existing_record.unwrap().cron_id
        );

        worker.tick().await.unwrap();

        let last_run = sqlx::query!(
            "SELECT last_run_at FROM Crons WHERE name = $1",
            TestJob::NAME
        )
        .fetch_one(&app_state.db)
        .await
        .unwrap();

        let now = Utc::now();
        let last_run_at = last_run.last_run_at;
        let diff = now.signed_duration_since(last_run_at);
        assert!(diff.num_milliseconds() < 1000);
    }

    #[sqlx::test]
    async fn test_tick_skips_updating_existing_cron_record(db: sqlx::PgPool) {
        let app_state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };
        let mut registry = CronRegistry::new();
        registry.register_job(TestJob, Duration::from_secs(60));
        let worker = crate::cron::Worker::new(app_state.clone(), registry);

        let previously = Utc::now();
        sqlx::query!(
            "INSERT INTO Crons (cron_id, name, last_run_at, created_at, updated_at)
            VALUES ($1, $2, $3, $3, $3)
            ON CONFLICT (name)
            DO UPDATE SET
            last_run_at = $3",
            uuid::Uuid::new_v4(),
            TestJob::NAME,
            previously
        )
        .execute(&app_state.db)
        .await
        .unwrap();

        worker.tick().await.unwrap();

        let last_run = sqlx::query!(
            "SELECT last_run_at FROM Crons WHERE name = $1",
            TestJob::NAME
        )
        .fetch_one(&app_state.db)
        .await
        .unwrap();

        assert_eq!(last_run.last_run_at, previously);
    }

    #[sqlx::test]
    async fn test_tick_updates_cron_record_when_interval_elapsed(db: sqlx::PgPool) {
        let app_state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };
        let mut registry = CronRegistry::new();
        registry.register_job(TestJob, Duration::from_secs(1));
        let worker = crate::cron::Worker::new(app_state.clone(), registry);

        let two_seconds_ago = Utc::now() - chrono::Duration::seconds(2);
        sqlx::query!(
            "INSERT INTO Crons (cron_id, name, last_run_at, created_at, updated_at)
            VALUES ($1, $2, $3, $3, $3)",
            uuid::Uuid::new_v4(),
            TestJob::NAME,
            two_seconds_ago
        )
        .execute(&app_state.db)
        .await
        .unwrap();

        worker.tick().await.unwrap();

        let last_run = sqlx::query!(
            "SELECT last_run_at FROM Crons WHERE name = $1",
            TestJob::NAME
        )
        .fetch_one(&app_state.db)
        .await
        .unwrap();

        assert!(last_run.last_run_at > two_seconds_ago);
        let diff = Utc::now().signed_duration_since(last_run.last_run_at);
        assert!(diff.num_milliseconds() < 1000);
    }

    #[sqlx::test]
    async fn test_tick_enqueues_failing_job_successfully(db: sqlx::PgPool) {
        let app_state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };
        let mut registry = CronRegistry::new();
        registry.register_job(FailingJob, Duration::from_secs(1));

        let cron_job = registry.jobs.get(FailingJob::NAME).unwrap();
        let last_enqueue_map = HashMap::new();

        let result = cron_job.tick(app_state.clone(), &last_enqueue_map).await;
        assert!(result.is_ok());

        let cron_record = sqlx::query!(
            "SELECT last_run_at FROM Crons WHERE name = $1",
            FailingJob::NAME
        )
        .fetch_one(&app_state.db)
        .await
        .unwrap();

        let now = Utc::now();
        let diff = now.signed_duration_since(cron_record.last_run_at);
        assert!(diff.num_milliseconds() < 1000);

        let job_count = sqlx::query!(
            "SELECT COUNT(*) as count FROM jobs WHERE name = $1",
            FailingJob::NAME
        )
        .fetch_one(&app_state.db)
        .await
        .unwrap();
        assert_eq!(job_count.count.unwrap(), 1);
    }

    #[sqlx::test]
    async fn test_tick_with_custom_function_error(db: sqlx::PgPool) {
        let app_state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };
        #[derive(Debug, thiserror::Error)]
        #[error("Custom function error")]
        struct CustomError;

        let mut registry = CronRegistry::new();
        registry.register(
            "custom_failing",
            Duration::from_secs(1),
            |_app_state, _context| Box::pin(async { Err(CustomError) }),
        );

        let cron_job = registry.jobs.get("custom_failing").unwrap();
        let last_enqueue_map = HashMap::new();

        let result = cron_job.tick(app_state.clone(), &last_enqueue_map).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            TickError::JobError(err) => {
                assert!(err.contains("CustomError"));
            }
            _ => panic!("Expected JobError"),
        }

        let cron_count = sqlx::query!(
            "SELECT COUNT(*) as count FROM Crons WHERE name = $1",
            "custom_failing"
        )
        .fetch_one(&app_state.db)
        .await
        .unwrap();
        assert_eq!(cron_count.count.unwrap(), 0);
    }

    #[sqlx::test]
    async fn test_worker_tick_with_multiple_jobs(db: sqlx::PgPool) {
        let app_state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };
        let mut registry = CronRegistry::new();
        registry.register_job(TestJob, Duration::from_secs(1));
        registry.register_job(SecondTestJob, Duration::from_secs(1));

        assert_eq!(registry.jobs.len(), 2);

        let worker = crate::cron::Worker::new(app_state.clone(), registry);

        worker.tick().await.unwrap();

        let test_job_record = sqlx::query!(
            "SELECT last_run_at FROM Crons WHERE name = $1",
            TestJob::NAME
        )
        .fetch_one(&app_state.db)
        .await
        .unwrap();

        let second_job_record = sqlx::query!(
            "SELECT last_run_at FROM Crons WHERE name = $1",
            SecondTestJob::NAME
        )
        .fetch_one(&app_state.db)
        .await
        .unwrap();

        let now = Utc::now();
        let diff1 = now.signed_duration_since(test_job_record.last_run_at);
        let diff2 = now.signed_duration_since(second_job_record.last_run_at);

        assert!(diff1.num_milliseconds() < 1000);
        assert!(diff2.num_milliseconds() < 1000);
    }

    #[sqlx::test]
    async fn test_worker_respects_existing_last_run_times(db: sqlx::PgPool) {
        let app_state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };
        let mut registry = CronRegistry::new();
        registry.register_job(TestJob, Duration::from_secs(10));
        registry.register_job(SecondTestJob, Duration::from_secs(5));

        let worker = crate::cron::Worker::new(app_state.clone(), registry);

        let recent_time = Utc::now() - chrono::Duration::seconds(3);
        let old_time = Utc::now() - chrono::Duration::seconds(10);

        sqlx::query!(
            "INSERT INTO Crons (cron_id, name, last_run_at, created_at, updated_at)
                VALUES ($1, $2, $3, $3, $3)",
            uuid::Uuid::new_v4(),
            TestJob::NAME,
            recent_time
        )
        .execute(&app_state.db)
        .await
        .unwrap();

        sqlx::query!(
            "INSERT INTO Crons (cron_id, name, last_run_at, created_at, updated_at)
                VALUES ($1, $2, $3, $3, $3)",
            uuid::Uuid::new_v4(),
            SecondTestJob::NAME,
            old_time
        )
        .execute(&app_state.db)
        .await
        .unwrap();

        worker.tick().await.unwrap();

        let test_job_record = sqlx::query!(
            "SELECT last_run_at FROM Crons WHERE name = $1",
            TestJob::NAME
        )
        .fetch_one(&app_state.db)
        .await
        .unwrap();

        let second_job_record = sqlx::query!(
            "SELECT last_run_at FROM Crons WHERE name = $1",
            SecondTestJob::NAME
        )
        .fetch_one(&app_state.db)
        .await
        .unwrap();

        assert_eq!(test_job_record.last_run_at, recent_time);
        assert!(second_job_record.last_run_at > old_time);
    }

    #[sqlx::test]
    async fn test_tick_handles_future_last_run_time(db: sqlx::PgPool) {
        let app_state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };
        let mut registry = CronRegistry::new();
        registry.register_job(TestJob, Duration::from_secs(1));

        let cron_job = registry.jobs.get(TestJob::NAME).unwrap();

        let future_time = Utc::now() + chrono::Duration::hours(1);
        let mut last_enqueue_map = HashMap::new();
        last_enqueue_map.insert(TestJob::NAME.to_string(), future_time);

        let result = cron_job.tick(app_state.clone(), &last_enqueue_map).await;

        match result {
            Err(TickError::NegativeDuration(_)) => {}
            Ok(()) => {
                let cron_count = sqlx::query!(
                    "SELECT COUNT(*) as count FROM Crons WHERE name = $1",
                    TestJob::NAME
                )
                .fetch_one(&app_state.db)
                .await
                .unwrap();
                assert_eq!(
                    cron_count.count.unwrap(),
                    0,
                    "Should not have created cron record"
                );
            }
            Err(e) => panic!("Expected NegativeDuration error or no action, got: {e:?}"),
        }
    }
}
