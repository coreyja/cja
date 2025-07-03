use std::{collections::HashMap, time::Duration};

use chrono::{DateTime, Utc};

use crate::app_state::AppState as AS;

use super::registry::{CronRegistry, TickError};

pub struct Worker<AppState: AS> {
    id: uuid::Uuid,
    state: AppState,
    registry: CronRegistry<AppState>,
}

impl<AppState: AS> Worker<AppState> {
    pub fn new(state: AppState, registry: CronRegistry<AppState>) -> Self {
        Self {
            id: uuid::Uuid::new_v4(),
            state,
            registry,
        }
    }

    pub async fn run(self) -> Result<(), TickError> {
        tracing::debug!("Starting Cron loop");
        loop {
            self.tick().await?;

            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    }

    async fn last_enqueue_map(&self) -> Result<HashMap<String, DateTime<Utc>>, TickError> {
        let last_runs = sqlx::query!("SELECT name, last_run_at FROM Crons")
            .fetch_all(self.state.db())
            .await
            .map_err(TickError::SqlxError)?;

        let last_run_map: HashMap<String, DateTime<Utc>> = last_runs
            .iter()
            .map(|row| (row.name.to_string(), row.last_run_at))
            .collect();

        Ok(last_run_map)
    }

    #[tracing::instrument(name = "cron.tick", skip_all, fields(cron_worker.id = %self.id))]
    pub(crate) async fn tick(&self) -> Result<(), TickError> {
        let last_enqueue_map = self.last_enqueue_map().await?;
        for job in self.registry.jobs.values() {
            job.tick(self.state.clone(), &last_enqueue_map).await?;
        }

        Ok(())
    }
}
