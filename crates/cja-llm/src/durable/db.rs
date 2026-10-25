//! Migrations and every SQL statement the durable engine runs.
//!
//! All statements are runtime-checked (`sqlx::query`/`query_as`, no macros):
//! this crate deliberately stays out of the root `.sqlx` offline cache.
//! Status values are bound as parameters via [`RunStatus::as_str`] — never
//! inlined string literals — so SQL equality (and its case sensitivity)
//! stays visible at every call site.

use std::time::Duration;

use sqlx::Executor;
use uuid::Uuid;

use super::RunStatus;

/// Row data returned by a successful claim.
pub(super) struct ClaimedRun {
    #[expect(dead_code)] // identical to the run id the worker already holds
    pub run_id: Uuid,
    pub kind: String,
    pub model: String,
    pub run_state: serde_json::Value,
    pub cancel_requested: bool,
    pub turns: i32,
    pub input_tokens: i64,
    pub output_tokens: i64,
}

/// Run cja-llm durable-run migrations.
///
/// Must be called AFTER `cja::db::run_migrations`: both migrators share the
/// single `_sqlx_migrations` table, so this one ignores rows it does not
/// recognize (the cja-passkey pattern). No hand-rolled advisory lock —
/// `sqlx::migrate!().run()` advisory-locks internally.
pub async fn run_migrations(pool: &sqlx::PgPool) -> cja::Result<()> {
    let mut migrator = sqlx::migrate!("./migrations");
    migrator.set_ignore_missing(true);
    migrator.run(pool).await?;
    Ok(())
}

/// Claim `run_id` for advancement: `queued`, or `running` with a dead lease
/// (NULL or older than `takeover`), transitions to `running` leased by `me`.
/// `me` is fresh per attempt, so a self-claim branch could never match.
/// `None` = not claimable; disambiguate with [`current_status`].
pub(super) async fn try_claim<'e, E>(
    db: E,
    run_id: Uuid,
    me: &str,
    takeover: Duration,
) -> sqlx::Result<Option<ClaimedRun>>
where
    E: Executor<'e, Database = sqlx::Postgres>,
{
    let takeover_micros = i64::try_from(takeover.as_micros()).unwrap_or(i64::MAX);

    sqlx::query_as::<_, (Uuid, String, String, serde_json::Value, bool, i32, i64, i64)>(
        "UPDATE agent_runs
         SET status = $4, locked_by = $2, locked_at = NOW(), started_at = COALESCE(started_at, NOW())
         WHERE id = $1
           AND (status = $5
                OR (status = $4
                    AND (locked_by IS NULL OR locked_at < NOW() - $3::bigint * interval '1 microsecond')))
         RETURNING id, kind, model, run_state, cancel_requested, turns, input_tokens, output_tokens",
    )
    .bind(run_id)
    .bind(me)
    .bind(takeover_micros)
    .bind(RunStatus::Running.as_str())
    .bind(RunStatus::Queued.as_str())
    .fetch_optional(db)
    .await
    .map(|row| {
        row.map(
            |(run_id, kind, model, run_state, cancel_requested, turns, input_tokens, output_tokens)| {
                ClaimedRun {
                    run_id,
                    kind,
                    model,
                    run_state,
                    cancel_requested,
                    turns,
                    input_tokens,
                    output_tokens,
                }
            },
        )
    })
}

/// The run's current status text, or `None` when the row is missing. Used to
/// disambiguate a failed claim (`Leased` vs `NothingToDo`) and by `cancel`.
pub(super) async fn current_status<'e, E>(db: E, run_id: Uuid) -> sqlx::Result<Option<String>>
where
    E: Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_scalar("SELECT status FROM agent_runs WHERE id = $1")
        .bind(run_id)
        .fetch_optional(db)
        .await
}

/// Release the lease unconditionally on our own `running` row (best-effort,
/// called before every `Err` return from the worker).
pub(super) async fn release_lease<'e, E>(db: E, run_id: Uuid, me: &str) -> sqlx::Result<u64>
where
    E: Executor<'e, Database = sqlx::Postgres>,
{
    Ok(sqlx::query(
        "UPDATE agent_runs SET locked_by = NULL, locked_at = NULL
         WHERE id = $1 AND locked_by = $2 AND status = $3",
    )
    .bind(run_id)
    .bind(me)
    .bind(RunStatus::Running.as_str())
    .execute(db)
    .await?
    .rows_affected())
}

