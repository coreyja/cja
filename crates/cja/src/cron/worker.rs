use std::{collections::HashMap, time::Duration};

use chrono::{DateTime, Utc};
use chrono_tz::Tz;

use crate::app_state::AppState as AS;

use super::registry::{CronRegistry, TickError};

/// Worker that executes cron jobs on a schedule
///
/// The worker runs cron jobs based on their configured schedules. For cron expressions,
/// the timezone parameter determines when the cron expression is evaluated. For example,
/// a cron expression "0 9 * * *" (9 AM daily) will run at 9 AM in the configured timezone.
///
/// Interval-based jobs ignore the timezone and run based on elapsed time since last run.
pub struct Worker<AppState: AS> {
    id: uuid::Uuid,
    state: AppState,
    registry: CronRegistry<AppState>,
    pub(crate) started_at: DateTime<Utc>,
    timezone: Tz,
    sleep_duration: Duration,
}

impl<AppState: AS> Worker<AppState> {
    /// Create a new Worker with UTC as the default timezone
    pub fn new(state: AppState, registry: CronRegistry<AppState>) -> Self {
        Self::new_with_timezone(state, registry, chrono_tz::UTC, Duration::from_secs(60))
    }

    /// Create a new Worker with a specific timezone
    pub fn new_with_timezone(
        state: AppState,
        registry: CronRegistry<AppState>,
        timezone: Tz,
        sleep_duration: Duration,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4(),
            state,
            registry,
            started_at: Utc::now(),
            timezone,
            sleep_duration,
        }
    }

    pub async fn run(self) -> Result<(), TickError> {
        tracing::debug!("Starting Cron loop");
        loop {
            self.tick().await?;

            tokio::time::sleep(self.sleep_duration).await;
        }
    }

    async fn last_enqueue_map(&self) -> Result<HashMap<String, DateTime<Utc>>, TickError> {
        let last_runs = sqlx::query!("SELECT name, last_run_at FROM Crons")
            .fetch_all(self.state.db())
            .await
            .map_err(TickError::SqlxError)?;

        let last_run_map: HashMap<String, DateTime<Utc>> = last_runs
            .iter()
            .map(|row| (row.name.clone(), row.last_run_at))
            .collect();

        Ok(last_run_map)
    }

    #[tracing::instrument(name = "cron.tick", skip_all, fields(cron_worker.id = %self.id))]
    pub(crate) async fn tick(&self) -> Result<(), TickError> {
        let last_enqueue_map = self.last_enqueue_map().await?;
        for job in self.registry.jobs.values() {
            job.tick(
                self.state.clone(),
                &last_enqueue_map,
                self.started_at,
                self.timezone,
            )
            .await?;
        }

        Ok(())
    }
}
