use std::{collections::HashMap, error::Error, future::Future, pin::Pin, time::Duration};

use chrono::{TimeZone, Timelike, Utc};
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

/// A cron expression, evaluated against local wall-clock time.
///
/// A job fires on the first tick at which the wall clock has passed a
/// scheduled time that it had not passed at the previous fire. Across a
/// daylight-saving change this means:
///
/// - A time the clock skips (02:30 when US clocks spring forward) fires on the
///   first tick after the jump.
/// - A time the clock repeats (01:30 when US clocks fall back) fires once, in
///   whichever copy of the hour the wall clock first reaches it.
/// - Sub-hourly expressions do not fire again while the clock repeats an hour,
///   so `0 */15 * * * * *` goes 75 minutes between fires that night. Register
///   an interval for elapsed-time cadence.
#[derive(Clone, Debug)]
pub struct CronSchedule(pub Box<cron::Schedule>);

/// The wall-clock time of `at` in `timezone`, carried in a `DateTime<Utc>`.
///
/// UTC has no daylight saving, so the cron expression can be evaluated here
/// without `cron`'s handling of repeated times: from inside the second copy
/// of a repeated hour, `Schedule::after` returns the first copy, which is
/// earlier than its argument.
fn wall_clock(at: &chrono::DateTime<Utc>, timezone: Tz) -> chrono::DateTime<Utc> {
    at.with_timezone(&timezone).naive_local().and_utc()
}

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
        self.0
            .after(&wall_clock(last_run, timezone))
            .next()
            .is_some_and(|next_run| wall_clock(&now, timezone) >= next_run)
    }

    pub fn next_run(
        &self,
        last_run: Option<&chrono::DateTime<Utc>>,
        now: chrono::DateTime<Utc>,
        timezone: Tz,
    ) -> chrono::DateTime<Tz> {
        let last_run = last_run.unwrap_or(&now);
        let next_run = self
            .0
            .after(&wall_clock(last_run, timezone))
            .next()
            .unwrap()
            .naive_utc();
        match timezone.from_local_datetime(&next_run) {
            chrono::LocalResult::Single(at) => at,
            chrono::LocalResult::Ambiguous(earlier, later) => {
                if earlier > *last_run {
                    earlier
                } else {
                    later
                }
            }
            // Skipped by the clock, so reached when the clock jumps past it:
            // the first whole minute that exists after it.
            chrono::LocalResult::None => {
                let minute = next_run
                    .with_second(0)
                    .and_then(|at| at.with_nanosecond(0))
                    .unwrap();
                (1..=24 * 60)
                    .find_map(|offset| {
                        timezone
                            .from_local_datetime(&(minute + chrono::Duration::minutes(offset)))
                            .earliest()
                    })
                    .unwrap()
            }
        }
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
    #[cfg(all(test, feature = "jobs"))]
    pause_after_commit: Option<std::sync::Arc<CallbackPause>>,
}

#[cfg(all(test, feature = "jobs"))]
struct ClaimPause {
    claimed: tokio::sync::mpsc::Sender<i32>,
    release: tokio::sync::Notify,
}

#[cfg(all(test, feature = "jobs"))]
struct CallbackPause {
    committed: tokio::sync::mpsc::Sender<chrono::DateTime<Utc>>,
    release: tokio::sync::Notify,
}

enum CronAction<AppState: AS> {
    Callback(Box<dyn CronFn<AppState> + Send + Sync + 'static>),
    #[cfg(feature = "jobs")]
    Job(Box<dyn CronEnqueueJob<AppState> + Send + Sync + 'static>),
}

#[cfg(feature = "jobs")]
#[async_trait::async_trait]
trait CronEnqueueJob<AppState: AS>: Send + Sync {
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
impl<AppState: AS, J: Job<AppState>> CronEnqueueJob<AppState> for J {
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
}

#[cfg(feature = "jobs")]
fn recoverable_sqlstate(code: &str) -> bool {
    matches!(code, "55P03" | "57014" | "25P03" | "57P01") || code.starts_with("08")
}

#[cfg(feature = "jobs")]
pub(super) fn recoverable(error: &sqlx::Error) -> bool {
    match error {
        sqlx::Error::Database(db) => db.code().is_some_and(|code| recoverable_sqlstate(&code)),
        sqlx::Error::Io(_) | sqlx::Error::PoolClosed | sqlx::Error::PoolTimedOut => true,
        _ => false,
    }
}

/// A due interval whose `crons` row this process holds locked. Committing the
/// transaction is what makes the interval this process's fire.
#[cfg(feature = "jobs")]
struct Claim<'c> {
    tx: sqlx::Transaction<'c, sqlx::Postgres>,
    /// Database time of the claim; becomes the row's new `last_run_at`.
    at: chrono::DateTime<Utc>,
    /// The row's `last_run_at` before this claim; `None` on a cron's first fire.
    last_run: Option<chrono::DateTime<Utc>>,
}

#[cfg(feature = "jobs")]
fn skip_recoverable(name: &str, operation: &str, error: &sqlx::Error) -> bool {
    if recoverable(error) {
        tracing::warn!(cron.name = name, operation, sqlstate = ?error.as_database_error().and_then(sqlx::error::DatabaseError::code), cause = %error, "Cron claim skipped");
        true
    } else {
        false
    }
}

impl<AppState: AS> CronJob<AppState> {
    /// Whether this cron is due at `now`, treating a `last_run_at` ahead of
    /// `now` as due: a future stamp must not stall the cron. Callers that own
    /// the authoritative decision log the warning; `should_run` itself stays
    /// a pure schedule predicate.
    fn is_due(
        &self,
        last_run: Option<&chrono::DateTime<Utc>>,
        now: chrono::DateTime<Utc>,
        worker_started_at: chrono::DateTime<Utc>,
        timezone: Tz,
    ) -> bool {
        last_run.is_some_and(|at| *at > now)
            || self
                .schedule
                .should_run(last_run, now, worker_started_at, timezone)
    }

    /// Runs this cron's claim-and-fire path if it is due at `now`.
    ///
    /// `now` is the tick's database time advanced by the monotonic time
    /// elapsed since that database read; this method performs no clock
    /// reads of its own. The advancement matters because a cron earlier in
    /// the tick can run a callback that outlives the tick-start snapshot:
    /// a stale `now` would read as not-due for this cron — a false negative
    /// that `claim_due` never corrects, since it is only reached when this
    /// pre-filter passes. `claim_due` remains the authoritative decision
    /// and re-checks against a fresh `clock_timestamp()`.
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
        now: chrono::DateTime<Utc>,
        worker_started_at: chrono::DateTime<Utc>,
        timezone: Tz,
    ) -> Result<(), TickError> {
        let last_enqueue = last_enqueue_map.get(self.name);
        let context = format!("Cron@{}", app_state.version());

        if self.is_due(last_enqueue, now, worker_started_at, timezone) {
            return self
                .tick_claimed(&app_state, &context, worker_started_at, timezone)
                .await;
        }

        Ok(())
    }

    pub async fn run(&self, app_state: AppState, context: String) -> Result<(), String> {
        match &self.action {
            CronAction::Callback(func) => func.run(app_state, context).await,
            #[cfg(feature = "jobs")]
            CronAction::Job(job) => job.enqueue_direct(app_state, context).await,
        }
    }

    /// The cron fire receipt: one INFO event per interval this process claimed
    /// and committed, for job and callback crons alike.
    ///
    /// **This event is an Eyes contract — do not remove, reword, or move it
    /// without coordinating Eyes.** Eyes classifies `cron.fire` as
    /// `level = INFO AND target = 'cja::cron::registry' AND message LIKE
    /// 'Enqueuing Task%'`, keyed by the `task_name` field; every `cron_missed`
    /// monitor and the crons dashboard depend on it. `last_run` is the `Debug`
    /// form of the claimed row's previous `last_run_at` (`None` on the first
    /// fire), which humans use to prove a run happened when telemetry was
    /// dropped. The target comes from `module_path!()`, so this must stay in
    /// `cja::cron::registry`. cja#53 dropped this event once and blinded every
    /// cron monitor in apps that picked it up.
    #[cfg(feature = "jobs")]
    fn fire_receipt(&self, last_run: Option<chrono::DateTime<Utc>>) {
        tracing::info!(task_name = self.name, last_run = ?last_run, "Enqueuing Task");
    }

    #[cfg(feature = "jobs")]
    async fn claim_due(
        &self,
        app_state: &AppState,
        worker_started_at: chrono::DateTime<Utc>,
        timezone: Tz,
    ) -> Result<Option<Claim<'_>>, TickError> {
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
        if last_run.is_some_and(|at| at > claim_time) {
            tracing::warn!(
                cron.name = self.name,
                last_run = ?last_run,
                claim_time = ?claim_time,
                "Cron last_run_at is ahead of the database clock; treating as due"
            );
        }
        if !self.is_due(last_run.as_ref(), claim_time, worker_started_at, timezone) {
            return Ok(None);
        }
        Ok(Some(Claim {
            tx,
            at: claim_time,
            last_run,
        }))
    }

    #[cfg(feature = "jobs")]
    async fn tick_claimed(
        &self,
        app_state: &AppState,
        context: &str,
        worker_started_at: chrono::DateTime<Utc>,
        timezone: Tz,
    ) -> Result<(), TickError> {
        let Some(Claim {
            tx,
            at: claim_time,
            last_run,
        }) = self
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
        match &self.action {
            CronAction::Job(job) => {
                self.fire(tx, claim_time, last_run, job.as_ref(), context)
                    .await
            }
            CronAction::Callback(callback) => {
                if !self.commit_claim(tx, claim_time).await? {
                    return Ok(());
                }
                self.fire_receipt(last_run);
                #[cfg(test)]
                if let Some(pause) = &self.pause_after_commit {
                    pause
                        .committed
                        .send(claim_time)
                        .await
                        .expect("callback test receiver");
                    pause.release.notified().await;
                }
                callback
                    .run(app_state.clone(), context.to_owned())
                    .await
                    .map_err(TickError::JobError)
            }
        }
    }

