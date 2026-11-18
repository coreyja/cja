//! A heartbeat cron that proves the site's jobs and cron workers are alive.
//!
//! Every [`SCHEDULE`] slot (evaluated in [`TIMEZONE`]) the cron enqueues a
//! [`HeartbeatJob`]. The job records a row in `heartbeats`, prunes rows older
//! than [`RETENTION_DAYS`], and stamps the row finished. `/status` renders the
//! recent rows.
//!
//! This is also cja's date/time reference app: it touches `timestamptz`
//! (nullable and not), `date`, time zone conversion, cron schedules, and the
//! framework's own `crons` table.

use std::sync::LazyLock;

use cja::cron::{CronRegistry, Schedule};
use cja::jiff::civil::Date;
use cja::jiff::tz::TimeZone;
use cja::jiff::{SignedDuration, Timestamp, ZonedDateTime};
use cja::jiff_sqlx::ToSqlx as _;
use cja::jobs::Job;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::AppState;

/// Time zone the cron schedule is evaluated in and the status page renders in.
pub static TIMEZONE: LazyLock<TimeZone> =
    LazyLock::new(|| TimeZone::get("America/New_York").expect("cja bundles the tz database"));

/// Every five minutes, on the minute (sec min hour day month weekday).
pub const SCHEDULE: &str = "0 */5 * * * *";

/// Heartbeats older than this are pruned by the job.
pub const RETENTION_DAYS: i64 = 7;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatJob;

#[async_trait::async_trait]
impl Job<AppState> for HeartbeatJob {
    const NAME: &'static str = "HeartbeatJob";

    async fn run(&self, app_state: AppState) -> cja::Result<()> {
        record(&app_state.db, Timestamp::now()).await?;
        Ok(())
    }
}

pub fn cron_registry() -> cja::Result<CronRegistry<AppState>> {
    let mut registry = CronRegistry::new();
    registry.register_job_with_cron(
        HeartbeatJob,
        Some("Records a heartbeat for the /status page"),
        SCHEDULE,
    )?;
    Ok(registry)
}

/// The heartbeat's schedule, for computing the next fire time on `/status`.
pub fn schedule(registry: &CronRegistry<AppState>) -> Schedule {
    registry
        .get(HeartbeatJob::NAME)
        .expect("heartbeat cron is registered by cron_registry")
        .schedule
        .clone()
}

/// Calendar day of `at` in [`TIMEZONE`].
pub fn local_date(at: Timestamp) -> Date {
    TIMEZONE.to_datetime(at).date()
}

/// Records one heartbeat started at `started_at`, prunes expired rows, and
/// marks the new row finished. A retried job records a second row, which is
/// harmless for a liveness signal.
pub async fn record(db: &PgPool, started_at: Timestamp) -> cja::Result<uuid::Uuid> {
    let id = sqlx::query_scalar!(
        "INSERT INTO heartbeats (started_at, local_date) VALUES ($1, $2) RETURNING id",
        started_at.to_sqlx(),
        local_date(started_at).to_sqlx(),
    )
    .fetch_one(db)
    .await?;

    let cutoff = started_at - SignedDuration::from_hours(24 * RETENTION_DAYS);
    sqlx::query!(
        "DELETE FROM heartbeats WHERE started_at < $1",
        cutoff.to_sqlx()
    )
    .execute(db)
    .await?;

    sqlx::query!(
        "UPDATE heartbeats SET finished_at = clock_timestamp() WHERE id = $1",
        id
    )
    .execute(db)
    .await?;

    Ok(id)
}