/// Heartbeat the lease; `None` = lost ownership, `Some(cancel_requested)`.
pub(super) async fn heartbeat<'e, E>(db: E, run_id: Uuid, me: &str) -> sqlx::Result<Option<bool>>
where
    E: Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_scalar(
        "UPDATE agent_runs SET locked_at = NOW()
         WHERE id = $1 AND locked_by = $2 AND status = $3
         RETURNING cancel_requested",
    )
    .bind(run_id)
    .bind(me)
    .bind(RunStatus::Running.as_str())
    .fetch_optional(db)
    .await
}

/// Persist one step atomically: conditionally update the run state (proving
/// lease ownership), then insert the newly committed transcript messages, in
/// one transaction. `Ok(None)` = lease lost (write nothing, stop silently).
#[allow(clippy::type_complexity)]
pub(super) async fn persist_step(
    db: &sqlx::PgPool,
    run_id: Uuid,
    me: &str,
    run_state_json: &serde_json::Value,
    turns: i32,
    input_tokens: i64,
    output_tokens: i64,
    new_messages: &[(i64, String, serde_json::Value)],
) -> sqlx::Result<Option<bool>> {
    let mut tx = db.begin().await?;
    let cancel_requested = sqlx::query_scalar::<_, bool>(
        "UPDATE agent_runs
         SET run_state = $3, turns = $4, input_tokens = $5, output_tokens = $6, locked_at = NOW()
         WHERE id = $1 AND locked_by = $2 AND status = $7
         RETURNING cancel_requested",
    )
    .bind(run_id)
    .bind(me)
    .bind(run_state_json)
    .bind(turns)
    .bind(input_tokens)
    .bind(output_tokens)
    .bind(RunStatus::Running.as_str())
    .fetch_optional(&mut *tx)
    .await?;
    let Some(cancel_requested) = cancel_requested else {
        tx.rollback().await?;
        return Ok(None);
    };
    insert_messages(&mut tx, run_id, new_messages).await?;
    tx.commit().await?;
    Ok(Some(cancel_requested))
}

/// Transition `running` (owned by `me`) to `waiting`, recording the suspended
/// ask relationally, clearing the lease, and projecting any not-yet-written
/// transcript rows (rig commits a turn inside `next_step`, so the suspension
/// is the first write after that commit) — one transaction. `false` = lease
/// lost.
pub(super) async fn set_waiting(
    db: &sqlx::PgPool,
    run_id: Uuid,
    me: &str,
    run_state_json: &serde_json::Value,
    pending_call_id: &str,
    pending_question: &str,
    new_messages: &[(i64, String, serde_json::Value)],
) -> sqlx::Result<bool> {
    let mut tx = db.begin().await?;
    let rows = sqlx::query(
        "UPDATE agent_runs
         SET status = $3, run_state = $4, pending_call_id = $5, pending_question = $6,
             locked_by = NULL, locked_at = NULL
         WHERE id = $1 AND locked_by = $2 AND status = $7",
    )
    .bind(run_id)
    .bind(me)
    .bind(RunStatus::Waiting.as_str())
    .bind(run_state_json)
    .bind(pending_call_id)
    .bind(pending_question)
    .bind(RunStatus::Running.as_str())
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if rows == 0 {
        tx.rollback().await?;
        return Ok(false);
    }
    insert_messages(&mut tx, run_id, new_messages).await?;
    tx.commit().await?;
    Ok(true)
}

/// Transition `running` (owned by `me`) to a terminal status, recording the
/// typed failure, the final output, and the preserved transcript state.
/// `false` = lease lost.
#[allow(clippy::too_many_arguments)]
pub(super) async fn finish(
    db: &sqlx::PgPool,
    run_id: Uuid,
    me: &str,
    status: RunStatus,
    fail_reason: Option<&str>,
    output: Option<&str>,
    error: Option<&str>,
    run_state_json: &serde_json::Value,
    turns: i32,
    input_tokens: i64,
    output_tokens: i64,
    new_messages: &[(i64, String, serde_json::Value)],
) -> sqlx::Result<bool> {
    let mut tx = db.begin().await?;
    let rows = sqlx::query(
        "UPDATE agent_runs
         SET status = $3, fail_reason = $4, output = $5, error = $6, run_state = $7,
             turns = $8, input_tokens = $9, output_tokens = $10,
             locked_by = NULL, locked_at = NULL, finished_at = NOW()
         WHERE id = $1 AND locked_by = $2 AND status = $11",
    )
    .bind(run_id)
    .bind(me)
    .bind(status.as_str())
    .bind(fail_reason)
    .bind(output)
    .bind(error)
    .bind(run_state_json)
    .bind(turns)
    .bind(input_tokens)
    .bind(output_tokens)
    .bind(RunStatus::Running.as_str())
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if rows == 0 {
        tx.rollback().await?;
        return Ok(false);
    }
    insert_messages(&mut tx, run_id, new_messages).await?;
    tx.commit().await?;
    Ok(true)
}

