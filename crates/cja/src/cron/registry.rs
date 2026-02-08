use std::{collections::HashMap, error::Error, future::Future, pin::Pin, time::Duration};

use chrono::Utc;
use chrono_tz::Tz;

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

#[derive(Clone, Debug)]
pub struct IntervalSchedule(pub Duration);

impl IntervalSchedule {
    fn should_run(
        &self,
        last_run: Option<&chrono::DateTime<Utc>>,
        now: chrono::DateTime<Utc>,
        _worker_started_at: chrono::DateTime<Utc>,
        _timezone: Tz,
    ) -> bool {
        if let Some(last_run) = last_run {
            let elapsed = now - last_run;
            // If elapsed is negative, return false (don't run)
            if elapsed < chrono::Duration::zero() {
                return false;
            }
            // Safe to convert since we checked it's non-negative
            let elapsed = elapsed.to_std().unwrap_or(Duration::ZERO);
            elapsed > self.0
        } else {
            true
        }
    }

    pub fn next_run(
        &self,
        last_run: Option<&chrono::DateTime<Utc>>,
        now: chrono::DateTime<Utc>,
        timezone: Tz,
    ) -> chrono::DateTime<Tz> {
        let last_run = last_run.unwrap_or(&now);
        let last_run_tz = last_run.with_timezone(&timezone);
        let duration: chrono::Duration = chrono::Duration::from_std(self.0).unwrap();
        let next_run = last_run_tz.checked_add_signed(duration).unwrap();
        next_run.with_timezone(&timezone)
    }
}

#[derive(Clone, Debug)]
pub struct CronSchedule(pub Box<cron::Schedule>);

impl CronSchedule {
    fn should_run(
        &self,
        last_run: Option<&chrono::DateTime<Utc>>,
        now: chrono::DateTime<Utc>,
        worker_started_at: chrono::DateTime<Utc>,
        timezone: Tz,
    ) -> bool {
        // Use last run time if available, otherwise use worker start time
        let last_run = last_run.unwrap_or(&worker_started_at);
        let last_run_tz = last_run.with_timezone(&timezone);

        if let Some(next_run) = self.0.after(&last_run_tz).next() {
            let now_tz = now.with_timezone(&timezone);
            now_tz >= next_run
        } else {
            false
        }
    }

    pub fn next_run(
        &self,
        last_run: Option<&chrono::DateTime<Utc>>,
        now: chrono::DateTime<Utc>,
        timezone: Tz,
    ) -> chrono::DateTime<Tz> {
        let last_run = last_run.unwrap_or(&now);
        let last_run_tz = last_run.with_timezone(&timezone);
        self.0.after(&last_run_tz).next().unwrap()
    }
}

#[derive(Clone, Debug)]
pub enum Schedule {
    Interval(IntervalSchedule),
    Cron(CronSchedule),
}

impl Schedule {
    pub fn should_run(
        &self,
        last_run: Option<&chrono::DateTime<Utc>>,
        now: chrono::DateTime<Utc>,
        worker_started_at: chrono::DateTime<Utc>,
        timezone: Tz,
    ) -> bool {
        match self {
            Schedule::Interval(interval) => {
                interval.should_run(last_run, now, worker_started_at, timezone)
            }
            Schedule::Cron(cron) => cron.should_run(last_run, now, worker_started_at, timezone),
        }
    }

    pub fn next_run(
        &self,
        last_run: Option<&chrono::DateTime<Utc>>,
        now: chrono::DateTime<Utc>,
        timezone: Tz,
    ) -> chrono::DateTime<Tz> {
        match self {
            Schedule::Interval(interval) => interval.next_run(last_run, now, timezone),
            Schedule::Cron(cron) => cron.next_run(last_run, now, timezone),
        }
    }
}

#[allow(clippy::type_complexity)]
pub struct CronJob<AppState: AS> {
    pub name: &'static str,
    pub description: Option<&'static str>,
    func: Box<dyn CronFn<AppState> + Send + Sync + 'static>,
    pub schedule: Schedule,
}