#[derive(Debug, Clone)]
pub struct Heartbeat {
    pub started_at: Timestamp,
    pub finished_at: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DayCount {
    pub local_date: Date,
    pub beats: i64,
}

#[derive(Debug, Clone)]
pub struct Status {
    pub now: Timestamp,
    pub recent: Vec<Heartbeat>,
    pub days: Vec<DayCount>,
    /// The cron's last claim, from cja's `crons` table. `None` before the
    /// first fire.
    pub last_fired_at: Option<Timestamp>,
    pub next_fire_at: ZonedDateTime,
}

pub async fn status(db: &PgPool, schedule: &Schedule, now: Timestamp) -> cja::Result<Status> {
    // Nullable columns decode as `Option<jiff_sqlx::Timestamp>`, which has no
    // `Into<Option<jiff::Timestamp>>`, so `query_as!` can't fill `Heartbeat`.
    let recent = sqlx::query!(
        "SELECT started_at, finished_at
         FROM heartbeats
         ORDER BY started_at DESC
         LIMIT 12"
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|row| Heartbeat {
        started_at: row.started_at.into(),
        finished_at: row.finished_at.map(Into::into),
    })
    .collect();

    let days = sqlx::query_as!(
        DayCount,
        r#"SELECT local_date, count(*) AS "beats!"
           FROM heartbeats
           GROUP BY local_date
           ORDER BY local_date DESC
           LIMIT $1"#,
        RETENTION_DAYS + 1,
    )
    .fetch_all(db)
    .await?;

    let last_fired_at = sqlx::query_scalar!(
        "SELECT last_run_at FROM crons WHERE name = $1",
        HeartbeatJob::NAME
    )
    .fetch_optional(db)
    .await?
    .map(Timestamp::from);

    let next_fire_at = schedule.next_run(last_fired_at.as_ref(), now, &TIMEZONE);

    Ok(Status {
        now,
        recent,
        days,
        last_fired_at,
        next_fire_at,
    })
}

/// Coarse "how long ago" for the status page: `now` for under a minute,
/// then minutes, hours, days. Times in the future (clock skew) read `now`.
pub fn ago(now: Timestamp, then: Timestamp) -> String {
    let elapsed = now.duration_since(then);
    if elapsed < SignedDuration::from_mins(1) {
        "just now".to_string()
    } else if elapsed < SignedDuration::from_hours(1) {
        format!("{}m ago", elapsed.as_mins())
    } else if elapsed < SignedDuration::from_hours(24) {
        format!("{}h ago", elapsed.as_hours())
    } else {
        format!("{}d ago", elapsed.as_hours() / 24)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cja::jiff::civil::date;

    fn utc(rfc3339: &str) -> Timestamp {
        rfc3339.parse().unwrap()
    }

    #[test]
    fn local_date_uses_eastern_calendar_day() {
        // 03:30Z on the 18th is 22:30 EST on the 17th.
        assert_eq!(local_date(utc("2026-11-18T03:30:00Z")), date(2026, 11, 17));
        // 05:30Z is 00:30 EST, already the 18th.
        assert_eq!(local_date(utc("2026-11-18T05:30:00Z")), date(2026, 11, 18));
        // Summer: 03:30Z on Jul 5 is 23:30 EDT on Jul 4.
        assert_eq!(local_date(utc("2026-07-05T03:30:00Z")), date(2026, 7, 4));
    }

    #[test]
    fn ago_buckets() {
        let now = utc("2026-11-17T12:00:00Z");
        assert_eq!(ago(now, utc("2026-11-17T12:00:30Z")), "just now");
        assert_eq!(ago(now, utc("2026-11-17T11:59:30Z")), "just now");
        assert_eq!(ago(now, utc("2026-11-17T11:55:00Z")), "5m ago");
        assert_eq!(ago(now, utc("2026-11-17T09:00:00Z")), "3h ago");
        assert_eq!(ago(now, utc("2026-11-15T12:00:00Z")), "2d ago");
    }

    #[test]
    fn next_fire_is_the_next_five_minute_slot_in_eastern() {
        let registry = cron_registry().unwrap();
        let schedule = schedule(&registry);
        let last = utc("2026-11-17T17:00:00Z");
        let next = schedule.next_run(Some(&last), utc("2026-11-17T17:02:00Z"), &TIMEZONE);
        assert_eq!(
            next,
            date(2026, 11, 17)
                .at(12, 5, 0, 0)
                .to_zoned_datetime(TIMEZONE.clone())
                .unwrap()
        );
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use cja::jiff::civil::date;
    use cja::testing::test_db::create_test_database;

    fn utc(rfc3339: &str) -> Timestamp {
        rfc3339.parse().unwrap()
    }

    #[tokio::test]
    async fn record_and_status_round_trip() {
        let (db, _guard) = create_test_database("cja_site_heartbeat_").await.unwrap();
        crate::migrate(&db).await.unwrap();

        // Older than retention relative to the later beats: pruned by them.
        let stale = utc("2026-11-01T12:00:00Z");
        // 03:30Z on the 18th: still the 17th in Eastern.
        let late_evening = utc("2026-11-18T03:30:00Z");
        let after_midnight = utc("2026-11-18T05:30:00Z");

        record(&db, stale).await.unwrap();
        record(&db, late_evening).await.unwrap();
        record(&db, after_midnight).await.unwrap();

        let registry = cron_registry().unwrap();
        let now = utc("2026-11-18T05:32:00Z");
        let status = status(&db, &schedule(&registry), now).await.unwrap();

        let started: Vec<_> = status.recent.iter().map(|b| b.started_at).collect();
        assert_eq!(started, vec![after_midnight, late_evening]);
        assert!(status.recent.iter().all(|b| b.finished_at.is_some()));
        assert_eq!(
            status.days,
            vec![
                DayCount {
                    local_date: date(2026, 11, 18),
                    beats: 1,
                },
                DayCount {
                    local_date: date(2026, 11, 17),
                    beats: 1,
                },
            ]
        );
        // The job ran outside the cron, so cja's crons table has no row.
        assert_eq!(status.last_fired_at, None);
    }
}
