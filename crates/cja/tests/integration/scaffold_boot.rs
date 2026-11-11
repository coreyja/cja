// Mirrors the boot sequence that `cja new` generates (see
// `generate_app_skeleton` in crates/cli/src/main.rs). If you change the
// generated setup_db_pool, change this to match — the fast CLI unit test
// `generated_boot_path_uses_cja_migrations` guards the template side; this
// test proves the sequence actually works against a fresh database.
use std::time::Duration;

use cja::jobs::{CancellationToken, DEFAULT_MAX_RETRIES, Job, worker::job_worker};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ScaffoldJob;

#[async_trait::async_trait]
impl Job<crate::common::app::TestAppState> for ScaffoldJob {
    const NAME: &'static str = "ScaffoldJob";

    async fn run(&self, _app_state: crate::common::app::TestAppState) -> color_eyre::Result<()> {
        Ok(())
    }
}

// The macro generates a fixed-name `pub struct Jobs;` implementing
// JobRegistry<TestAppState> (crates/cja/src/jobs/registry.rs:92-94).
cja::impl_job_registry!(crate::common::app::TestAppState, ScaffoldJob);

/// The exact boot sequence a generated app runs, twice (two boots), followed
/// by a job-worker startup against the same DB. This is DEV-2188's acceptance:
/// `verify_jobs_schema` must pass on a fresh database.
#[tokio::test]
async fn generated_boot_path_passes_worker_schema_check() {
    let (pool, _guard) = cja::testing::test_db::create_test_database("cja_test_scaffold_")
        .await
        .unwrap();

    for _ in 0..2 {
        cja::db::run_migrations(&pool).await.unwrap();
        let mut app = sqlx::migrate!("./tests/fixtures/app_migrations");
        app.set_ignore_missing(true);
        app.run(&pool).await.unwrap();
    }

    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = 'scaffold_things')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(exists, "app migration never applied");

    // Scaffolds boot with the new framework table too (DEV-2227): the pause
    // claim consults it, so a scaffold missing it would fail every due cron.
    let cron_pauses: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('public.cron_pauses')::text")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(cron_pauses.is_some(), "cron_pauses never applied");

    // Pre-cancel the token: the worker verifies the schema, then exits Ok
    // without claiming anything (job_worker runs verify_jobs_schema before
    // its loop, worker.rs:780, and the loop's select breaks on a cancelled
    // token). On an unmigrated DB this returns the "jobs schema is missing
    // cja migrations" error instead.
    let state = crate::common::app::TestAppState::new(pool.clone());
    let token = CancellationToken::new();
    token.cancel();
    job_worker(
        state,
        Jobs,
        Duration::from_mins(1),
        DEFAULT_MAX_RETRIES,
        token,
        cja::jobs::worker::JobWorkerConfig::default(),
    )
    .await
    .unwrap();
}
