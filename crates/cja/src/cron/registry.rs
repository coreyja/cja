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

        async fn run(&self, _app_state: TestAppState) -> crate::Result<()> {
            Ok(())
        }
    }

    async fn create_test_db(db_name: &str) {
        let setup_db = sqlx::PgPool::connect("postgresql://localhost/postgres")
            .await
            .unwrap();
        sqlx::query(&format!("DROP DATABASE IF EXISTS {db_name}"))
            .execute(&setup_db)
            .await
            .unwrap();
        sqlx::query(&format!("CREATE DATABASE {db_name}"))
            .execute(&setup_db)
            .await
            .unwrap();
        setup_db.close().await;
    }

    async fn drop_test_db(db_name: &str) {
        let setup_db = sqlx::PgPool::connect("postgresql://localhost/postgres")
            .await
            .unwrap();
        sqlx::query(&format!("DROP DATABASE {db_name}"))
            .execute(&setup_db)
            .await
            .unwrap();
    }

    async fn run_test<F, Fut>(test: F)
    where
        F: FnOnce(TestAppState) -> Fut,
        Fut: Future<Output = ()>,
    {
        let uuid = uuid::Uuid::new_v4().to_string().replace('-', "_");
        let db_name = format!("cja_test_{uuid}");
        create_test_db(&db_name).await;

        let db = sqlx::PgPool::connect(&format!("postgresql://localhost/{db_name}"))
            .await
            .unwrap();
        crate::db::run_migrations(&db).await.unwrap();

        let app_state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };

        test(app_state).await;

        db.close().await;

        drop_test_db(&db_name).await;
    }

    #[tokio::test]
    async fn test_tick_creates_new_cron_record() {
        run_test(async move |app_state: TestAppState| {
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
                "Record should not exist {} in DB {:?}",
                existing_record.unwrap().cron_id,
                app_state.db
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
        })
        .await;
    }

    #[tokio::test]
    async fn test_tick_updates_existing_cron_record() {
        run_test(async move |app_state: TestAppState| {
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
}