    #[cfg(feature = "jobs")]
    async fn commit_claim(
        &self,
        mut tx: sqlx::Transaction<'_, sqlx::Postgres>,
        claim_time: chrono::DateTime<Utc>,
    ) -> Result<bool, TickError> {
        if let Err(err) =
            sqlx::query("UPDATE crons SET last_run_at = $1, updated_at = $1 WHERE name = $2")
                .bind(claim_time)
                .bind(self.name)
                .execute(&mut *tx)
                .await
        {
            if skip_recoverable(self.name, "cron update", &err) {
                return Ok(false);
            }
            return Err(TickError::SqlxError(err));
        }
        if let Err(err) = tx.commit().await {
            if skip_recoverable(self.name, "commit", &err) {
                return Ok(false);
            }
            return Err(TickError::SqlxError(err));
        }
        Ok(true)
    }

    #[cfg(feature = "jobs")]
    async fn fire(
        &self,
        mut tx: sqlx::Transaction<'_, sqlx::Postgres>,
        claim_time: chrono::DateTime<Utc>,
        last_run: Option<chrono::DateTime<Utc>>,
        job: &dyn CronEnqueueJob<AppState>,
        context: &str,
    ) -> Result<(), TickError> {
        let id = uuid::Uuid::new_v4();
        let span = job.span(context, claim_time, id);
        async {
            if let Err(err) = job.insert(&mut tx, context, claim_time, id).await {
                enqueue_failure(id, &err);
                match err {
                    EnqueueError::SqlxError(sqlx_err)
                        if skip_recoverable(self.name, "queue insert", &sqlx_err) =>
                    {
                        return Ok(());
                    }
                    EnqueueError::SqlxError(sqlx_err) => {
                        return Err(TickError::SqlxError(sqlx_err));
                    }
                    EnqueueError::SerdeJsonError(serde_err) => {
                        return Err(TickError::JobError(serde_err.to_string()));
                    }
                }
            }
            if self.commit_claim(tx, claim_time).await? {
                // Inside the `jobs.enqueue` span so the fire shares its trace.
                self.fire_receipt(last_run);
                enqueue_receipt(id, job.name());
            }
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

    /// Insert `cron_job`, panicking if its name is already registered.
    ///
    /// Callback and job crons share one name namespace; a duplicate is a
    /// static registration bug, so it fails at startup instead of silently
    /// replacing the first cron.
    ///
    /// # Panics
    ///
    /// Panics if a cron with `cron_job.name` is already registered.
    fn insert_unique(&mut self, cron_job: CronJob<AppState>) {
        assert!(
            !self.jobs.contains_key(cron_job.name),
            "duplicate cron registration: cron name `{}` is already registered",
            cron_job.name
        );
        self.jobs.insert(cron_job.name, cron_job);
    }

    /// Registers a callback cron at a fixed interval.
    ///
    /// # Panics
    ///
    /// Panics if a cron with `name` is already registered.
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
            #[cfg(all(test, feature = "jobs"))]
            pause_after_commit: None,
        };
        self.insert_unique(cron_job);
    }