/// `answer`'s transition: `waiting` to `running` with the answered run state,
/// pending columns and lease cleared, in one conditional UPDATE on the
/// caller's executor (inside `answer`'s single transaction). Follow with
/// [`insert_messages`] on the same executor. `false` = not `waiting`.
pub(super) async fn answer_waiting<'e, E>(
    db: E,
    run_id: Uuid,
    run_state_json: &serde_json::Value,
) -> sqlx::Result<bool>
where
    E: Executor<'e, Database = sqlx::Postgres>,
{
    let rows = sqlx::query(
        "UPDATE agent_runs
         SET status = $3, run_state = $4, pending_call_id = NULL, pending_question = NULL,
             locked_by = NULL, locked_at = NULL
         WHERE id = $1 AND status = $2",
    )
    .bind(run_id)
    .bind(RunStatus::Waiting.as_str())
    .bind(RunStatus::Running.as_str())
    .bind(run_state_json)
    .execute(db)
    .await?
    .rows_affected();
    Ok(rows > 0)
}

/// Insert committed transcript messages; `(run_id, seq)` conflicts are
/// no-ops (idempotent projection on resume). Takes the caller's connection
/// (e.g. inside `answer`'s transaction) so the rows commit with it.
pub(super) async fn insert_messages(
    conn: &mut sqlx::PgConnection,
    run_id: Uuid,
    new_messages: &[(i64, String, serde_json::Value)],
) -> sqlx::Result<()> {
    for (seq, role, message) in new_messages {
        sqlx::query(
            "INSERT INTO agent_messages (run_id, seq, role, message)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (run_id, seq) DO NOTHING",
        )
        .bind(run_id)
        .bind(seq)
        .bind(role)
        .bind(message)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Directly transition `run_id` to `cancelled` if its current status is one
/// of `statuses` (queued/waiting, or running under a dead lease), clearing
/// the suspended ask columns and the lease. Returns the status it moved
/// FROM, or `None` when no row matched.
pub(super) async fn cancel_now<'e, E>(
    db: E,
    run_id: Uuid,
    statuses: &[RunStatus],
) -> sqlx::Result<Option<RunStatus>>
where
    E: Executor<'e, Database = sqlx::Postgres>,
{
    let wanted: Vec<String> = statuses.iter().map(RunStatus::as_str).map(str::to_owned).collect();
    let from = sqlx::query_scalar::<_, String>(
        "WITH target AS (
             SELECT id, status FROM agent_runs
             WHERE id = $1 AND status = ANY($2)
             FOR UPDATE
         )
         UPDATE agent_runs r
         SET status = $3, pending_call_id = NULL, pending_question = NULL,
             locked_by = NULL, locked_at = NULL, finished_at = NOW()
         FROM target
         WHERE r.id = target.id
         RETURNING target.status",
    )
    .bind(run_id)
    .bind(&wanted)
    .bind(RunStatus::Cancelled.as_str())
    .fetch_optional(db)
    .await?;
    Ok(from.and_then(|s| s.parse().ok()))
}

/// Flag a live `running` run for cancellation; the owner notices at its next
/// step boundary (claim/persist/heartbeat `RETURNING`). `false` = not running.
pub(super) async fn request_cancel<'e, E>(db: E, run_id: Uuid) -> sqlx::Result<bool>
where
    E: Executor<'e, Database = sqlx::Postgres>,
{
    let rows = sqlx::query(
        "UPDATE agent_runs SET cancel_requested = TRUE WHERE id = $1 AND status = $2",
    )
    .bind(run_id)
    .bind(RunStatus::Running.as_str())
    .execute(db)
    .await?
    .rows_affected();
    Ok(rows > 0)
}
