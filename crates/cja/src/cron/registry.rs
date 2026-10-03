use std::{collections::HashMap, error::Error, future::Future, pin::Pin, time::Duration};

use chrono::Utc;
use chrono_tz::Tz;

use crate::app_state::AppState as AS;
#[cfg(feature = "jobs")]
use crate::jobs::Job;
#[cfg(feature = "jobs")]
use crate::jobs::{
    EnqueueError, enqueue_failure, enqueue_on_connection, enqueue_receipt, enqueue_span,
};
#[cfg(feature = "jobs")]
use tracing::Instrument;

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
    action: CronAction<AppState>,
    pub schedule: Schedule,
    #[cfg(all(test, feature = "jobs"))]
    pause_after_claim: Option<std::sync::Arc<ClaimPause>>,
}

#[cfg(all(test, feature = "jobs"))]
struct ClaimPause {
    claimed: tokio::sync::mpsc::Sender<i32>,
    release: tokio::sync::Notify,
}

enum CronAction<AppState: AS> {
    Callback(Box<dyn CronFn<AppState> + Send + Sync + 'static>),
    #[cfg(feature = "jobs")]
    Atomic(Box<dyn AtomicCronJob<AppState> + Send + Sync + 'static>),
}

#[cfg(feature = "jobs")]
#[async_trait::async_trait]
trait AtomicCronJob<AppState: AS>: Send + Sync {
    async fn insert(
        &self,
        conn: &mut sqlx::PgConnection,
        context: &str,
        at: chrono::DateTime<Utc>,
        id: uuid::Uuid,
    ) -> Result<uuid::Uuid, EnqueueError>;
    fn span(&self, context: &str, at: chrono::DateTime<Utc>, id: uuid::Uuid) -> tracing::Span;
    fn name(&self) -> &'static str;
    async fn enqueue_direct(&self, app_state: AppState, context: String) -> Result<(), String>;
}

#[cfg(feature = "jobs")]
#[async_trait::async_trait]
impl<AppState: AS, J: Job<AppState>> AtomicCronJob<AppState> for J {
    async fn insert(
        &self,
        conn: &mut sqlx::PgConnection,
        context: &str,
        at: chrono::DateTime<Utc>,
        id: uuid::Uuid,
    ) -> Result<uuid::Uuid, EnqueueError> {
        enqueue_on_connection::<AppState, J>(conn, self.clone(), context, None, at, id).await
    }
    fn span(&self, context: &str, at: chrono::DateTime<Utc>, id: uuid::Uuid) -> tracing::Span {
        enqueue_span::<AppState, J>(self, context, None, at, id)
    }
    fn name(&self) -> &'static str {
        J::NAME
    }
    async fn enqueue_direct(&self, app_state: AppState, context: String) -> Result<(), String> {
        self.clone()
            .enqueue(app_state, context, None)
            .await
            .map_err(|err| err.to_string())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("TickError: {0}")]
pub enum TickError {
    JobError(String),
    SqlxError(sqlx::Error),
    #[cfg(feature = "jobs")]
    Enqueue(EnqueueError),
}

#[cfg(feature = "jobs")]
fn recoverable_sqlstate(code: &str) -> bool {
    matches!(code, "55P03" | "57014" | "25P03" | "57P01") || code.starts_with("08")
}

#[cfg(feature = "jobs")]
fn recoverable(error: &sqlx::Error) -> bool {
    match error {
        sqlx::Error::Database(db) => db.code().is_some_and(|code| recoverable_sqlstate(&code)),
        sqlx::Error::Io(_) | sqlx::Error::PoolClosed => true,
        _ => false,
    }
}

#[cfg(feature = "jobs")]
fn skip_recoverable(name: &str, operation: &str, error: &sqlx::Error) -> bool {
    if recoverable(error) {
        tracing::warn!(cron.name = name, operation, sqlstate = ?error.as_database_error().and_then(sqlx::error::DatabaseError::code), cause = %error, "Atomic cron claim skipped");
        true
    } else {
        false
    }
}

impl<AppState: AS> CronJob<AppState> {
    #[tracing::instrument(
        name = "cron_job.tick",
        level = "trace",
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
            #[cfg(feature = "jobs")]
            if let CronAction::Atomic(job) = &self.action {
                return self
                    .tick_atomic(
                        &app_state,
                        job.as_ref(),
                        &context,
                        worker_started_at,
                        timezone,
                    )
                    .await;
            }
            tracing::info!(
                task_name = self.name,
                last_run = ?last_enqueue,
                "Enqueuing Task"
            );
            if let CronAction::Callback(func) = &self.action {
                func.run(app_state.clone(), context)
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
        }

        Ok(())
    }

    pub async fn run(&self, app_state: AppState, context: String) -> Result<(), String> {
        match &self.action {
            CronAction::Callback(func) => func.run(app_state, context).await,
            #[cfg(feature = "jobs")]
            CronAction::Atomic(job) => job.enqueue_direct(app_state, context).await,
        }
    }

    pub fn is_atomic(&self) -> bool {
        #[cfg(feature = "jobs")]
        {
            matches!(self.action, CronAction::Atomic(_))
        }
        #[cfg(not(feature = "jobs"))]
        {
            false
        }
    }

    #[cfg(feature = "jobs")]
    async fn claim_due(
        &self,
        app_state: &AppState,
        worker_started_at: chrono::DateTime<Utc>,
        timezone: Tz,
    ) -> Result<Option<(sqlx::Transaction<'_, sqlx::Postgres>, chrono::DateTime<Utc>)>, TickError>
    {
        let mut tx = match app_state.db().begin().await {
            Ok(tx) => tx,
            Err(err) if skip_recoverable(self.name, "begin", &err) => return Ok(None),
            Err(err) => return Err(TickError::SqlxError(err)),
        };
        for (setting, value) in [
            ("lock_timeout", "1s"),
            ("statement_timeout", "5s"),
            ("idle_in_transaction_session_timeout", "15s"),
        ] {
            let sql = format!("SET LOCAL {setting} = '{value}'");
            if let Err(err) = sqlx::query(&sql).execute(&mut *tx).await {
                if skip_recoverable(self.name, "timeout setup", &err) {
                    return Ok(None);
                }
                return Err(TickError::SqlxError(err));
            }
        }
        let existing = sqlx::query_as::<_, (chrono::DateTime<Utc>,)>(
            "SELECT last_run_at FROM crons WHERE name = $1 FOR UPDATE NOWAIT",
        )
        .bind(self.name)
        .fetch_optional(&mut *tx)
        .await;
        let last_run = match existing {
            Ok(Some((at,))) => Some(at),
            Ok(None) => {
                let inserted = sqlx::query(
                    "INSERT INTO crons (cron_id, name, last_run_at, created_at, updated_at)
                     VALUES ($1, $2, clock_timestamp(), clock_timestamp(), clock_timestamp())
                     ON CONFLICT (name) DO NOTHING RETURNING cron_id",
                )
                .bind(uuid::Uuid::new_v4())
                .bind(self.name)
                .fetch_optional(&mut *tx)
                .await;
                match inserted {
                    Ok(Some(_)) => None,
                    Ok(None) => match sqlx::query_as::<_, (chrono::DateTime<Utc>,)>(
                        "SELECT last_run_at FROM crons WHERE name = $1 FOR UPDATE NOWAIT",
                    )
                    .bind(self.name)
                    .fetch_one(&mut *tx)
                    .await
                    {
                        Ok((at,)) => Some(at),
                        Err(err) if skip_recoverable(self.name, "reselect", &err) => {
                            return Ok(None);
                        }
                        Err(err) => return Err(TickError::SqlxError(err)),
                    },
                    Err(err) if skip_recoverable(self.name, "first-row insert", &err) => {
                        return Ok(None);
                    }
                    Err(err) => return Err(TickError::SqlxError(err)),
                }
            }
            Err(err) if skip_recoverable(self.name, "row claim", &err) => return Ok(None),
            Err(err) => return Err(TickError::SqlxError(err)),
        };
        let claim_time =
            match sqlx::query_scalar::<_, chrono::DateTime<Utc>>("SELECT clock_timestamp()")
                .fetch_one(&mut *tx)
                .await
            {
                Ok(at) => at,
                Err(err) if skip_recoverable(self.name, "database time", &err) => return Ok(None),
                Err(err) => return Err(TickError::SqlxError(err)),
            };
        if !self
            .schedule
            .should_run(last_run.as_ref(), claim_time, worker_started_at, timezone)
        {
            return Ok(None);
        }
        Ok(Some((tx, claim_time)))
    }

    #[cfg(feature = "jobs")]
    async fn tick_atomic(
        &self,
        app_state: &AppState,
        job: &dyn AtomicCronJob<AppState>,
        context: &str,
        worker_started_at: chrono::DateTime<Utc>,
        timezone: Tz,
    ) -> Result<(), TickError> {
        let Some((tx, claim_time)) = self
            .claim_due(app_state, worker_started_at, timezone)
            .await?
        else {
            return Ok(());
        };
        #[cfg(test)]
        let mut tx = tx;
        #[cfg(test)]
        if let Some(pause) = &self.pause_after_claim {
            let pid = sqlx::query_scalar::<_, i32>("SELECT pg_backend_pid()")
                .fetch_one(&mut *tx)
                .await
                .map_err(TickError::SqlxError)?;
            pause.claimed.send(pid).await.expect("claim test receiver");
            pause.release.notified().await;
        }
        self.fire(tx, claim_time, job, context).await
    }

    #[cfg(feature = "jobs")]
    async fn fire(
        &self,
        mut tx: sqlx::Transaction<'_, sqlx::Postgres>,
        claim_time: chrono::DateTime<Utc>,
        job: &dyn AtomicCronJob<AppState>,
        context: &str,
    ) -> Result<(), TickError> {
        let id = uuid::Uuid::new_v4();
        let span = job.span(context, claim_time, id);
        async {
            if let Err(err) = job.insert(&mut tx, context, claim_time, id).await {
                enqueue_failure(id, &err);
                if let EnqueueError::SqlxError(sqlx_err) = &err
                    && skip_recoverable(self.name, "queue insert", sqlx_err)
                {
                    return Ok(());
                }
                return Err(TickError::Enqueue(err));
            }
            if let Err(err) =
                sqlx::query("UPDATE crons SET last_run_at = $1, updated_at = $1 WHERE name = $2")
                    .bind(claim_time)
                    .bind(self.name)
                    .execute(&mut *tx)
                    .await
            {
                if skip_recoverable(self.name, "cron update", &err) {
                    return Ok(());
                }
                return Err(TickError::SqlxError(err));
            }
            if let Err(err) = tx.commit().await {
                if skip_recoverable(self.name, "commit", &err) {
                    return Ok(());
                }
                return Err(TickError::SqlxError(err));
            }
            enqueue_receipt(id, job.name());
            Ok(())
        }
        .instrument(span)
        .await
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
            action: CronAction::Callback(Box::new(CronFnClosure {
                func: job,
                _marker: std::marker::PhantomData,
            })),
            schedule: Schedule::Interval(IntervalSchedule(interval)),
            #[cfg(all(test, feature = "jobs"))]
            pause_after_claim: None,
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
            action: CronAction::Callback(Box::new(CronFnClosure {
                func: job,
                _marker: std::marker::PhantomData,
            })),
            schedule: Schedule::Cron(CronSchedule(Box::new(cron_schedule))),
            #[cfg(all(test, feature = "jobs"))]
            pause_after_claim: None,
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
    #[tracing::instrument(name = "cron.register_job_atomic", skip_all, fields(cron_job.name = J::NAME, cron_job.interval = ?interval))]
    pub fn register_job_atomic<J: Job<AppState>>(
        &mut self,
        job: J,
        description: Option<&'static str>,
        interval: Duration,
    ) {
        self.jobs.insert(
            J::NAME,
            CronJob {
                name: J::NAME,
                description,
                action: CronAction::Atomic(Box::new(job)),
                schedule: Schedule::Interval(IntervalSchedule(interval)),
                #[cfg(all(test, feature = "jobs"))]
                pause_after_claim: None,
            },
        );
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

    /// Returns each registered cron job's name and schedule string, sorted by name.
    ///
    /// Interval schedules use the `Duration` `Debug` form (e.g. `"300s"`),
    /// matching what the `cron.register` span records. Cron-expression
    /// schedules use the original expression (e.g. `"0 0 * * * * *"`).
    #[must_use]
    pub fn entries(&self) -> Vec<(&'static str, String)> {
        let mut entries: Vec<(&'static str, String)> = self
            .jobs
            .values()
            .map(|job| {
                let schedule = match &job.schedule {
                    Schedule::Interval(IntervalSchedule(duration)) => format!("{duration:?}"),
                    Schedule::Cron(CronSchedule(schedule)) => schedule.to_string(),
                };
                (job.name, schedule)
            })
            .collect();
        entries.sort_unstable_by_key(|(name, _)| *name);
        entries
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
    use std::fmt::Write;
    use std::sync::{Arc, Mutex, OnceLock};
    use tracing::{
        Event, Id, Subscriber,
        field::{Field, Visit},
        span::Attributes,
    };
    use tracing_subscriber::{Layer, layer::Context, prelude::*, registry::LookupSpan};

    use super::*;

    #[derive(Default)]
    struct Fields(String);

    impl Visit for Fields {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            let _ = write!(self.0, " {}={value:?}", field.name());
        }
    }

    struct TraceCapture(Arc<Mutex<Vec<String>>>);

    fn trace_capture() -> &'static Arc<Mutex<Vec<String>>> {
        static CAPTURE: OnceLock<Arc<Mutex<Vec<String>>>> = OnceLock::new();
        CAPTURE.get_or_init(|| {
            let lines = Arc::new(Mutex::new(Vec::new()));
            tracing::subscriber::set_global_default(
                tracing_subscriber::registry()
                    .with(TraceCapture(lines.clone()))
                    .with(tracing_subscriber::filter::LevelFilter::TRACE),
            )
            .expect("install one test tracing subscriber");
            lines
        })
    }

    impl<S> Layer<S> for TraceCapture
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
            let mut fields = Fields::default();
            attrs.record(&mut fields);
            let parent = ctx
                .span(id)
                .and_then(|span| span.parent().map(|parent| parent.name().to_owned()));
            self.0.lock().unwrap().push(format!(
                "span:{} parent:{parent:?}{}",
                attrs.metadata().name(),
                fields.0
            ));
        }

        fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
            let mut fields = Fields::default();
            event.record(&mut fields);
            let parent = ctx.event_span(event).map(|span| span.name().to_owned());
            self.0
                .lock()
                .unwrap()
                .push(format!("event parent:{parent:?}{}", fields.0));
        }
    }

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

    #[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
    struct TelemetryJob;

    #[async_trait::async_trait]
    impl Job<TestAppState> for TelemetryJob {
        const NAME: &'static str = "atomic_telemetry_job";

        async fn run(&self, _app_state: TestAppState) -> color_eyre::Result<()> {
            Ok(())
        }
    }

    fn atomic_worker(db: sqlx::PgPool) -> crate::cron::Worker<TestAppState> {
        let state = TestAppState {
            db,
            cookie_key: CookieKey::generate(),
        };
        let mut registry = CronRegistry::new();
        registry.register_job_atomic(TestJob, None, Duration::from_millis(100));
        crate::cron::Worker::new(state, registry)
    }

    #[sqlx::test]
    async fn atomic_two_pools_commit_one_enqueue_per_interval(db: sqlx::PgPool) {
        let other = sqlx::PgPool::connect_with((*db.connect_options()).clone())
            .await
            .unwrap();
        let first = atomic_worker(db.clone());
        let second = atomic_worker(other);
        let count = || async {
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs WHERE name = $1")
                .bind(TestJob::NAME)
                .fetch_one(&db)
                .await
                .unwrap()
        };
        let pair = || async {
            let barrier = Arc::new(tokio::sync::Barrier::new(3));
            let (a, b, _) = tokio::join!(
                async {
                    barrier.wait().await;
                    first.tick().await
                },
                async {
                    barrier.wait().await;
                    second.tick().await
                },
                barrier.wait(),
            );
            a.unwrap();
            b.unwrap();
        };
        pair().await;
        assert_eq!(count().await, 1);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM crons WHERE name = $1")
                .bind(TestJob::NAME)
                .fetch_one(&db)
                .await
                .unwrap(),
            1
        );
        pair().await;
        assert_eq!(count().await, 1);
        for expected in 2..=4 {
            sqlx::query("UPDATE crons SET last_run_at = clock_timestamp() - interval '2 seconds' WHERE name = $1")
                .bind(TestJob::NAME).execute(&db).await.unwrap();
            pair().await;
            assert_eq!(count().await, expected);
            pair().await;
            assert_eq!(count().await, expected);
        }
    }