    /// Registers a callback cron on a cron expression.
    ///
    /// Returns an error when `cron_expr` does not parse; the registry is
    /// untouched in that case.
    ///
    /// # Panics
    ///
    /// Panics if a cron with `name` is already registered.
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
            #[cfg(all(test, feature = "jobs"))]
            pause_after_commit: None,
        };
        self.insert_unique(cron_job);
        Ok(())
    }

    /// Registers a job cron at a fixed interval.
    ///
    /// # Panics
    ///
    /// Panics if a cron with `J::NAME` is already registered.
    #[cfg(feature = "jobs")]
    #[tracing::instrument(name = "cron.register_job", skip_all, fields(cron_job.name = J::NAME, cron_job.interval = ?interval))]
    pub fn register_job<J: Job<AppState>>(
        &mut self,
        job: J,
        description: Option<&'static str>,
        interval: Duration,
    ) {
        self.insert_unique(CronJob {
            name: J::NAME,
            description,
            action: CronAction::Job(Box::new(job)),
            schedule: Schedule::Interval(IntervalSchedule(interval)),
            #[cfg(all(test, feature = "jobs"))]
            pause_after_claim: None,
            #[cfg(all(test, feature = "jobs"))]
            pause_after_commit: None,
        });
    }

    /// Registers a job cron on a cron expression.
    ///
    /// Returns an error when `cron_expr` does not parse; the registry is
    /// untouched in that case.
    ///
    /// # Panics
    ///
    /// Panics if a cron with `J::NAME` is already registered.
    #[cfg(feature = "jobs")]
    #[tracing::instrument(name = "cron.register_job_with_cron", skip_all, fields(cron_job.name = J::NAME, cron_job.cron = cron_expr))]
    pub fn register_job_with_cron<J: Job<AppState>>(
        &mut self,
        job: J,
        description: Option<&'static str>,
        cron_expr: &str,
    ) -> Result<(), cron::error::Error> {
        let schedule = cron_expr.parse::<cron::Schedule>()?;
        self.insert_unique(CronJob {
            name: J::NAME,
            description,
            action: CronAction::Job(Box::new(job)),
            schedule: Schedule::Cron(CronSchedule(Box::new(schedule))),
            #[cfg(all(test, feature = "jobs"))]
            pause_after_claim: None,
            #[cfg(all(test, feature = "jobs"))]
            pause_after_commit: None,
        });
        Ok(())
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
    use crate::cron::worker::TickOutcome;
    use crate::server::cookies::CookieKey;
    use std::fmt::Write;
    use std::sync::{Arc, Mutex, OnceLock};
    use tracing::{
        Event, Id, Subscriber,
        field::{Field, Visit},
        instrument::WithSubscriber,
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

    /// One structured event, for asserting the Eyes `cron.fire` contract.
    #[derive(Clone, Debug)]
    struct CapturedEvent {
        target: String,
        level: tracing::Level,
        parent: Option<String>,
        fields: std::collections::BTreeMap<String, String>,
    }

    #[derive(Default)]
    struct FieldMap(std::collections::BTreeMap<String, String>);

    impl Visit for FieldMap {
        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.insert(field.name().to_owned(), value.to_owned());
        }

        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.0.insert(field.name().to_owned(), format!("{value:?}"));
        }
    }

    /// Per-test capture, attached to the future under test with
    /// `with_subscriber`. Unlike the global [`TraceCapture`], concurrent tests
    /// can neither clear it nor write into it.
    #[derive(Clone, Default)]
    struct EventCapture(Arc<Mutex<Vec<CapturedEvent>>>);

    impl<S> Layer<S> for EventCapture
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
            let mut fields = FieldMap::default();
            event.record(&mut fields);
            self.0.lock().unwrap().push(CapturedEvent {
                target: event.metadata().target().to_owned(),
                level: *event.metadata().level(),
                parent: ctx.event_span(event).map(|span| span.name().to_owned()),
                fields: fields.0,
            });
        }
    }

    impl EventCapture {
        fn dispatch(&self) -> tracing::Dispatch {
            // While at most one dispatcher is registered, tracing-core caches a
            // callsite's interest from whichever dispatcher is current on the
            // thread that first hits it. An uncaptured test thread would then
            // cache `never` for spans and events this capture needs. The
            // always-on global subscriber makes every thread's default enable
            // everything, so no callsite can be cached as disabled.
            trace_capture();
            tracing::Dispatch::new(tracing_subscriber::registry().with(self.clone()))
        }

        fn messages(&self) -> Vec<String> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter_map(|event| event.fields.get("message").cloned())
                .collect()
        }

        /// Every event claiming to be a fire of cron `name`. Matched loosely so
        /// a wrong target, level, or field fails [`assert_fire_receipt`] rather
        /// than reading as a missing receipt.
        fn fire_receipts(&self, name: &str) -> Vec<CapturedEvent> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter(|event| {
                    event
                        .fields
                        .get("message")
                        .is_some_and(|message| message.starts_with("Enqueuing Task"))
                        && event.fields.get("task_name").map(String::as_str) == Some(name)
                })
                .cloned()
                .collect()
        }

        /// WARN events from `cja::cron::registry` for `cron.name == name`,
        /// mirroring `fire_receipts` (filter level + field, not message).
        fn warns_for(&self, name: &str) -> Vec<CapturedEvent> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter(|event| {
                    event.target == "cja::cron::registry"
                        && event.level == tracing::Level::WARN
                        && event.fields.get("cron.name").map(String::as_str) == Some(name)
                })
                .cloned()
                .collect()
        }
    }

    /// The exact event shape Eyes classifies as `cron.fire`.
    fn assert_fire_receipt(
        receipt: &CapturedEvent,
        name: &str,
        last_run: Option<chrono::DateTime<Utc>>,
    ) {
        assert_eq!(receipt.target, "cja::cron::registry", "{receipt:?}");
        assert_eq!(receipt.level, tracing::Level::INFO, "{receipt:?}");
        let field = |key: &str| receipt.fields.get(key).map(String::as_str);
        assert_eq!(field("message"), Some("Enqueuing Task"), "{receipt:?}");
        assert_eq!(field("task_name"), Some(name), "{receipt:?}");
        let last_run = format!("{last_run:?}");
        assert_eq!(field("last_run"), Some(last_run.as_str()), "{receipt:?}");
    }

    /// Moves cron `name` back by `by` so it is due, returning the
    /// `last_run_at` its next fire must report.
    async fn backdate(db: &sqlx::PgPool, name: &str, by: &str) -> chrono::DateTime<Utc> {
        sqlx::query_scalar(
            "UPDATE crons SET last_run_at = clock_timestamp() - $2::text::interval
             WHERE name = $1 RETURNING last_run_at",
        )
        .bind(name)
        .bind(by)
        .fetch_one(db)
        .await
        .unwrap()
    }

    async fn db_now(db: &sqlx::PgPool) -> chrono::DateTime<Utc> {
        sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(db)
            .await
            .unwrap()
    }

    /// A pool whose connections resolve unqualified `clock_timestamp()` to a
    /// shadowing function 30 seconds ahead. Postgres implicitly searches
    /// `pg_catalog` before the rest of the `search_path` unless `pg_catalog`
    /// is named explicitly in it, so the shadow works only because this
    /// `search_path` names `pg_catalog` explicitly, after `skew`: the
    /// resolution order is then `skew`, `public`, `pg_catalog`, and
    /// unqualified `clock_timestamp()` finds the shadow while
    /// `pg_catalog.clock_timestamp()` stays real.
    async fn skewed_pool(db: &sqlx::PgPool) -> sqlx::PgPool {
        sqlx::query("CREATE SCHEMA skew").execute(db).await.unwrap();
        sqlx::query(
            "CREATE FUNCTION skew.clock_timestamp() RETURNS timestamptz LANGUAGE sql
             AS $$ SELECT pg_catalog.clock_timestamp() + interval '30 seconds' $$",
        )
        .execute(db)
        .await
        .unwrap();
        sqlx::PgPool::connect_with(
            (*db.connect_options())
                .clone()
                .options([("search_path", "skew,public,pg_catalog")]),
        )
        .await
        .unwrap()
    }

    const RECEIPT_CALLBACK: &str = "receipt_callback";

    /// A 1-minute `TestJob` cron plus a 1-minute callback cron that logs
    /// `"callback body ran"`.
    fn receipt_registry() -> CronRegistry<TestAppState> {
        let mut registry = CronRegistry::new();
        registry.register_job(TestJob, None, Duration::from_mins(1));
        registry.register(RECEIPT_CALLBACK, None, Duration::from_mins(1), |_, _| {
            Box::pin(async {
                tracing::info!("callback body ran");
                Ok::<(), std::io::Error>(())
            })
        });
        registry
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

    fn job_worker(db: sqlx::PgPool) -> crate::cron::Worker<TestAppState> {
        let state = TestAppState {
            db,
            cookie_key: CookieKey::generate(),
        };
        let mut registry = CronRegistry::new();
        registry.register_job(TestJob, None, Duration::from_mins(1));
        crate::cron::Worker::new(state, registry)
    }

    #[sqlx::test]
    async fn two_pools_commit_one_enqueue_per_interval(db: sqlx::PgPool) {
        let other = sqlx::PgPool::connect_with((*db.connect_options()).clone())
            .await
            .unwrap();
        let first = job_worker(db.clone());
        let second = job_worker(other);
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
            assert_eq!(a, TickOutcome::default());
            assert_eq!(b, TickOutcome::default());
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
            sqlx::query("UPDATE crons SET last_run_at = clock_timestamp() - interval '2 minutes' WHERE name = $1")
                .bind(TestJob::NAME).execute(&db).await.unwrap();
            pair().await;
            assert_eq!(count().await, expected);
            println!("job interval {expected}: {} enqueue(s)", count().await);
            pair().await;
            assert_eq!(count().await, expected);
        }
    }

    #[sqlx::test]
    async fn two_pools_commit_one_enqueue_per_cron_slot(db: sqlx::PgPool) {
        let other = sqlx::PgPool::connect_with((*db.connect_options()).clone())
            .await
            .unwrap();
        let make_worker = |pool| {
            let mut registry = CronRegistry::new();
            registry
                .register_job_with_cron(TestJob, None, "* * * * * * *")
                .unwrap();
            crate::cron::Worker::new(
                TestAppState {
                    db: pool,
                    cookie_key: CookieKey::generate(),
                },
                registry,
            )
        };
        let first = make_worker(db.clone());
        let second = make_worker(other);
        for expected in 1..=4 {
            // Start each pair just after a database-clock slot boundary. A pair
            // straddling a boundary is entitled to two enqueues.
            let db_now = sqlx::query_scalar::<_, chrono::DateTime<Utc>>("SELECT clock_timestamp()")
                .fetch_one(&db)
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(
                1100 - u64::from(db_now.timestamp_subsec_millis()),
            ))
            .await;
            if expected > 1 {
                sqlx::query("UPDATE crons SET last_run_at = clock_timestamp() - interval '2 seconds' WHERE name = $1")
                    .bind(TestJob::NAME).execute(&db).await.unwrap();
            }
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
            assert_eq!(a, TickOutcome::default());
            assert_eq!(b, TickOutcome::default());
            let actual = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs WHERE name = $1")
                .bind(TestJob::NAME)
                .fetch_one(&db)
                .await
                .unwrap();
            assert_eq!(actual, expected);
            println!("cron expression slot {expected}: {actual} enqueue(s)");
        }
    }

    #[sqlx::test]
    async fn two_pools_claim_callback_once_per_interval(db: sqlx::PgPool) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let other = sqlx::PgPool::connect_with((*db.connect_options()).clone())
            .await
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let make_worker = |pool, calls: Arc<AtomicUsize>| {
            let mut registry = CronRegistry::new();
            registry.register("callback", None, Duration::from_mins(1), move |_, _| {
                let calls = calls.clone();
                Box::pin(async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok::<(), std::io::Error>(())
                })
            });
            crate::cron::Worker::new(
                TestAppState {
                    db: pool,
                    cookie_key: CookieKey::generate(),
                },
                registry,
            )
        };
        let first = make_worker(db.clone(), calls.clone());
        let second = make_worker(other, calls.clone());
        for expected in 1..=4 {
            if expected > 1 {
                sqlx::query("UPDATE crons SET last_run_at = clock_timestamp() - interval '2 minutes' WHERE name = 'callback'")
                    .execute(&db).await.unwrap();
            }
            let (a, b) = tokio::join!(first.tick(), second.tick());
            assert_eq!(a, TickOutcome::default());
            assert_eq!(b, TickOutcome::default());
            assert_eq!(calls.load(Ordering::SeqCst), expected);
            println!(
                "callback interval {expected}: {} invocation(s)",
                calls.load(Ordering::SeqCst)
            );
        }
    }

    #[sqlx::test]
    async fn callback_aborted_after_commit_waits_for_next_interval(db: sqlx::PgPool) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = CronRegistry::new();
        let observed = calls.clone();
        registry.register("callback", None, Duration::from_secs(30), move |_, _| {
            let observed = observed.clone();
            Box::pin(async move {
                observed.fetch_add(1, Ordering::SeqCst);
                Ok::<(), std::io::Error>(())
            })
        });
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let pause = Arc::new(CallbackPause {
            committed: sender,
            release: tokio::sync::Notify::new(),
        });
        registry
            .jobs
            .get_mut("callback")
            .unwrap()
            .pause_after_commit = Some(pause.clone());
        let state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };
        let worker = crate::cron::Worker::new(state.clone(), registry);
        let task = tokio::spawn(async move { worker.tick().await });
        let claim_time = tokio::time::timeout(Duration::from_secs(3), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let stored = sqlx::query_scalar::<_, chrono::DateTime<Utc>>(
            "SELECT last_run_at FROM crons WHERE name = 'callback'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(stored, claim_time);
        sqlx::query("UPDATE crons SET last_run_at = clock_timestamp() - interval '31 seconds' WHERE name = 'callback'")
            .execute(&db).await.unwrap();
        let remaining = calls.clone();
        let mut retry = CronRegistry::new();
        retry.register("callback", None, Duration::from_secs(30), move |_, _| {
            let calls = calls.clone();
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok::<(), std::io::Error>(())
            })
        });
        assert_eq!(
            crate::cron::Worker::new(state, retry).tick().await,
            TickOutcome::default()
        );
        assert_eq!(remaining.load(Ordering::SeqCst), 1);
        println!("aborted callback: first interval skipped; next interval invoked once");
    }

    #[sqlx::test]
    async fn dropped_claim_rolls_back_and_next_tick_fires(db: sqlx::PgPool) {
        let state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };
        let mut registry = CronRegistry::new();
        registry.register_job(TestJob, None, Duration::from_secs(864));
        let cron = registry.get(TestJob::NAME).unwrap();
        let claim = cron
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
        assert_eq!(worker.tick().await, TickOutcome::default());
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
        registry.register_job(TestJob, None, Duration::from_secs(864));
        let cron = registry.get(TestJob::NAME).unwrap();
        let claim = cron
            .claim_due(&state, Utc::now(), chrono_tz::UTC)
            .await
            .unwrap()
            .unwrap();
        drop(claim);
        assert_eq!(
            crate::cron::Worker::new(state, registry).tick().await,
            TickOutcome::default()
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs")
                .fetch_one(&db)
                .await
                .unwrap(),
            2
        );
    }

    #[sqlx::test]
    async fn cadence_864_seconds_and_database_stamp(db: sqlx::PgPool) {
        let state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };
        let mut registry = CronRegistry::new();
        registry.register_job(TestJob, None, Duration::from_secs(864));
        let worker = crate::cron::Worker::new(state, registry);
        assert_eq!(worker.tick().await, TickOutcome::default());
        sqlx::query("UPDATE crons SET last_run_at = clock_timestamp() - interval '862 seconds' WHERE name = $1")
            .bind(TestJob::NAME).execute(&db).await.unwrap();
        assert_eq!(worker.tick().await, TickOutcome::default());
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
        assert_eq!(worker.tick().await, TickOutcome::default());
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
    async fn locked_cron_does_not_block_other_crons(db: sqlx::PgPool) {
        let state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };
        let mut registry = CronRegistry::new();
        registry.register_job(TestJob, None, Duration::from_secs(30));
        registry.register_job(SecondTestJob, None, Duration::from_secs(30));
        let worker = crate::cron::Worker::new(state, registry);
        sqlx::query("INSERT INTO crons (cron_id, name, last_run_at, created_at, updated_at) VALUES ($1, $2, clock_timestamp() - interval '60 seconds', clock_timestamp(), clock_timestamp())")
            .bind(uuid::Uuid::new_v4()).bind(TestJob::NAME).execute(&db).await.unwrap();
        let mut blocker = db.begin().await.unwrap();
        sqlx::query("SELECT last_run_at FROM crons WHERE name = $1 FOR UPDATE")
            .bind(TestJob::NAME)
            .fetch_one(&mut *blocker)
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), worker.tick())
                .await
                .unwrap(),
            TickOutcome::default()
        );
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
        assert_eq!(worker.tick().await, TickOutcome::default());
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
    async fn uncommitted_first_row_is_bounded_and_retried(db: sqlx::PgPool) {
        let other = sqlx::PgPool::connect_with((*db.connect_options()).clone())
            .await
            .unwrap();
        let worker = job_worker(other);
        let mut blocker = db.begin().await.unwrap();
        sqlx::query("INSERT INTO crons (cron_id, name, last_run_at, created_at, updated_at) VALUES ($1, $2, clock_timestamp(), clock_timestamp(), clock_timestamp())")
            .bind(uuid::Uuid::new_v4()).bind(TestJob::NAME).execute(&mut *blocker).await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), worker.tick())
                .await
                .unwrap(),
            TickOutcome::default()
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs")
                .fetch_one(&db)
                .await
                .unwrap(),
            0
        );
        blocker.rollback().await.unwrap();
        assert_eq!(worker.tick().await, TickOutcome::default());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs")
                .fetch_one(&db)
                .await
                .unwrap(),
            1
        );
    }

    #[sqlx::test]
    async fn terminated_claimer_rolls_back_and_retries(db: sqlx::PgPool) {
        let other = sqlx::PgPool::connect_with((*db.connect_options()).clone())
            .await
            .unwrap();
        let state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };
        let mut registry = CronRegistry::new();
        registry.register_job(TestJob, None, Duration::from_mins(1));
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
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap(),
            TickOutcome::default()
        );
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
        retry_registry.register_job(TestJob, None, Duration::from_mins(1));
        assert_eq!(
            crate::cron::Worker::new(state, retry_registry).tick().await,
            TickOutcome::default()
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs")
                .fetch_one(&db)
                .await
                .unwrap(),
            1
        );
    }

    #[sqlx::test]
    async fn statement_timeout_is_recoverable(db: sqlx::PgPool) {
        let state = TestAppState {
            db,
            cookie_key: CookieKey::generate(),
        };
        let mut registry = CronRegistry::new();
        registry.register_job(TestJob, None, Duration::from_mins(1));
        let cron = registry.get(TestJob::NAME).unwrap();
        let Claim { mut tx, .. } = cron
            .claim_due(&state, Utc::now(), chrono_tz::UTC)
            .await
            .unwrap()
            .unwrap();
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
    fn termination_sqlstates_are_recoverable() {
        for code in ["55P03", "57014", "25P03", "57P01", "08006", "08003"] {
            assert!(recoverable_sqlstate(code), "{code}");
        }
        for code in ["23505", "42P01", "25P02"] {
            assert!(!recoverable_sqlstate(code), "{code}");
        }
    }

    #[test]
    fn pool_errors_are_recoverable() {
        assert!(recoverable(&sqlx::Error::PoolTimedOut));
        assert!(recoverable(&sqlx::Error::PoolClosed));
        assert!(!recoverable(&sqlx::Error::RowNotFound));
    }

    #[sqlx::test]
    async fn failing_callback_does_not_stop_sibling_crons_in_a_tick(db: sqlx::PgPool) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };
        let failing_calls = Arc::new(AtomicUsize::new(0));
        let healthy_calls = Arc::new(AtomicUsize::new(0));

        // `jobs` is a HashMap: iteration order is arbitrary. Gate on the
        // failing cron iterating FIRST — mirroring `Worker::tick`'s
        // `values()` iteration — so the loop under test cannot reach the
        // sibling only by luck. Each `CronRegistry::new()` gets a fresh
        // RandomState seed, so a rebuild re-rolls the order (~2 tries).
        let mut first_is_failing = None;
        for _ in 0..64 {
            let mut candidate = CronRegistry::new();
            let failing_seen = failing_calls.clone();
            candidate.register("failing", None, Duration::from_millis(1), move |_, _| {
                let failing_seen = failing_seen.clone();
                Box::pin(async move {
                    failing_seen.fetch_add(1, Ordering::SeqCst);
                    Err::<(), std::io::Error>(std::io::Error::other("boom"))
                })
            });
            let healthy_seen = healthy_calls.clone();
            candidate.register(
                "healthy_sibling",
                None,
                Duration::from_millis(1),
                move |_, _| {
                    let healthy_seen = healthy_seen.clone();
                    Box::pin(async move {
                        healthy_seen.fetch_add(1, Ordering::SeqCst);
                        Ok::<(), std::io::Error>(())
                    })
                },
            );
            if candidate.jobs.values().next().map(|job| job.name) == Some("failing") {
                first_is_failing = Some(candidate);
                break;
            }
        }
        let registry =
            first_is_failing.expect("no ordering put the failing cron first in 64 attempts");

        let capture = EventCapture::default();
        let outcome = crate::cron::Worker::new(state, registry)
            .tick()
            .with_subscriber(capture.dispatch())
            .await;
        assert_eq!(
            outcome,
            TickOutcome {
                failed: 1,
                ..TickOutcome::default()
            }
        );
        assert_eq!(healthy_calls.load(Ordering::SeqCst), 1);
        assert_eq!(failing_calls.load(Ordering::SeqCst), 1);
        // The failing claim commits before its callback errors, so both rows exist.
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM crons")
                .fetch_one(&db)
                .await
                .unwrap(),
            2
        );
        // Exactly one ERROR from the worker names the failing cron. Assert
        // level, target and field — not the message text.
        let failing_errors = capture
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|event| {
                event.target == "cja::cron::worker"
                    && event.level == tracing::Level::ERROR
                    && event.fields.get("cron.name").map(String::as_str) == Some("failing")
            })
            .count();
        assert_eq!(failing_errors, 1);
    }

    #[sqlx::test]
    async fn unreadable_crons_map_skips_the_whole_tick(db: sqlx::PgPool) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        sqlx::query("DROP TABLE crons").execute(&db).await.unwrap();
        let state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = CronRegistry::new();
        let seen = calls.clone();
        registry.register("probe", None, Duration::from_millis(1), move |_, _| {
            let seen = seen.clone();
            Box::pin(async move {
                seen.fetch_add(1, Ordering::SeqCst);
                Ok::<(), std::io::Error>(())
            })
        });
        let outcome = crate::cron::Worker::new(state, registry).tick().await;
        assert_eq!(
            outcome,
            TickOutcome {
                map_read_failed: true,
                ..TickOutcome::default()
            }
        );
        // The whole tick is skipped: no callback runs while the map is unreadable.
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
    struct CronContextJob;

    #[async_trait::async_trait]
    impl Job<TestAppState> for CronContextJob {
        const NAME: &'static str = "cron_context_job";

        async fn run(&self, _app_state: TestAppState) -> color_eyre::Result<()> {
            Ok(())
        }
    }

    mod cron_context_jobs {
        use super::{CronContextJob, TestAppState};
        crate::impl_job_registry!(TestAppState, CronContextJob);
    }

    #[sqlx::test]
    async fn cron_triggered_run_span_carries_cron_context(db: sqlx::PgPool) {
        // Eyes contract: `cron_job_failed` monitors only count a job outcome
        // whose `worker.run_job` span has `job.context` starting `Cron@`.
        let lines = trace_capture();
        let state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };
        let mut registry = CronRegistry::new();
        registry.register_job(CronContextJob, None, Duration::from_mins(1));
        assert_eq!(
            crate::cron::Worker::new(state.clone(), registry)
                .tick()
                .await,
            TickOutcome::default()
        );
        let job_id: uuid::Uuid = sqlx::query_scalar("SELECT job_id FROM jobs WHERE name = $1")
            .bind(CronContextJob::NAME)
            .fetch_one(&db)
            .await
            .unwrap();

        let token = crate::jobs::CancellationToken::new();
        let worker = tokio::spawn(crate::jobs::worker::job_worker(
            state,
            cron_context_jobs::Jobs,
            Duration::from_millis(20),
            3,
            token.clone(),
            crate::jobs::JobWorkerConfig::default(),
        ));
        tokio::time::timeout(Duration::from_secs(10), async {
            while sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs WHERE job_id = $1")
                .bind(job_id)
                .fetch_one(&db)
                .await
                .unwrap()
                > 0
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("cron-enqueued job runs");
        token.cancel();
        worker.await.unwrap().unwrap();

        let captured = lines.lock().unwrap().clone();
        let span = captured
            .iter()
            .find(|line| {
                line.starts_with("span:worker.run_job") && line.contains(&job_id.to_string())
            })
            .expect("run span for the cron-enqueued job");
        assert!(span.contains("job.context=\"Cron@test\""), "{span}");
        for field in [
            "job.name",
            "job.priority",
            "job.run_at",
            "job.created_at",
            "job.error_count",
        ] {
            assert!(span.contains(field), "missing {field}: {span}");
        }
    }

    #[sqlx::test]
    async fn enqueue_receipt_follows_commit_and_has_parent_span(db: sqlx::PgPool) {
        let lines = trace_capture();
        let state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };
        let mut registry = CronRegistry::new();
        registry.register_job(TelemetryJob, None, Duration::from_mins(1));
        let worker = crate::cron::Worker::new(state.clone(), registry);
        assert_eq!(worker.tick().await, TickOutcome::default());
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
        let fire =
            |line: &&String| line.contains("Enqueuing Task") && line.contains(TelemetryJob::NAME);
        let fire_receipt = captured.iter().find(fire).expect("cron fire receipt");
        assert!(
            fire_receipt.contains("parent:Some(\"jobs.enqueue\")"),
            "{fire_receipt}"
        );
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
        // The capture is shared by concurrently running tests, so look only at
        // what was recorded from here on instead of clearing it under them.
        let start = lines.lock().unwrap().len();
        assert_eq!(
            worker.tick().await,
            TickOutcome {
                failed: 1,
                ..TickOutcome::default()
            }
        );
        let rejected = lines.lock().unwrap()[start..].to_vec();
        assert!(
            !rejected
                .iter()
                .any(|line| line.contains("job_enqueued") && line.contains(TelemetryJob::NAME))
        );
        assert!(!rejected.iter().any(|line| fire(&line)));
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
        let start = lines.lock().unwrap().len();
        TelemetryJob
            .enqueue(state, "ordinary".into(), None)
            .await
            .unwrap();
        let ordinary = lines.lock().unwrap()[start..].to_vec();
        assert!(
            ordinary
                .iter()
                .any(|line| line.starts_with("span:jobs.enqueue")
                    && line.contains("job.context")
                    && line.contains(TelemetryJob::NAME))
        );
        assert!(
            ordinary
                .iter()
                .any(|line| line.contains("job_enqueued") && line.contains(TelemetryJob::NAME))
        );
    }

    #[sqlx::test]
    async fn job_cron_fire_receipt_once_per_committed_interval(db: sqlx::PgPool) {
        let capture = EventCapture::default();
        let dispatch = capture.dispatch();
        let worker = job_worker(db.clone());
        assert_eq!(
            worker.tick().with_subscriber(dispatch.clone()).await,
            TickOutcome::default()
        );
        let receipts = capture.fire_receipts(TestJob::NAME);
        assert_eq!(receipts.len(), 1, "{receipts:?}");
        assert_fire_receipt(&receipts[0], TestJob::NAME, None);
        // Emitted inside the enqueue span, so the fire shares its trace.
        assert_eq!(receipts[0].parent.as_deref(), Some("jobs.enqueue"));

        // Same interval: nothing is due, so nothing is claimed or reported.
        assert_eq!(
            worker.tick().with_subscriber(dispatch.clone()).await,
            TickOutcome::default()
        );
        assert_eq!(capture.fire_receipts(TestJob::NAME).len(), 1);

        let previous = backdate(&db, TestJob::NAME, "2 minutes").await;
        assert_eq!(
            worker.tick().with_subscriber(dispatch).await,
            TickOutcome::default()
        );
        let receipts = capture.fire_receipts(TestJob::NAME);
        assert_eq!(receipts.len(), 2, "{receipts:?}");
        assert_fire_receipt(&receipts[1], TestJob::NAME, Some(previous));
        assert_eq!(receipts[1].parent.as_deref(), Some("jobs.enqueue"));
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs")
                .fetch_one(&db)
                .await
                .unwrap(),
            2
        );
    }

    #[sqlx::test]
    async fn callback_cron_fire_receipt_precedes_callback(db: sqlx::PgPool) {
        let capture = EventCapture::default();
        let dispatch = capture.dispatch();
        let mut registry = receipt_registry();
        registry.jobs.retain(|name, _| *name == RECEIPT_CALLBACK);
        let state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };
        let worker = crate::cron::Worker::new(state, registry);
        assert_eq!(
            worker.tick().with_subscriber(dispatch.clone()).await,
            TickOutcome::default()
        );
        let receipts = capture.fire_receipts(RECEIPT_CALLBACK);
        assert_eq!(receipts.len(), 1, "{receipts:?}");
        assert_fire_receipt(&receipts[0], RECEIPT_CALLBACK, None);
        assert_eq!(receipts[0].parent.as_deref(), Some("cron_job.tick"));

        assert_eq!(
            worker.tick().with_subscriber(dispatch.clone()).await,
            TickOutcome::default()
        );
        assert_eq!(capture.fire_receipts(RECEIPT_CALLBACK).len(), 1);

        let previous = backdate(&db, RECEIPT_CALLBACK, "2 minutes").await;
        assert_eq!(
            worker.tick().with_subscriber(dispatch).await,
            TickOutcome::default()
        );
        let receipts = capture.fire_receipts(RECEIPT_CALLBACK);
        assert_eq!(receipts.len(), 2, "{receipts:?}");
        assert_fire_receipt(&receipts[1], RECEIPT_CALLBACK, Some(previous));
        // The claim commits, then the receipt, then the callback runs.
        let order: Vec<String> = capture
            .messages()
            .into_iter()
            .filter(|message| message == "Enqueuing Task" || message == "callback body ran")
            .collect();
        assert_eq!(
            order,
            [
                "Enqueuing Task",
                "callback body ran",
                "Enqueuing Task",
                "callback body ran"
            ]
        );
    }

    #[sqlx::test]
    async fn contended_claims_emit_no_fire_receipt(db: sqlx::PgPool) {
        let capture = EventCapture::default();
        let dispatch = capture.dispatch();
        let state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };
        let worker = crate::cron::Worker::new(state, receipt_registry());
        let mut previous = HashMap::new();
        for name in [TestJob::NAME, RECEIPT_CALLBACK] {
            let at = sqlx::query_scalar::<_, chrono::DateTime<Utc>>("INSERT INTO crons (cron_id, name, last_run_at, created_at, updated_at) VALUES ($1, $2, clock_timestamp() - interval '2 minutes', clock_timestamp(), clock_timestamp()) RETURNING last_run_at")
                .bind(uuid::Uuid::new_v4()).bind(name).fetch_one(&db).await.unwrap();
            previous.insert(name, at);
        }
        // Both crons are due, but another scheduler holds their rows.
        let mut blocker = db.begin().await.unwrap();
        sqlx::query("SELECT 1 FROM crons FOR UPDATE")
            .execute(&mut *blocker)
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(3),
                worker.tick().with_subscriber(dispatch.clone()),
            )
            .await
            .unwrap(),
            TickOutcome::default()
        );
        let skipped = capture
            .messages()
            .iter()
            .filter(|message| *message == "Cron claim skipped")
            .count();
        assert_eq!(skipped, 2, "{:?}", capture.messages());
        for name in [TestJob::NAME, RECEIPT_CALLBACK] {
            assert!(capture.fire_receipts(name).is_empty(), "{name}");
        }

        blocker.rollback().await.unwrap();
        assert_eq!(
            worker.tick().with_subscriber(dispatch).await,
            TickOutcome::default()
        );
        for name in [TestJob::NAME, RECEIPT_CALLBACK] {
            let receipts = capture.fire_receipts(name);
            assert_eq!(receipts.len(), 1, "{name}: {receipts:?}");
            assert_fire_receipt(&receipts[0], name, Some(previous[name]));
        }
    }

    #[sqlx::test]
    async fn uncommitted_claims_emit_no_fire_receipt(db: sqlx::PgPool) {
        let other = sqlx::PgPool::connect_with((*db.connect_options()).clone())
            .await
            .unwrap();
        let capture = EventCapture::default();
        let dispatch = capture.dispatch();
        let state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };
        for name in [TestJob::NAME, RECEIPT_CALLBACK] {
            let mut registry = receipt_registry();
            registry.jobs.retain(|registered, _| *registered == name);
            let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
            let pause = Arc::new(ClaimPause {
                claimed: sender,
                release: tokio::sync::Notify::new(),
            });
            registry.jobs.get_mut(name).unwrap().pause_after_claim = Some(pause.clone());
            let worker = crate::cron::Worker::new(state.clone(), registry);
            let task =
                tokio::spawn(async move { worker.tick().await }.with_subscriber(dispatch.clone()));
            let pid = tokio::time::timeout(Duration::from_secs(3), receiver.recv())
                .await
                .unwrap()
                .unwrap();
            // The claim is held but dies before it can commit.
            assert!(
                sqlx::query_scalar::<_, bool>("SELECT pg_terminate_backend($1)")
                    .bind(pid)
                    .fetch_one(&other)
                    .await
                    .unwrap()
            );
            pause.release.notify_one();
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(3), task)
                    .await
                    .unwrap()
                    .unwrap(),
                TickOutcome::default()
            );
            assert!(capture.fire_receipts(name).is_empty(), "{name}");
        }
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM crons")
                .fetch_one(&db)
                .await
                .unwrap(),
            0
        );

        assert_eq!(
            crate::cron::Worker::new(state, receipt_registry())
                .tick()
                .with_subscriber(dispatch)
                .await,
            TickOutcome::default()
        );
        for name in [TestJob::NAME, RECEIPT_CALLBACK] {
            let receipts = capture.fire_receipts(name);
            assert_eq!(receipts.len(), 1, "{name}: {receipts:?}");
            assert_fire_receipt(&receipts[0], name, None);
        }
    }

    #[sqlx::test]
    async fn two_pools_emit_one_fire_receipt_per_interval(db: sqlx::PgPool) {
        let other = sqlx::PgPool::connect_with((*db.connect_options()).clone())
            .await
            .unwrap();
        let capture = EventCapture::default();
        let dispatch = capture.dispatch();
        let make_worker = |pool| {
            let state = TestAppState {
                db: pool,
                cookie_key: CookieKey::generate(),
            };
            crate::cron::Worker::new(state, receipt_registry())
        };
        let first = make_worker(db.clone());
        let second = make_worker(other);
        let mut previous = HashMap::new();
        for interval in 1..=3 {
            if interval > 1 {
                for name in [TestJob::NAME, RECEIPT_CALLBACK] {
                    previous.insert(name, backdate(&db, name, "2 minutes").await);
                }
            }
            let barrier = Arc::new(tokio::sync::Barrier::new(3));
            let (a, b, _) = async {
                tokio::join!(
                    async {
                        barrier.wait().await;
                        first.tick().await
                    },
                    async {
                        barrier.wait().await;
                        second.tick().await
                    },
                    barrier.wait(),
                )
            }
            .with_subscriber(dispatch.clone())
            .await;
            assert_eq!(a, TickOutcome::default());
            assert_eq!(b, TickOutcome::default());
            for name in [TestJob::NAME, RECEIPT_CALLBACK] {
                let receipts = capture.fire_receipts(name);
                assert_eq!(receipts.len(), interval, "{name}: {receipts:?}");
                assert_fire_receipt(receipts.last().unwrap(), name, previous.get(name).copied());
            }
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

        assert_eq!(worker.tick().await, TickOutcome::default());

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

        assert_eq!(worker.tick().await, TickOutcome::default());

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

        assert_eq!(worker.tick().await, TickOutcome::default());

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
                db_now(&db).await,
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
                db_now(&db).await,
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
        assert_eq!(cron_count.count.unwrap(), 1);
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

        assert_eq!(worker.tick().await, TickOutcome::default());

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

        assert_eq!(worker.tick().await, TickOutcome::default());

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
    async fn future_stamp_fires_warns_and_restamps(db: sqlx::PgPool) {
        let state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };
        let mut registry = CronRegistry::new();
        registry.register_job(TestJob, None, Duration::from_mins(1));
        let worker = crate::cron::Worker::new(state, registry);

        let stamped = sqlx::query_scalar::<_, chrono::DateTime<Utc>>(
            "INSERT INTO crons (cron_id, name, last_run_at, created_at, updated_at)
             VALUES ($1, $2, clock_timestamp() + interval '1 day', clock_timestamp(), clock_timestamp())
             RETURNING last_run_at",
        )
        .bind(uuid::Uuid::new_v4())
        .bind(TestJob::NAME)
        .fetch_one(&db)
        .await
        .unwrap();

        let capture = EventCapture::default();
        let before = db_now(&db).await;
        assert_eq!(
            worker.tick().with_subscriber(capture.dispatch()).await,
            TickOutcome::default()
        );
        let after = db_now(&db).await;

        let job_count = sqlx::query!(
            "SELECT COUNT(*) as count FROM jobs WHERE name = $1",
            TestJob::NAME
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(job_count.count.unwrap(), 1);
        let stamp = sqlx::query_scalar::<_, chrono::DateTime<Utc>>(
            "SELECT last_run_at FROM crons WHERE name = $1",
        )
        .bind(TestJob::NAME)
        .fetch_one(&db)
        .await
        .unwrap();
        assert!(stamp >= before && stamp <= after);
        let receipts = capture.fire_receipts(TestJob::NAME);
        assert_eq!(receipts.len(), 1, "{receipts:?}");
        assert_fire_receipt(&receipts[0], TestJob::NAME, Some(stamped));
        let warns = capture.warns_for(TestJob::NAME);
        assert_eq!(warns.len(), 1, "{warns:?}");

        // The restamped row is now in the past and the interval has not
        // elapsed: no re-fire and no repeated warning.
        assert_eq!(
            worker.tick().with_subscriber(capture.dispatch()).await,
            TickOutcome::default()
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs WHERE name = $1")
                .bind(TestJob::NAME)
                .fetch_one(&db)
                .await
                .unwrap(),
            1
        );
        assert_eq!(capture.fire_receipts(TestJob::NAME).len(), 1);
        assert_eq!(capture.warns_for(TestJob::NAME).len(), 1);
    }

    #[sqlx::test]
    async fn future_stamp_on_cron_expression_fires_warns_and_restamps(db: sqlx::PgPool) {
        let state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };
        let mut registry = CronRegistry::new();
        registry
            .register_job_with_cron(TestJob, None, "0 0 3 * * * *")
            .unwrap();
        let worker = crate::cron::Worker::new(state, registry);

        let stamped = sqlx::query_scalar::<_, chrono::DateTime<Utc>>(
            "INSERT INTO crons (cron_id, name, last_run_at, created_at, updated_at)
             VALUES ($1, $2, clock_timestamp() + interval '1 day', clock_timestamp(), clock_timestamp())
             RETURNING last_run_at",
        )
        .bind(uuid::Uuid::new_v4())
        .bind(TestJob::NAME)
        .fetch_one(&db)
        .await
        .unwrap();

        let capture = EventCapture::default();
        let before = db_now(&db).await;
        assert_eq!(
            worker.tick().with_subscriber(capture.dispatch()).await,
            TickOutcome::default()
        );
        let after = db_now(&db).await;

        let job_count = sqlx::query!(
            "SELECT COUNT(*) as count FROM jobs WHERE name = $1",
            TestJob::NAME
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(job_count.count.unwrap(), 1);
        let stamp = sqlx::query_scalar::<_, chrono::DateTime<Utc>>(
            "SELECT last_run_at FROM crons WHERE name = $1",
        )
        .bind(TestJob::NAME)
        .fetch_one(&db)
        .await
        .unwrap();
        assert!(stamp >= before && stamp <= after);
        let receipts = capture.fire_receipts(TestJob::NAME);
        assert_eq!(receipts.len(), 1, "{receipts:?}");
        assert_fire_receipt(&receipts[0], TestJob::NAME, Some(stamped));
        let warns = capture.warns_for(TestJob::NAME);
        assert_eq!(warns.len(), 1, "{warns:?}");

        // The restamped row is in the past and the next slot (03:00) is at
        // least minutes away: no re-fire and no repeated warning.
        assert_eq!(
            worker.tick().with_subscriber(capture.dispatch()).await,
            TickOutcome::default()
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs WHERE name = $1")
                .bind(TestJob::NAME)
                .fetch_one(&db)
                .await
                .unwrap(),
            1
        );
        assert_eq!(capture.fire_receipts(TestJob::NAME).len(), 1);
        assert_eq!(capture.warns_for(TestJob::NAME).len(), 1);
    }

    #[sqlx::test]
    async fn future_stamp_on_callback_cron_fires_warns_and_restamps(db: sqlx::PgPool) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = CronRegistry::new();
        let seen = calls.clone();
        registry.register(
            "failing_callback",
            None,
            Duration::from_mins(1),
            move |_, _| {
                let seen = seen.clone();
                Box::pin(async move {
                    seen.fetch_add(1, Ordering::SeqCst);
                    Err::<(), std::io::Error>(std::io::Error::other("boom"))
                })
            },
        );
        let worker = crate::cron::Worker::new(state, registry);

        let stamped = sqlx::query_scalar::<_, chrono::DateTime<Utc>>(
            "INSERT INTO crons (cron_id, name, last_run_at, created_at, updated_at)
             VALUES ($1, $2, clock_timestamp() + interval '1 day', clock_timestamp(), clock_timestamp())
             RETURNING last_run_at",
        )
        .bind(uuid::Uuid::new_v4())
        .bind("failing_callback")
        .fetch_one(&db)
        .await
        .unwrap();

        // The callback errors, but the claim commits first: the row is
        // restamped and the tick counts one failure.
        let capture = EventCapture::default();
        let before = db_now(&db).await;
        assert_eq!(
            worker.tick().with_subscriber(capture.dispatch()).await,
            TickOutcome {
                failed: 1,
                ..TickOutcome::default()
            }
        );
        let after = db_now(&db).await;

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let warns = capture.warns_for("failing_callback");
        assert_eq!(warns.len(), 1, "{warns:?}");
        let receipts = capture.fire_receipts("failing_callback");
        assert_eq!(receipts.len(), 1, "{receipts:?}");
        assert_fire_receipt(&receipts[0], "failing_callback", Some(stamped));
        let stamp = sqlx::query_scalar::<_, chrono::DateTime<Utc>>(
            "SELECT last_run_at FROM crons WHERE name = $1",
        )
        .bind("failing_callback")
        .fetch_one(&db)
        .await
        .unwrap();
        assert!(stamp >= before && stamp <= after);

        // The restamped row is now in the past and the interval has not
        // elapsed: the callback is not invoked again and the warning does
        // not repeat.
        assert_eq!(
            worker.tick().with_subscriber(capture.dispatch()).await,
            TickOutcome::default()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(capture.warns_for("failing_callback").len(), 1);
    }

    #[sqlx::test]
    async fn slow_callback_does_not_delay_a_later_cron_in_the_same_tick(db: sqlx::PgPool) {
        type Calls = Arc<Mutex<std::collections::HashMap<&'static str, usize>>>;
        type SlowName = Arc<Mutex<Option<&'static str>>>;
        type RunOrder = Arc<Mutex<Vec<&'static str>>>;

        // Both callbacks share one body: count the run, record the order,
        // then sleep only when the shared role holder names this callback
        // as the slow one.
        fn probe_callback(
            name: &'static str,
            calls: Calls,
            slow_name: SlowName,
            run_order: RunOrder,
        ) -> impl Fn(
            TestAppState,
            String,
        ) -> Pin<Box<dyn Future<Output = Result<(), std::io::Error>> + Send>>
        + Send
        + Sync
        + 'static {
            move |_, _| {
                let calls = calls.clone();
                let slow_name = slow_name.clone();
                let run_order = run_order.clone();
                Box::pin(async move {
                    *calls.lock().unwrap().entry(name).or_insert(0) += 1;
                    run_order.lock().unwrap().push(name);
                    if *slow_name.lock().unwrap() == Some(name) {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                    Ok::<(), std::io::Error>(())
                })
            }
        }

        let state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };
        let calls: Calls = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let slow_name: SlowName = Arc::new(Mutex::new(None));
        let run_order: RunOrder = Arc::new(Mutex::new(Vec::new()));

        let mut registry = CronRegistry::new();
        registry.register(
            "probe_a",
            None,
            Duration::from_secs(60),
            probe_callback(
                "probe_a",
                calls.clone(),
                slow_name.clone(),
                run_order.clone(),
            ),
        );
        registry.register(
            "probe_b",
            None,
            Duration::from_secs(60),
            probe_callback(
                "probe_b",
                calls.clone(),
                slow_name.clone(),
                run_order.clone(),
            ),
        );

        // HashMap iteration order is stable for an unmodified map, so the
        // order read here is the order `Worker::tick` iterates. The
        // first-iterated cron becomes the slow one; the second is the late
        // one that must still fire in this tick.
        let mut order = registry.jobs.keys().copied();
        let first = order.next().unwrap();
        let late = order.next().unwrap();
        assert_ne!(first, late);
        *slow_name.lock().unwrap() = Some(first);

        // The slow cron is well past due; the late cron is NOT due at the
        // tick-start snapshot (58s of a 60s interval, 2s of margin) but IS
        // due once the slow callback's 5s sleep has elapsed.
        sqlx::query(
            "INSERT INTO crons (cron_id, name, last_run_at, created_at, updated_at)
             VALUES ($1, $2, clock_timestamp() - interval '2 hours', clock_timestamp(), clock_timestamp())",
        )
        .bind(uuid::Uuid::new_v4())
        .bind(first)
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO crons (cron_id, name, last_run_at, created_at, updated_at)
             VALUES ($1, $2, clock_timestamp() - interval '58 seconds', clock_timestamp(), clock_timestamp())",
        )
        .bind(uuid::Uuid::new_v4())
        .bind(late)
        .execute(&db)
        .await
        .unwrap();

        let worker = crate::cron::Worker::new(state, registry);
        let capture = EventCapture::default();
        assert_eq!(
            worker.tick().with_subscriber(capture.dispatch()).await,
            TickOutcome::default()
        );

        // The slow cron ran first (proving the iteration order matched),
        // and the late cron still fired despite the 5s callback ahead of
        // it: its pre-filter `now` must include the elapsed sleep time.
        assert_eq!(*run_order.lock().unwrap(), vec![first, late]);
        let ran = calls.lock().unwrap().clone();
        assert_eq!(ran.get(first), Some(&1), "slow cron should have run");
        assert_eq!(
            ran.get(late),
            Some(&1),
            "late cron should have run after the slow callback's sleep"
        );
    }

    #[sqlx::test]
    async fn tick_uses_the_provided_now_not_the_app_clock(db: sqlx::PgPool) {
        let state = TestAppState {
            db: db.clone(),
            cookie_key: CookieKey::generate(),
        };
        let mut registry = CronRegistry::new();
        registry.register_job(TestJob, None, Duration::from_secs(60));
        let cron_job = registry.jobs.get(TestJob::NAME).unwrap();
        let stamped = sqlx::query_scalar::<_, chrono::DateTime<Utc>>(
            "INSERT INTO crons (cron_id, name, last_run_at, created_at, updated_at)
             VALUES ($1, $2, clock_timestamp() - interval '61 seconds', clock_timestamp(), clock_timestamp())
             RETURNING last_run_at",
        )
        .bind(uuid::Uuid::new_v4())
        .bind(TestJob::NAME)
        .fetch_one(&db)
        .await
        .unwrap();
        let mut last_enqueue_map = HashMap::new();
        last_enqueue_map.insert(TestJob::NAME.to_string(), stamped);

        // The passed `now` says 51s elapsed: not due. An implementation that
        // ignores `now` and reads `Utc::now()` sees 61s elapsed and fires.
        // App and DB clocks share a host in CI, so only a passed-in `now`
        // that differs from the real clock can discriminate here.
        cron_job
            .tick(
                state.clone(),
                &last_enqueue_map,
                db_now(&db).await - chrono::Duration::seconds(10),
                Utc::now(),
                chrono_tz::UTC,
            )
            .await
            .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs WHERE name = $1")
                .bind(TestJob::NAME)
                .fetch_one(&db)
                .await
                .unwrap(),
            0
        );

        // A fresh database time: 61s elapsed, due.
        cron_job
            .tick(
                state.clone(),
                &last_enqueue_map,
                db_now(&db).await,
                Utc::now(),
                chrono_tz::UTC,
            )
            .await
            .unwrap();
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
    async fn pre_filter_uses_database_clock_when_app_clock_lags(db: sqlx::PgPool) {
        let skewed = skewed_pool(&db).await;
        let state = TestAppState {
            db: skewed.clone(),
            cookie_key: CookieKey::generate(),
        };
        let mut registry = CronRegistry::new();
        registry.register_job(TestJob, None, Duration::from_secs(60));
        // Every clock read in the tick path (per-tick DB time, map read,
        // claim time, restamp) goes through the skewed pool; assertions go
        // through the plain pool.
        let worker = crate::cron::Worker::new(state, registry);

        // Through the skewed pool the stamp lands ≈ real-now − 31s: not in
        // the app clock's future (so the future-stamp override cannot rescue
        // a partial fix), and 61s stale for the database clock.
        sqlx::query(
            "INSERT INTO crons (cron_id, name, last_run_at, created_at, updated_at)
             VALUES ($1, $2, clock_timestamp() - interval '61 seconds', clock_timestamp(), clock_timestamp())",
        )
        .bind(uuid::Uuid::new_v4())
        .bind(TestJob::NAME)
        .execute(&skewed)
        .await
        .unwrap();

        let capture = EventCapture::default();
        assert_eq!(
            worker.tick().with_subscriber(capture.dispatch()).await,
            TickOutcome::default()
        );

        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs WHERE name = $1")
                .bind(TestJob::NAME)
                .fetch_one(&db)
                .await
                .unwrap(),
            1
        );
        assert_eq!(capture.fire_receipts(TestJob::NAME).len(), 1);
        // The stamp is in the past for both clocks: no future-stamp warning.
        assert!(capture.warns_for(TestJob::NAME).is_empty());
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
                db_now(&db).await,
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
                db_now(&db).await,
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

    fn ok_callback()
    -> impl Fn(
        TestAppState,
        String,
    ) -> Pin<Box<dyn Future<Output = Result<(), std::io::Error>> + Send>>
    + Send
    + Sync
    + 'static {
        |_app_state, _context| Box::pin(async { Ok::<(), std::io::Error>(()) })
    }

    #[test]
    #[should_panic(expected = "duplicate cron registration")]
    fn duplicate_callback_registration_panics() {
        let mut registry = CronRegistry::<TestAppState>::new();
        registry.register("dup", None, Duration::from_mins(1), ok_callback());
        registry.register("dup", None, Duration::from_mins(1), ok_callback());
    }

    #[test]
    #[should_panic(expected = "duplicate cron registration")]
    fn duplicate_callback_registration_with_cron_panics() {
        let mut registry = CronRegistry::<TestAppState>::new();
        registry
            .register_with_cron("dup", None, "0 * * * * * *", ok_callback())
            .unwrap();
        // A valid expression: the duplicate, not the parse, must panic.
        registry
            .register_with_cron("dup", None, "0 * * * * * *", ok_callback())
            .unwrap();
    }

    #[test]
    #[should_panic(expected = "duplicate cron registration")]
    fn duplicate_job_registration_panics() {
        let mut registry = CronRegistry::<TestAppState>::new();
        registry.register_job(TestJob, None, Duration::from_mins(1));
        registry.register_job(TestJob, None, Duration::from_mins(1));
    }

    #[test]
    #[should_panic(expected = "duplicate cron registration")]
    fn duplicate_job_registration_with_cron_panics() {
        let mut registry = CronRegistry::<TestAppState>::new();
        registry
            .register_job_with_cron(TestJob, None, "0 * * * * * *")
            .unwrap();
        registry
            .register_job_with_cron(TestJob, None, "0 * * * * * *")
            .unwrap();
    }

    #[test]
    #[should_panic(expected = "duplicate cron registration")]
    fn callback_and_job_names_share_one_namespace_panics() {
        let mut registry = CronRegistry::<TestAppState>::new();
        registry.register(TestJob::NAME, None, Duration::from_mins(1), ok_callback());
        registry.register_job(TestJob, None, Duration::from_mins(1));
    }

    #[test]
    fn duplicate_with_invalid_expression_is_a_parse_error() {
        // Parse happens before the insert, so an invalid expression is an
        // error — not a panic — and leaves the registry untouched.
        let mut registry = CronRegistry::<TestAppState>::new();
        registry.register("dup", None, Duration::from_mins(1), ok_callback());
        let result = registry.register_with_cron("dup", None, "not a cron", ok_callback());
        assert!(result.is_err());
        assert_eq!(registry.jobs.len(), 1);

        let mut job_registry = CronRegistry::<TestAppState>::new();
        job_registry.register_job(TestJob, None, Duration::from_mins(1));
        let result = job_registry.register_job_with_cron(TestJob, None, "not a cron");
        assert!(result.is_err());
        assert_eq!(job_registry.jobs.len(), 1);
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
        assert_eq!(worker.tick().await, TickOutcome::default());

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
            assert_eq!(worker.tick().await, TickOutcome::default());

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

        assert_eq!(worker.tick().await, TickOutcome::default());

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

#[cfg(test)]
mod dst_test {
    //! Cron expressions across daylight-saving transitions, against the pure
    //! `CronSchedule` logic the worker runs on every tick.

    use std::str::FromStr;

    use chrono::{DateTime, Duration, NaiveDateTime, Offset, TimeZone, Timelike, Utc};
    use chrono_tz::Tz;
    use proptest::prelude::*;

    use super::CronSchedule;

    const NEW_YORK: Tz = chrono_tz::America::New_York;

    fn schedule(expr: &str) -> CronSchedule {
        CronSchedule(Box::new(cron::Schedule::from_str(expr).unwrap()))
    }

    fn utc(at: &str) -> DateTime<Utc> {
        at.parse().unwrap()
    }

    /// Local wall-clock time of `at`, computed independently of the code under test.
    fn wall(at: DateTime<Utc>, timezone: Tz) -> NaiveDateTime {
        at.with_timezone(&timezone).naive_local()
    }

    /// Ticks a worker every `tick` from `from` until `until`, starting from a
    /// stored `last_run`, and returns the instants it fired.
    fn fires(
        schedule: &CronSchedule,
        timezone: Tz,
        last_run: DateTime<Utc>,
        (from, until): (DateTime<Utc>, DateTime<Utc>),
        tick: Duration,
    ) -> Vec<DateTime<Utc>> {
        let mut last_run = last_run;
        let mut fired = vec![];
        let mut now = from;
        while now < until {
            if schedule.should_run(Some(&last_run), now, from, timezone) {
                fired.push(now);
                last_run = now;
            }
            now += tick;
        }
        fired
    }

    // New York repeats 01:00-01:59 on 2026-11-01 (EDT 05:00-05:59Z, then EST
    // 06:00-06:59Z) and skips 02:00-02:59 on 2027-03-14 (07:00Z is 03:00 EDT).

    #[test]
    fn repeated_time_fires_once_when_the_worker_runs_all_night() {
        let fired = fires(
            &schedule("0 30 1 * * * *"),
            NEW_YORK,
            utc("2026-10-31T05:30:07Z"),
            (utc("2026-11-01T04:00:03Z"), utc("2026-11-01T09:00:00Z")),
            Duration::seconds(60),
        );
        assert_eq!(fired, vec![utc("2026-11-01T05:30:03Z")]);
    }

    #[test]
    fn worker_back_during_the_second_copy_fires_once() {
        // Down through the EDT copy, back at 01:10 EST. cron's `after` from
        // there returns 01:30 EDT, which is already past, so every tick fired
        // until 01:30 EST: 21 times on a 60 s tick, 121 times on a 10 s tick.
        for tick in [10, 60] {
            let fired = fires(
                &schedule("0 30 1 * * * *"),
                NEW_YORK,
                utc("2026-10-31T05:30:07Z"),
                (utc("2026-11-01T06:10:03Z"), utc("2026-11-01T09:00:00Z")),
                Duration::seconds(tick),
            );
            assert_eq!(fired, vec![utc("2026-11-01T06:30:03Z")], "{tick}s tick");
        }
    }

    #[test]
    fn last_fire_in_the_final_second_before_fall_back_does_not_refire_the_hour() {
        // cron's `after(01:59:59 EDT)` returns 01:00 EDT, an hour before its
        // argument, and the every-minute job then fired on every 10 s tick.
        let fired = fires(
            &schedule("0 * * * * * *"),
            NEW_YORK,
            utc("2026-11-01T05:59:59.400Z"),
            (utc("2026-11-01T06:00:09.400Z"), utc("2026-11-01T07:00:00Z")),
            Duration::seconds(10),
        );
        assert!(fired.is_empty(), "{fired:?}");
    }

    #[test]
    fn skipped_time_fires_when_the_clock_jumps_past_it() {
        let fired = fires(
            &schedule("0 30 2 * * * *"),
            NEW_YORK,
            utc("2027-03-13T07:30:05Z"),
            (utc("2027-03-14T05:00:05Z"), utc("2027-03-14T12:00:00Z")),
            Duration::seconds(60),
        );
        assert_eq!(fired, vec![utc("2027-03-14T07:00:05Z")]);
    }

    #[test]
    fn utc_schedules_are_unchanged() {
        let fired = fires(
            &schedule("0 */20 * * * * *"),
            chrono_tz::UTC,
            utc("2026-11-01T05:40:00.250Z"),
            (utc("2026-11-01T05:40:30Z"), utc("2026-11-01T07:00:00Z")),
            Duration::seconds(30),
        );
        let expected: Vec<_> = ["06:00:00", "06:20:00", "06:40:00"]
            .iter()
            .map(|t| utc(&format!("2026-11-01T{t}Z")))
            .collect();
        assert_eq!(fired, expected);
    }

    #[test]
    fn next_run_is_when_the_wall_clock_reaches_the_slot() {
        let now = utc("2026-10-01T00:00:00Z");
        // Skipped 02:30 is reached when the clock jumps to 03:00 EDT.
        assert_eq!(
            schedule("0 30 2 * * * *").next_run(Some(&utc("2027-03-13T07:30:05Z")), now, NEW_YORK),
            utc("2027-03-14T07:00:00Z")
        );
        // Repeated 01:50 after a fire before the hour: the first copy.
        assert_eq!(
            schedule("0 50 1 * * * *").next_run(Some(&utc("2026-10-31T05:50:00Z")), now, NEW_YORK),
            utc("2026-11-01T05:50:00Z")
        );
        // Repeated 01:50 after a fire at 01:20 EST: the second copy.
        assert_eq!(
            schedule("0 50 1 * * * *").next_run(Some(&utc("2026-11-01T06:20:00Z")), now, NEW_YORK),
            utc("2026-11-01T06:50:00Z")
        );
    }

    /// Instants at which `timezone` changes its UTC offset during `year`.
    fn transitions(timezone: Tz, year: i32) -> Vec<DateTime<Utc>> {
        let offset = |at: DateTime<Utc>| timezone.offset_from_utc_datetime(&at.naive_utc()).fix();
        let start = Utc.with_ymd_and_hms(year, 1, 1, 0, 0, 0).unwrap();
        (0..366 * 24)
            .map(|hour| start + Duration::hours(hour))
            .collect::<Vec<_>>()
            .windows(2)
            .filter(|pair| offset(pair[0]) != offset(pair[1]))
            .map(|pair| pair[1])
            .collect()
    }

    fn transition() -> impl Strategy<Value = (Tz, DateTime<Utc>)> {
        // Lord Howe shifts 30 minutes; Santiago changes at midnight.
        let zones = [
            chrono_tz::America::New_York,
            chrono_tz::Europe::London,
            chrono_tz::Australia::Sydney,
            chrono_tz::Australia::Lord_Howe,
            chrono_tz::America::Santiago,
        ];
        let all: Vec<_> = zones
            .into_iter()
            .flat_map(|tz| [2026, 2027].map(|year| (tz, year)))
            .flat_map(|(tz, year)| transitions(tz, year).into_iter().map(move |at| (tz, at)))
            .collect();
        proptest::sample::select(all)
    }

    fn field(size: u32, hot: &'static [u32]) -> impl Strategy<Value = String> {
        prop_oneof![
            Just("*".to_owned()),
            (2..=size / 2).prop_map(|step| format!("*/{step}")),
            (0..size).prop_map(|value| value.to_string()),
            proptest::sample::select(hot).prop_map(|value| value.to_string()),
            (0..size, 1..size).prop_map(move |(a, b)| format!("{a},{}", (a + b) % size)),
            (0..size, 0..size).prop_map(|(a, b)| format!("{}-{}", a.min(b), a.max(b))),
        ]
    }

    proptest! {
        /// The spec, checked tick by tick against a brute-force oracle: a tick
        /// fires iff some scheduled wall-clock time lies after the wall clock
        /// at the last fire and at or before the wall clock now.
        #[test]
        fn fires_exactly_when_the_wall_clock_passes_a_scheduled_time(
            (timezone, transition) in transition(),
            minute in field(60, &[0, 30, 59]),
            hour in field(24, &[0, 1, 2, 3, 23]),
            tick_secs in 5i64..=120,
            phase_ms in 0i64..120_000,
            lead_minutes in 1i64..=26 * 60,
            outage in proptest::option::of((0i64..=5 * 60, 1i64..=3 * 60)),
        ) {
            let expr = format!("0 {minute} {hour} * * * *");
            let schedule = schedule(&expr);
            let from = transition - Duration::hours(3) + Duration::milliseconds(phase_ms);
            let until = transition + Duration::hours(3);
            let down = outage.map(|(start, length)| {
                (from + Duration::minutes(start), from + Duration::minutes(start + length))
            });
            let mut last_run = from - Duration::minutes(lead_minutes);

            // Every scheduled wall-clock minute in range, found with `includes`
            // rather than the `after` iterator the code under test uses.
            let first = (wall(last_run, timezone) - Duration::hours(2))
                .with_second(0)
                .and_then(|at| at.with_nanosecond(0))
                .unwrap();
            let slots: Vec<NaiveDateTime> = (0..36 * 60)
                .map(|minute| first + Duration::minutes(minute))
                .filter(|at| schedule.0.includes(at.and_utc()))
                .collect();
            let slot_in = |after: NaiveDateTime, upto: NaiveDateTime| {
                let i = slots.partition_point(|slot| *slot <= after);
                slots.get(i).is_some_and(|slot| *slot <= upto)
            };

            let mut now = from;
            while now < until {
                if !down.is_some_and(|(start, end)| start <= now && now < end) {
                    let fired = schedule.should_run(Some(&last_run), now, from, timezone);
                    let expected = slot_in(wall(last_run, timezone), wall(now, timezone));
                    prop_assert_eq!(
                        fired,
                        expected,
                        "{} in {}: tick {} ({}), last fire {} ({})",
                        expr, timezone, now, wall(now, timezone), last_run, wall(last_run, timezone)
                    );
                    if fired {
                        last_run = now;
                    }
                }
                now += Duration::seconds(tick_secs);
            }
        }
    }
}