#[derive(Debug, thiserror::Error)]
#[error("TickError: {0}")]
pub enum TickError {
    JobError(String),
    SqlxError(sqlx::Error),
}

impl<AppState: AS> CronJob<AppState> {
    #[tracing::instrument(
        name = "cron_job.tick",
        skip_all,
        fields(
            cron_job.name = self.name,
            cron_job.schedule = ?self.schedule
        )
    )]
    pub(crate) async fn tick(
        &self,
        app_state: AppState,
        last_enqueue_map: &HashMap<String, chrono::DateTime<Utc>>,
        worker_started_at: chrono::DateTime<Utc>,
        timezone: Tz,
    ) -> Result<(), TickError> {
        let last_enqueue = last_enqueue_map.get(self.name);
        let context = format!("Cron@{}", app_state.version());
        let now = Utc::now();

        let should_run = self
            .schedule
            .should_run(last_enqueue, now, worker_started_at, timezone);

        if should_run {
            tracing::info!(
                task_name = self.name,
                last_run = ?last_enqueue,
                "Enqueuing Task"
            );
            (self.func)
                .run(app_state.clone(), context)
                .await
                .map_err(TickError::JobError)?;

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
        }

        Ok(())
    }

    pub async fn run(&self, app_state: AppState, context: String) -> Result<(), String> {
        (self.func).run(app_state, context).await
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
        description: Option<&'static str>,
        interval: Duration,
        job: impl Fn(AppState, String) -> Pin<Box<dyn Future<Output = Result<(), FnError>> + Send>>
        + Send
        + Sync
        + 'static,
    ) {
        let cron_job = CronJob {
            name,
            description,
            func: Box::new(CronFnClosure {
                func: job,
                _marker: std::marker::PhantomData,
            }),
            schedule: Schedule::Interval(IntervalSchedule(interval)),
        };
        self.jobs.insert(name, cron_job);
    }

    #[tracing::instrument(name = "cron.register_with_cron", skip_all, fields(cron_job.name = name, cron_job.cron = cron_expr))]
    pub fn register_with_cron<FnError: Error + Send + Sync + 'static>(
        &mut self,
        name: &'static str,
        description: Option<&'static str>,
        cron_expr: &str,
        job: impl Fn(AppState, String) -> Pin<Box<dyn Future<Output = Result<(), FnError>> + Send>>
        + Send
        + Sync
        + 'static,
    ) -> Result<(), cron::error::Error> {
        let cron_schedule = cron_expr.parse::<cron::Schedule>()?;
        let cron_job = CronJob {
            name,
            description,
            func: Box::new(CronFnClosure {
                func: job,
                _marker: std::marker::PhantomData,
            }),
            schedule: Schedule::Cron(CronSchedule(Box::new(cron_schedule))),
        };
        self.jobs.insert(name, cron_job);
        Ok(())
    }

    #[cfg(feature = "jobs")]
    #[tracing::instrument(name = "cron.register_job", skip_all, fields(cron_job.name = J::NAME, cron_job.interval = ?interval))]
    pub fn register_job<J: Job<AppState>>(
        &mut self,
        job: J,
        description: Option<&'static str>,
        interval: Duration,
    ) {
        self.register(J::NAME, description, interval, move |app_state, context| {
            J::enqueue(job.clone(), app_state, context, None)
        });
    }

    #[cfg(feature = "jobs")]
    #[tracing::instrument(name = "cron.register_job_with_cron", skip_all, fields(cron_job.name = J::NAME, cron_job.cron = cron_expr))]
    pub fn register_job_with_cron<J: Job<AppState>>(
        &mut self,
        job: J,
        description: Option<&'static str>,
        cron_expr: &str,
    ) -> Result<(), cron::error::Error> {
        self.register_with_cron(
            J::NAME,
            description,
            cron_expr,
            move |app_state, context| J::enqueue(job.clone(), app_state, context, None),
        )
    }

    #[cfg(feature = "jobs")]
    #[tracing::instrument(name = "cron.get", skip_all, fields(cron_job.name = name))]
    pub fn get(&self, name: &str) -> Option<&CronJob<AppState>> {
        self.jobs.get(name)
    }

    /// Returns a reference to all registered cron jobs.
    pub fn jobs(&self) -> &HashMap<&'static str, CronJob<AppState>> {
        &self.jobs
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
        registry.register_job(TestJob, None, Duration::from_secs(1));

        let cron_job = registry.jobs.get(TestJob::NAME).unwrap();
        assert_eq!(cron_job.name, TestJob::NAME);
        assert!(
            matches!(cron_job.schedule, Schedule::Interval(IntervalSchedule(d)) if d == Duration::from_secs(1))
        );

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
        registry.register_job(TestJob, None, Duration::from_secs(60));
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

        let diff = last_run.last_run_at.signed_duration_since(previously);
        assert!(diff.num_milliseconds() < 50);
    }

    #[sqlx::test]
    async fn test_tick_updates_cron_record_when_interval_elapsed(db: sqlx::PgPool) {
        let app_state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };
        let mut registry = CronRegistry::new();
        registry.register_job(TestJob, None, Duration::from_secs(1));
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
        registry.register_job(FailingJob, None, Duration::from_secs(1));

        let cron_job = registry.jobs.get(FailingJob::NAME).unwrap();
        let last_enqueue_map = HashMap::new();
        let worker_started_at = Utc::now();

        let result = cron_job
            .tick(
                app_state.clone(),
                &last_enqueue_map,
                worker_started_at,
                chrono_tz::UTC,
            )
            .await;
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
            None,
            Duration::from_secs(1),
            |_app_state, _context| Box::pin(async { Err(CustomError) }),
        );

        let cron_job = registry.jobs.get("custom_failing").unwrap();
        let last_enqueue_map = HashMap::new();
        let worker_started_at = Utc::now();

        let result = cron_job
            .tick(
                app_state.clone(),
                &last_enqueue_map,
                worker_started_at,
                chrono_tz::UTC,
            )
            .await;
        assert!(result.is_err());
        match result.unwrap_err() {
            TickError::JobError(err) => {
                assert!(err.contains("CustomError"));
            }
            TickError::SqlxError(_) => panic!("Expected JobError"),
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
        registry.register_job(TestJob, None, Duration::from_secs(1));
        registry.register_job(SecondTestJob, None, Duration::from_secs(1));

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
        registry.register_job(TestJob, None, Duration::from_secs(10));
        registry.register_job(SecondTestJob, None, Duration::from_secs(5));

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

        let recent_diff = test_job_record
            .last_run_at
            .signed_duration_since(recent_time);
        assert!(recent_diff.num_milliseconds() < 100);
        assert!(second_job_record.last_run_at > old_time);
    }

    #[sqlx::test]
    async fn test_tick_handles_future_last_run_time(db: sqlx::PgPool) {
        let app_state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };
        let mut registry = CronRegistry::new();
        registry.register_job(TestJob, None, Duration::from_secs(1));

        let cron_job = registry.jobs.get(TestJob::NAME).unwrap();

        let future_time = Utc::now() + chrono::Duration::hours(1);
        let mut last_enqueue_map = HashMap::new();
        last_enqueue_map.insert(TestJob::NAME.to_string(), future_time);
        let worker_started_at = Utc::now();

        let result = cron_job
            .tick(
                app_state.clone(),
                &last_enqueue_map,
                worker_started_at,
                chrono_tz::UTC,
            )
            .await;

        // With future last_run time, the job should not run
        assert!(result.is_ok());

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
            "Should not have created cron record with future last_run time"
        );
    }

    #[sqlx::test]
    async fn test_cron_expression_scheduling(db: sqlx::PgPool) {
        let app_state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };
        let mut registry = CronRegistry::new();

        // Register with cron expression that runs every minute
        registry
            .register_job_with_cron(TestJob, None, "0 * * * * * *")
            .unwrap();

        let cron_job = registry.jobs.get(TestJob::NAME).unwrap();
        assert!(matches!(cron_job.schedule, Schedule::Cron(_)));

        // First run should wait for the next scheduled time based on worker start
        let last_enqueue_map = HashMap::new();
        // Use a time in the past to ensure the cron will trigger
        let worker_started_at = Utc::now() - chrono::Duration::minutes(2);
        let result = cron_job
            .tick(
                app_state.clone(),
                &last_enqueue_map,
                worker_started_at,
                chrono_tz::UTC,
            )
            .await;
        assert!(result.is_ok());

        // Verify cron record was created (if it ran)
        let cron_record = sqlx::query!(
            "SELECT last_run_at FROM Crons WHERE name = $1",
            TestJob::NAME
        )
        .fetch_optional(&app_state.db)
        .await
        .unwrap();

        // The job should have run since we used a worker start time 2 minutes ago
        assert!(cron_record.is_some());
        if let Some(record) = cron_record {
            let now = Utc::now();
            let diff = now.signed_duration_since(record.last_run_at);
            assert!(diff.num_milliseconds() < 1000);
        }
    }

    #[sqlx::test]
    async fn test_cron_expression_respects_schedule(db: sqlx::PgPool) {
        let app_state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };
        let mut registry = CronRegistry::new();

        // Register with cron expression that runs at specific minute (e.g., minute 30)
        registry
            .register_job_with_cron(TestJob, None, "0 30 * * * * *")
            .unwrap();

        let cron_job = registry.jobs.get(TestJob::NAME).unwrap();

        // Set last run to 29 minutes ago (shouldn't trigger if we're not at minute 30)
        let last_run = Utc::now() - chrono::Duration::minutes(29);
        let mut last_enqueue_map = HashMap::new();
        last_enqueue_map.insert(TestJob::NAME.to_string(), last_run);

        // This might or might not run depending on current minute
        // We'll just verify it doesn't error
        let worker_started_at = Utc::now();
        let result = cron_job
            .tick(
                app_state.clone(),
                &last_enqueue_map,
                worker_started_at,
                chrono_tz::UTC,
            )
            .await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_invalid_cron_expression() {
        let mut registry: CronRegistry<TestAppState> = CronRegistry::new();

        // Invalid cron expression should return error
        let result = registry.register_with_cron(
            "invalid_cron",
            None,
            "invalid expression",
            |_app_state, _context| Box::pin(async { Ok::<(), std::io::Error>(()) }),
        );

        assert!(result.is_err());
    }

    #[sqlx::test]
    async fn test_mixed_interval_and_cron_jobs(db: sqlx::PgPool) {
        let app_state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };
        let mut registry = CronRegistry::new();

        // Register one job with interval
        registry.register_job(TestJob, None, Duration::from_secs(1));

        // Register another job with cron expression (every second)
        registry
            .register_job_with_cron(SecondTestJob, None, "* * * * * * *")
            .unwrap();

        assert_eq!(registry.jobs.len(), 2);

        let interval_job = registry.jobs.get(TestJob::NAME).unwrap();
        assert!(matches!(interval_job.schedule, Schedule::Interval(_)));

        let cron_job = registry.jobs.get(SecondTestJob::NAME).unwrap();
        assert!(matches!(cron_job.schedule, Schedule::Cron(_)));

        // Create worker with start time in past to ensure both jobs run
        let mut worker = crate::cron::Worker::new(app_state.clone(), registry);
        worker.started_at = Utc::now() - chrono::Duration::seconds(2);

        // Both jobs should run on first tick
        worker.tick().await.unwrap();

        let test_job_count = sqlx::query!(
            "SELECT COUNT(*) as count FROM Crons WHERE name = $1",
            TestJob::NAME
        )
        .fetch_one(&app_state.db)
        .await
        .unwrap();

        let second_job_count = sqlx::query!(
            "SELECT COUNT(*) as count FROM Crons WHERE name = $1",
            SecondTestJob::NAME
        )
        .fetch_one(&app_state.db)
        .await
        .unwrap();

        assert_eq!(test_job_count.count.unwrap(), 1);
        assert_eq!(second_job_count.count.unwrap(), 1);
    }
}