    #[sqlx::test]
    async fn atomic_dropped_claim_rolls_back_and_next_tick_fires(db: sqlx::PgPool) {
        let state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };
        let mut registry = CronRegistry::new();
        registry.register_job_atomic(TestJob, None, Duration::from_secs(864));
        let cron = registry.get(TestJob::NAME).unwrap();
        let (claim, _) = cron
            .claim_due(&state, Utc::now(), chrono_tz::UTC)
            .await
            .unwrap()
            .unwrap();
        drop(claim);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM crons")
                .fetch_one(&db)
                .await
                .unwrap(),
            0
        );
        let worker = crate::cron::Worker::new(state.clone(), registry);
        worker.tick().await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs")
                .fetch_one(&db)
                .await
                .unwrap(),
            1
        );
        sqlx::query("UPDATE crons SET last_run_at = clock_timestamp() - interval '866 seconds' WHERE name = $1")
            .bind(TestJob::NAME).execute(&db).await.unwrap();
        let mut registry = CronRegistry::new();
        registry.register_job_atomic(TestJob, None, Duration::from_secs(864));
        let cron = registry.get(TestJob::NAME).unwrap();
        let (claim, _) = cron
            .claim_due(&state, Utc::now(), chrono_tz::UTC)
            .await
            .unwrap()
            .unwrap();
        drop(claim);
        crate::cron::Worker::new(state, registry)
            .tick()
            .await
            .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs")
                .fetch_one(&db)
                .await
                .unwrap(),
            2
        );
    }

    #[sqlx::test]
    async fn atomic_864_second_cadence_and_database_stamp(db: sqlx::PgPool) {
        let state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };
        let mut registry = CronRegistry::new();
        registry.register_job_atomic(TestJob, None, Duration::from_secs(864));
        let worker = crate::cron::Worker::new(state, registry);
        worker.tick().await.unwrap();
        sqlx::query("UPDATE crons SET last_run_at = clock_timestamp() - interval '862 seconds' WHERE name = $1")
            .bind(TestJob::NAME).execute(&db).await.unwrap();
        worker.tick().await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs")
                .fetch_one(&db)
                .await
                .unwrap(),
            1
        );
        sqlx::query("UPDATE crons SET last_run_at = clock_timestamp() - interval '866 seconds' WHERE name = $1")
            .bind(TestJob::NAME).execute(&db).await.unwrap();
        let before = sqlx::query_scalar::<_, chrono::DateTime<Utc>>("SELECT clock_timestamp()")
            .fetch_one(&db)
            .await
            .unwrap();
        worker.tick().await.unwrap();
        let after = sqlx::query_scalar::<_, chrono::DateTime<Utc>>("SELECT clock_timestamp()")
            .fetch_one(&db)
            .await
            .unwrap();
        let stamp = sqlx::query_scalar::<_, chrono::DateTime<Utc>>(
            "SELECT last_run_at FROM crons WHERE name = $1",
        )
        .bind(TestJob::NAME)
        .fetch_one(&db)
        .await
        .unwrap();
        assert!(stamp >= before && stamp <= after);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs")
                .fetch_one(&db)
                .await
                .unwrap(),
            2
        );
    }

    #[sqlx::test]
    async fn atomic_locked_cron_does_not_block_other_crons(db: sqlx::PgPool) {
        let state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };
        let mut registry = CronRegistry::new();
        registry.register_job_atomic(TestJob, None, Duration::from_secs(30));
        registry.register_job_atomic(SecondTestJob, None, Duration::from_secs(30));
        let worker = crate::cron::Worker::new(state, registry);
        sqlx::query("INSERT INTO crons (cron_id, name, last_run_at, created_at, updated_at) VALUES ($1, $2, clock_timestamp() - interval '60 seconds', clock_timestamp(), clock_timestamp())")
            .bind(uuid::Uuid::new_v4()).bind(TestJob::NAME).execute(&db).await.unwrap();
        let mut blocker = db.begin().await.unwrap();
        sqlx::query("SELECT last_run_at FROM crons WHERE name = $1 FOR UPDATE")
            .bind(TestJob::NAME)
            .fetch_one(&mut *blocker)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), worker.tick())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs WHERE name = $1")
                .bind(SecondTestJob::NAME)
                .fetch_one(&db)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs WHERE name = $1")
                .bind(TestJob::NAME)
                .fetch_one(&db)
                .await
                .unwrap(),
            0
        );
        blocker.rollback().await.unwrap();
        worker.tick().await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs WHERE name = $1")
                .bind(TestJob::NAME)
                .fetch_one(&db)
                .await
                .unwrap(),
            1
        );
    }

    #[sqlx::test]
    async fn atomic_uncommitted_first_row_is_bounded_and_retried(db: sqlx::PgPool) {
        let other = sqlx::PgPool::connect_with((*db.connect_options()).clone())
            .await
            .unwrap();
        let worker = atomic_worker(other);
        let mut blocker = db.begin().await.unwrap();
        sqlx::query("INSERT INTO crons (cron_id, name, last_run_at, created_at, updated_at) VALUES ($1, $2, clock_timestamp(), clock_timestamp(), clock_timestamp())")
            .bind(uuid::Uuid::new_v4()).bind(TestJob::NAME).execute(&mut *blocker).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), worker.tick())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs")
                .fetch_one(&db)
                .await
                .unwrap(),
            0
        );
        blocker.rollback().await.unwrap();
        worker.tick().await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs")
                .fetch_one(&db)
                .await
                .unwrap(),
            1
        );
    }

    #[sqlx::test]
    async fn atomic_terminated_claimer_rolls_back_and_retries(db: sqlx::PgPool) {
        let other = sqlx::PgPool::connect_with((*db.connect_options()).clone())
            .await
            .unwrap();
        let state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };
        let mut registry = CronRegistry::new();
        registry.register_job_atomic(TestJob, None, Duration::from_mins(1));
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let pause = std::sync::Arc::new(ClaimPause {
            claimed: sender,
            release: tokio::sync::Notify::new(),
        });
        registry
            .jobs
            .get_mut(TestJob::NAME)
            .unwrap()
            .pause_after_claim = Some(pause.clone());
        let worker = crate::cron::Worker::new(state.clone(), registry);
        let task = tokio::spawn(async move { worker.tick().await });
        let pid = tokio::time::timeout(Duration::from_secs(3), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            sqlx::query_scalar::<_, bool>("SELECT pg_terminate_backend($1)")
                .bind(pid)
                .fetch_one(&other)
                .await
                .unwrap()
        );
        pause.release.notify_one();
        tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs")
                .fetch_one(&db)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM crons")
                .fetch_one(&db)
                .await
                .unwrap(),
            0
        );
        let mut retry_registry = CronRegistry::new();
        retry_registry.register_job_atomic(TestJob, None, Duration::from_mins(1));
        crate::cron::Worker::new(state, retry_registry)
            .tick()
            .await
            .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs")
                .fetch_one(&db)
                .await
                .unwrap(),
            1
        );
    }

    #[sqlx::test]
    async fn atomic_statement_timeout_is_recoverable(db: sqlx::PgPool) {
        let mut tx = db.begin().await.unwrap();
        sqlx::query("SET LOCAL statement_timeout = '10ms'")
            .execute(&mut *tx)
            .await
            .unwrap();
        let error = sqlx::query("SELECT pg_sleep(0.1)")
            .execute(&mut *tx)
            .await
            .unwrap_err();
        assert_eq!(
            error.as_database_error().unwrap().code().as_deref(),
            Some("57014")
        );
        assert!(recoverable(&error));
    }

    #[test]
    fn atomic_termination_sqlstates_are_recoverable() {
        for code in ["55P03", "57014", "25P03", "57P01", "08006", "08003"] {
            assert!(recoverable_sqlstate(code), "{code}");
        }
        for code in ["23505", "42P01", "25P02"] {
            assert!(!recoverable_sqlstate(code), "{code}");
        }
    }

    #[sqlx::test]
    async fn atomic_enqueue_receipt_follows_commit_and_has_parent_span(db: sqlx::PgPool) {
        let lines = trace_capture();
        let state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };
        let mut registry = CronRegistry::new();
        registry.register_job_atomic(TelemetryJob, None, Duration::from_mins(1));
        let worker = crate::cron::Worker::new(state.clone(), registry);
        worker.tick().await.unwrap();
        let captured = lines.lock().unwrap().clone();
        let span = captured
            .iter()
            .find(|line| line.starts_with("span:jobs.enqueue") && line.contains(TelemetryJob::NAME))
            .expect("enqueue span");
        assert!(span.contains("parent:Some(\"cron_job.tick\")"), "{span}");
        for field in [
            "job.id",
            "job.name",
            "job.context",
            "job.priority",
            "job.created_at",
            "job.run_at",
        ] {
            assert!(span.contains(field), "missing {field}: {span}");
        }
        let receipt = captured
            .iter()
            .find(|line| line.contains("job_enqueued") && line.contains(TelemetryJob::NAME))
            .expect("enqueue receipt");
        assert!(receipt.contains("jobs.enqueue"), "{receipt}");
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs")
                .fetch_one(&db)
                .await
                .unwrap(),
            1
        );

        sqlx::query("DELETE FROM jobs").execute(&db).await.unwrap();
        sqlx::query("DELETE FROM crons").execute(&db).await.unwrap();
        sqlx::query("ALTER TABLE jobs ADD CONSTRAINT test_reject_job CHECK (name <> 'atomic_telemetry_job')")
            .execute(&db)
            .await
            .unwrap();
        lines.lock().unwrap().clear();
        assert!(worker.tick().await.is_err());
        assert!(
            !lines
                .lock()
                .unwrap()
                .iter()
                .any(|line| line.contains("job_enqueued"))
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM crons")
                .fetch_one(&db)
                .await
                .unwrap(),
            0
        );
        sqlx::query("ALTER TABLE jobs DROP CONSTRAINT test_reject_job")
            .execute(&db)
            .await
            .unwrap();
        lines.lock().unwrap().clear();
        TelemetryJob
            .enqueue(state, "ordinary".into(), None)
            .await
            .unwrap();
        let ordinary = lines.lock().unwrap();
        assert!(
            ordinary
                .iter()
                .any(|line| line.starts_with("span:jobs.enqueue") && line.contains("job.context"))
        );
        assert!(ordinary.iter().any(|line| line.contains("job_enqueued")));
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
        registry.register_job(TestJob, None, Duration::from_mins(1));
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
            TickError::SqlxError(_) | TickError::Enqueue(_) => panic!("Expected JobError"),
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

    #[test]
    fn test_entries_lists_names_and_schedule_strings() {
        let mut registry: CronRegistry<TestAppState> = CronRegistry::new();

        registry.register(
            "interval_job",
            None,
            Duration::from_mins(5),
            |_app_state, _context| Box::pin(async { Ok::<(), std::io::Error>(()) }),
        );
        registry
            .register_with_cron(
                "cron_expr_job",
                None,
                "0 0 9 * * * *",
                |_app_state, _context| Box::pin(async { Ok::<(), std::io::Error>(()) }),
            )
            .unwrap();

        let entries = registry.entries();
        assert_eq!(
            entries,
            vec![
                ("cron_expr_job", "0 0 9 * * * *".to_string()),
                ("interval_job", "300s".to_string()),
            ]
        );
    }

    /// Regression test for the cron starvation bug fixed in 4f29b39 ("Fix Cron (#5)"):
    /// `last_run_at` used to be bumped on every worker tick (even when the job did
    /// not fire), so any interval longer than the tick cadence never fired again
    /// after its first run. This verifies a >60s interval survives multiple
    /// non-firing ticks with `last_run_at` untouched, then fires once due.
    #[sqlx::test]
    async fn test_long_interval_last_run_only_advances_on_fire(db: sqlx::PgPool) {
        let app_state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };
        let mut registry = CronRegistry::new();
        // 5-minute interval: much longer than the worker's ~60s tick cadence
        registry.register_job(TestJob, None, Duration::from_mins(5));
        let worker = crate::cron::Worker::new(app_state.clone(), registry);

        // Simulate a fire 2 minutes ago — the interval has NOT yet elapsed
        let two_minutes_ago = Utc::now() - chrono::Duration::minutes(2);
        sqlx::query!(
            "INSERT INTO Crons (cron_id, name, last_run_at, created_at, updated_at)
            VALUES ($1, $2, $3, $3, $3)",
            uuid::Uuid::new_v4(),
            TestJob::NAME,
            two_minutes_ago
        )
        .execute(&app_state.db)
        .await
        .unwrap();

        // Multiple simulated worker ticks while the interval has not elapsed
        for _ in 0..3 {
            worker.tick().await.unwrap();

            let last_run = sqlx::query!(
                "SELECT last_run_at FROM Crons WHERE name = $1",
                TestJob::NAME
            )
            .fetch_one(&app_state.db)
            .await
            .unwrap();

            let drift = last_run
                .last_run_at
                .signed_duration_since(two_minutes_ago)
                .num_milliseconds()
                .abs();
            assert!(
                drift < 50,
                "last_run_at must not advance on non-firing ticks (drifted {drift}ms)"
            );
        }

        // No job should have been enqueued by the non-firing ticks
        let job_count = sqlx::query!(
            "SELECT COUNT(*) as count FROM jobs WHERE name = $1",
            TestJob::NAME
        )
        .fetch_one(&app_state.db)
        .await
        .unwrap();
        assert_eq!(job_count.count.unwrap(), 0);

        // Now simulate the interval having elapsed: last fire 6 minutes ago
        let six_minutes_ago = Utc::now() - chrono::Duration::minutes(6);
        sqlx::query!(
            "UPDATE Crons SET last_run_at = $1 WHERE name = $2",
            six_minutes_ago,
            TestJob::NAME
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
        assert!(
            last_run.last_run_at > six_minutes_ago,
            "cron should fire once the interval has elapsed"
        );
        let diff = Utc::now().signed_duration_since(last_run.last_run_at);
        assert!(diff.num_milliseconds() < 1000);

        let job_count = sqlx::query!(
            "SELECT COUNT(*) as count FROM jobs WHERE name = $1",
            TestJob::NAME
        )
        .fetch_one(&app_state.db)
        .await
        .unwrap();
        assert_eq!(job_count.count.unwrap(), 1);
    }
}
