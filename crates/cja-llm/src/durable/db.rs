//! Migrations and every SQL statement the durable engine runs.
//!
//! All statements are runtime-checked (`sqlx::query`/`query_as`, no macros):
//! this crate deliberately stays out of the root `.sqlx` offline cache.
//! Status values are bound as parameters via [`RunStatus::as_str`] — never
//! inlined string literals — so SQL equality (and its case sensitivity)
//! stays visible at every call site.

use std::time::Duration;

use rig_agent::run::AgentRun;
use rig_core::completion::Message;
use sqlx::Executor;
use uuid::Uuid;

use super::{FailReason, RunError, RunStatus};

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

/// The SQLSTATE family Postgres uses to reject a U+0000 (verified against
/// a real server): JSONB rejects the `\u0000` escape (22P05 `unsupported
/// Unicode escape sequence`), TEXT rejects the byte (22021 `invalid byte
/// sequence for encoding "UTF8": 0x00`), and NULs produced server-side
/// surface as 54000. Matched from the real sqlx error's
/// `DatabaseError::code()`, never from message text.
pub(super) fn unstorable_content(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .and_then(sqlx::error::DatabaseError::code)
        .is_some_and(|code| matches!(code.as_ref(), "22P05" | "22021" | "54000"))
}

/// Mark a `running` run `failed` with `unstorable_content` after a write
/// was rejected because its content cannot be stored. Deliberately writes
/// NEITHER `run_state` NOR transcript rows: the failing transaction rolled
/// back, so the row keeps its last good persisted state. Returns `false`
/// when the lease was already lost.
pub(super) async fn fail_unstorable<'e, E>(db: E, run_id: Uuid, me: &str) -> sqlx::Result<bool>
where
    E: Executor<'e, Database = sqlx::Postgres>,
{
    Ok(sqlx::query(
        "UPDATE agent_runs
         SET status = $3, fail_reason = $4, error = $5,
             locked_by = NULL, locked_at = NULL, finished_at = NOW()
         WHERE id = $1 AND locked_by = $2 AND status = $6",
    )
    .bind(run_id)
    .bind(me)
    .bind(RunStatus::Failed.as_str())
    .bind(FailReason::UnstorableContent.as_str())
    .bind("run content cannot be stored: it contains a NUL (U+0000)")
    .bind(RunStatus::Running.as_str())
    .execute(db)
    .await?
    .rows_affected()
        > 0)
}

/// The `answer`-path variant: mark a `waiting` run `failed` with
/// `unstorable_content` (pending columns cleared — the CHECK requires
/// it). Best-effort; the caller surfaces the original storage error either
/// way.
pub(super) async fn fail_unstorable_waiting<'e, E>(db: E, run_id: Uuid) -> sqlx::Result<bool>
where
    E: Executor<'e, Database = sqlx::Postgres>,
{
    Ok(sqlx::query(
        "UPDATE agent_runs
         SET status = $2, fail_reason = $3, error = $4,
             pending_call_id = NULL, pending_question = NULL, finished_at = NOW()
         WHERE id = $1 AND status = $5",
    )
    .bind(run_id)
    .bind(RunStatus::Failed.as_str())
    .bind(FailReason::UnstorableContent.as_str())
    .bind("run content cannot be stored: it contains a NUL (U+0000)")
    .bind(RunStatus::Waiting.as_str())
    .execute(db)
    .await?
    .rows_affected()
        > 0)
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
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
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
/// is the first write after that commit) — one transaction. The
/// `cancel_requested` check and the write are the SAME statement under the
/// row lock (`FOR UPDATE`): a `cancel()` that commits between this
/// attempt's last persist and this write can never slip through and park
/// behind a question — the row finishes `cancelled` instead (pending
/// columns cleared, transcript kept: the projected rows are inserted in
/// the same transaction either way), exactly like a cancel noticed at a
/// step boundary. `Ok(None)` = lease lost (nothing written);
/// `Ok(Some(true))` = cancelled (never parked).
pub(super) async fn set_waiting(
    db: &sqlx::PgPool,
    run_id: Uuid,
    me: &str,
    run_state_json: &serde_json::Value,
    pending_call_id: &str,
    pending_question: &str,
    new_messages: &[(i64, String, serde_json::Value)],
) -> sqlx::Result<Option<bool>> {
    let mut tx = db.begin().await?;
    let parked = sqlx::query_scalar::<_, bool>(
        "WITH decision AS (
             SELECT id, cancel_requested FROM agent_runs
             WHERE id = $1 AND locked_by = $2 AND status = $7
             FOR UPDATE
         )
         UPDATE agent_runs r
         SET status = CASE WHEN d.cancel_requested THEN $8 ELSE $3 END,
             run_state = $4,
             pending_call_id = CASE WHEN d.cancel_requested THEN NULL ELSE $5 END,
             pending_question = CASE WHEN d.cancel_requested THEN NULL ELSE $6 END,
             locked_by = NULL, locked_at = NULL,
             finished_at = CASE WHEN d.cancel_requested THEN NOW() ELSE r.finished_at END
         FROM decision d
         WHERE r.id = d.id
         RETURNING d.cancel_requested",
    )
    .bind(run_id)
    .bind(me)
    .bind(RunStatus::Waiting.as_str())
    .bind(run_state_json)
    .bind(pending_call_id)
    .bind(pending_question)
    .bind(RunStatus::Running.as_str())
    .bind(RunStatus::Cancelled.as_str())
    .fetch_optional(&mut *tx)
    .await?;
    let Some(cancelled) = parked else {
        tx.rollback().await?;
        return Ok(None);
    };
    insert_messages(&mut tx, run_id, new_messages).await?;
    tx.commit().await?;
    Ok(Some(cancelled))
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

/// The transcript role of a rig message (its serde tag is `role`).
pub(super) fn message_role(message: &Message) -> &'static str {
    match message {
        Message::System { .. } => "system",
        Message::User { .. } => "user",
        Message::Assistant(_) => "assistant",
    }
}

/// The next free transcript `seq` for `run_id` (`0` when nothing is written
/// yet). One indexed MAX; safe to call inside an open transaction (the
/// caller's row lock orders it against concurrent projections).
pub(super) async fn next_message_seq<'e, E>(db: E, run_id: Uuid) -> sqlx::Result<i64>
where
    E: Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_scalar("SELECT COALESCE(MAX(seq) + 1, 0) FROM agent_messages WHERE run_id = $1")
        .bind(run_id)
        .fetch_one(db)
        .await
}

/// Compute the transcript rows not yet written for `run`, numbering from
/// the stored high-water mark `written` (see [`next_message_seq`]). Shared
/// by the worker's persists and `answer`'s projection so the two paths
/// cannot drift: same role mapping, same seq arithmetic.
pub(super) fn project_rows(
    written: i64,
    run: &AgentRun,
) -> Result<Vec<(i64, String, serde_json::Value)>, RunError> {
    let mut rows = Vec::new();
    for (i, message) in run
        .messages()
        .iter()
        .skip(usize::try_from(written).unwrap_or(usize::MAX))
        .enumerate()
    {
        let body = serde_json::to_value(&**message)
            .map_err(|_| RunError::Corrupt("transcript message failed to serialize", 0, 0))?;
        let seq = written + i64::try_from(i).unwrap_or(i64::MAX);
        rows.push((seq, message_role(message).to_owned(), body));
    }
    Ok(rows)
}

/// The decision [`cancel_run`] reached under the row lock.
#[derive(Debug)]
pub(super) enum CancelOutcome {
    /// Transitioned to `cancelled` directly (queued/waiting, or `running`
    /// under a dead lease — the orphaned-run escape hatch), clearing the
    /// suspended ask columns and the lease.
    Cancelled,
    /// Flagged `cancel_requested` on a live `running` run; its owner marks
    /// it `cancelled` at the next step boundary (claim/persist/heartbeat
    /// `RETURNING`).
    Requested,
    /// Already `cancelled` — idempotent success (the row is rewritten
    /// with its own values).
    AlreadyCancelled,
    /// Terminal (`succeeded`/`failed`); carries the raw status text for the
    /// caller to parse and reject with `InvalidTransition`.
    Terminal(String),
}

/// Cancel (or request cancellation of) a run in ONE row-locked statement:
/// classify the row under `FOR UPDATE`, then apply the matching action, so
/// a concurrent claim between a read and a conditional write can neither
/// lose the cancellation nor report an existing run as missing — a run
/// claimed while we cancel it is flagged `cancel_requested` for its new
/// owner. `None` = the run does not exist.
pub(super) async fn cancel_run<'e, E>(
    db: E,
    run_id: Uuid,
    takeover: Duration,
) -> sqlx::Result<Option<CancelOutcome>>
where
    E: Executor<'e, Database = sqlx::Postgres>,
{
    let takeover_micros = i64::try_from(takeover.as_micros()).unwrap_or(i64::MAX);
    let direct: Vec<String> = [RunStatus::Queued, RunStatus::Waiting]
        .iter()
        .map(RunStatus::as_str)
        .map(str::to_owned)
        .collect();
    // The `noop`/`terminal` arms rewrite the row's own values (every SET is
    // CASE-guarded), so the single RETURNING row reports every existing-row
    // outcome — no follow-up read, no second race window.
    let row = sqlx::query_as::<_, (String, String)>(
        "WITH target AS (
             SELECT id, status, locked_by, locked_at
             FROM agent_runs
             WHERE id = $1
             FOR UPDATE
         ),
         decision AS (
             SELECT id, status,
                 CASE
                     WHEN status = ANY($2) THEN 'cancel_now'
                     WHEN status = $3
                          AND (locked_by IS NULL OR locked_at IS NULL
                               OR locked_at < NOW() - $4::bigint * interval '1 microsecond')
                         THEN 'cancel_now'
                     WHEN status = $3 THEN 'request'
                     WHEN status = $5 THEN 'noop'
                     ELSE 'terminal'
                 END AS action
             FROM target
         )
         UPDATE agent_runs r
         SET status = CASE WHEN d.action = 'cancel_now' THEN $5 ELSE r.status END,
             pending_call_id =
                 CASE WHEN d.action = 'cancel_now' THEN NULL ELSE r.pending_call_id END,
             pending_question =
                 CASE WHEN d.action = 'cancel_now' THEN NULL ELSE r.pending_question END,
             locked_by = CASE WHEN d.action = 'cancel_now' THEN NULL ELSE r.locked_by END,
             locked_at = CASE WHEN d.action = 'cancel_now' THEN NULL ELSE r.locked_at END,
             cancel_requested =
                 CASE WHEN d.action = 'request' THEN TRUE ELSE r.cancel_requested END,
             finished_at = CASE WHEN d.action = 'cancel_now' THEN NOW() ELSE r.finished_at END
         FROM decision d
         WHERE r.id = d.id
         RETURNING d.action, d.status",
    )
    .bind(run_id)
    .bind(&direct)
    .bind(RunStatus::Running.as_str())
    .bind(takeover_micros)
    .bind(RunStatus::Cancelled.as_str())
    .fetch_optional(db)
    .await?;
    Ok(row.map(|(action, status)| match action.as_str() {
        "cancel_now" => CancelOutcome::Cancelled,
        "request" => CancelOutcome::Requested,
        "noop" => CancelOutcome::AlreadyCancelled,
        _ => CancelOutcome::Terminal(status),
    }))
}

#[cfg(all(test, feature = "durable"))]
mod tests {
    //! Claim/lease predicate tests against real SQL. These exercise the
    //! branches the worker's outcomes depend on: every claimable shape, the
    //! not-claimable shapes, and the ownership conditionals of release and
    //! heartbeat. Status texts are compared by SQL equality only — exactly
    //! what the conditional UPDATEs do — so casing bugs surface here.

    use super::*;
    use std::time::Duration;

    async fn test_db() -> sqlx::PgPool {
        let (pool, guard) = cja::testing::test_db::create_test_database("cja_llm_durable_db_")
            .await
            .expect("create test database");
        sqlx::migrate!("../cja/migrations")
            .run(&pool)
            .await
            .expect("cja migrations");
        crate::durable::run_migrations(&pool)
            .await
            .expect("durable migrations");
        std::mem::forget(guard); // live for the test body; the harness drops the DB
        pool
    }

    /// Insert one run row in the given state; returns its id. A `waiting`
    /// row carries both pending-ask columns (the CHECK requires them).
    async fn insert_run(db: &sqlx::PgPool, status: &str) -> Uuid {
        let id = Uuid::new_v4();
        let (pending_call, pending_question) = if status == "waiting" {
            ("'c'", "'q'")
        } else {
            ("NULL", "NULL")
        };
        sqlx::query(&format!(
            "INSERT INTO agent_runs (id, kind, status, input, model, app, run_state, \
             pending_call_id, pending_question) \
             VALUES ($1, 'k', $2, 'in', 'm', 'a', '{{}}'::jsonb, {pending_call}, {pending_question})"
        ))
        .bind(id)
        .bind(status)
        .execute(db)
        .await
        .expect("insert run row");
        id
    }

    async fn status_of(db: &sqlx::PgPool, id: Uuid) -> String {
        sqlx::query_scalar("SELECT status FROM agent_runs WHERE id = $1")
            .bind(id)
            .fetch_one(db)
            .await
            .expect("status")
    }

    #[tokio::test]
    async fn claim_predicate_branches() {
        let db = test_db().await;
        let takeover = Duration::from_secs(90);

        // queued -> claimed.
        let queued = insert_run(&db, "queued").await;
        assert!(
            try_claim(&db, queued, "me", takeover)
                .await
                .expect("claim queued")
                .is_some()
        );
        assert_eq!(status_of(&db, queued).await, "running");

        // release: owner releases once; a second release is a no-op.
        assert_eq!(
            release_lease(&db, queued, "me").await.expect("release"),
            1,
            "owner on running releases"
        );
        assert_eq!(
            release_lease(&db, queued, "me")
                .await
                .expect("release again"),
            0,
            "no longer leased by me"
        );

        // running, unowned -> claimed (a released lease is immediately
        // re-claimable: the fast retry path).
        let unowned = insert_run(&db, "running").await;
        assert!(
            try_claim(&db, unowned, "me", takeover)
                .await
                .expect("claim unowned running")
                .is_some()
        );

        // running under a FRESH foreign lease -> not claimable; the status
        // read disambiguates Leased (back off) from NothingToDo.
        let leased = insert_run(&db, "running").await;
        sqlx::query("UPDATE agent_runs SET locked_by = 'other', locked_at = NOW() WHERE id = $1")
            .bind(leased)
            .execute(&db)
            .await
            .expect("fresh foreign lease");
        assert!(
            try_claim(&db, leased, "me", takeover)
                .await
                .expect("claim leased running")
                .is_none()
        );
        assert_eq!(
            current_status(&db, leased).await.expect("row exists"),
            Some("running".to_owned()),
            "-> Leased: back off"
        );

        // running under a STALE foreign lease -> taken over.
        sqlx::query(
            "UPDATE agent_runs SET locked_by = 'other', \
             locked_at = NOW() - interval '120 seconds' WHERE id = $1",
        )
        .bind(leased)
        .execute(&db)
        .await
        .expect("stale foreign lease");
        let taken = try_claim(&db, leased, "me", takeover)
            .await
            .expect("claim stale running")
            .expect("stale lease taken over");
        assert_eq!(taken.kind, "k");

        // waiting -> not claimable, NothingToDo (a waiting run already owns
        // a fresh job from answer()).
        let waiting = insert_run(&db, "waiting").await;
        assert!(
            try_claim(&db, waiting, "me", takeover)
                .await
                .expect("claim waiting")
                .is_none()
        );
        assert_eq!(
            current_status(&db, waiting).await.expect("row"),
            Some("waiting".to_owned())
        );

        // terminal -> not claimable, NothingToDo.
        let done = insert_run(&db, "succeeded").await;
        assert!(
            try_claim(&db, done, "me", takeover)
                .await
                .expect("claim succeeded")
                .is_none()
        );
        assert_eq!(
            current_status(&db, done).await.expect("row"),
            Some("succeeded".to_owned()),
            "-> NothingToDo"
        );

        // missing row -> not claimable, and no status to read.
        let missing = Uuid::new_v4();
        assert!(
            try_claim(&db, missing, "me", takeover)
                .await
                .expect("claim missing")
                .is_none()
        );
        assert_eq!(current_status(&db, missing).await.expect("read"), None);
    }

    #[tokio::test]
    async fn heartbeat_ownership_conditionals() {
        let db = test_db().await;
        let id = insert_run(&db, "running").await;
        sqlx::query("UPDATE agent_runs SET locked_by = 'me', locked_at = NOW() WHERE id = $1")
            .bind(id)
            .execute(&db)
            .await
            .expect("lease");

        // Owner, no cancel requested.
        assert_eq!(
            heartbeat(&db, id, "me").await.expect("heartbeat"),
            Some(false)
        );
        // Not the owner.
        assert_eq!(
            heartbeat(&db, id, "other").await.expect("heartbeat other"),
            None,
            "a foreign heartbeat matches nothing"
        );
        // Owner, cancel requested.
        sqlx::query("UPDATE agent_runs SET cancel_requested = TRUE WHERE id = $1")
            .bind(id)
            .execute(&db)
            .await
            .expect("cancel requested");
        assert_eq!(
            heartbeat(&db, id, "me").await.expect("heartbeat cancel"),
            Some(true)
        );
        // Not running: even the owner's heartbeat matches nothing.
        sqlx::query(
            "UPDATE agent_runs SET status = 'waiting', locked_by = NULL, \
                     pending_call_id = 'c', pending_question = 'q' WHERE id = $1",
        )
        .bind(id)
        .execute(&db)
        .await
        .expect("to waiting");
        assert_eq!(
            heartbeat(&db, id, "me").await.expect("heartbeat waiting"),
            None
        );
    }

    /// A wrong-case status text must not match any transition predicate.
    /// Only SQL equality exposes casing bugs: Rust-side `from_str` hides
    /// them, and tests that insert the same wrong literal stay green. The
    /// CHECK constraint blocks such rows from ordinary writes, so drop it
    /// first — the case being pinned is corruption or forward-version skew.
    #[tokio::test]
    async fn wrong_case_status_matches_no_predicate() {
        let db = test_db().await;
        sqlx::query("ALTER TABLE agent_runs DROP CONSTRAINT agent_runs_status_check")
            .execute(&db)
            .await
            .expect("drop status check");
        let id = insert_run(&db, "Running").await;
        assert!(
            try_claim(&db, id, "me", Duration::from_secs(90))
                .await
                .expect("claim wrong-case queued literal")
                .is_none(),
            "'Running' matches neither 'queued' nor 'running'"
        );
        // And the disambiguation read reports the row verbatim: it does not
        // equal 'running', so the worker treats it as NothingToDo, never
        // as Leased.
        assert_ne!(
            current_status(&db, id).await.expect("row"),
            Some("running".to_owned())
        );
    }

    /// A `cancel()` that lands between the attempt's last persist and the
    /// suspension write must never be parked behind the question:
    /// `set_waiting` checks `cancel_requested` under the row lock and
    /// finishes the run `cancelled` (pending columns cleared) instead.
    /// This is the deterministic pin for that race — end-to-end, the
    /// cancel is usually caught one persist earlier, so this in-module
    /// test is what discriminates.
    #[tokio::test]
    async fn set_waiting_with_cancel_requested_finishes_cancelled() {
        let db = test_db().await;
        let id = insert_run(&db, "running").await;
        sqlx::query("UPDATE agent_runs SET locked_by = 'me', locked_at = NOW() WHERE id = $1")
            .bind(id)
            .execute(&db)
            .await
            .expect("lease");
        sqlx::query("UPDATE agent_runs SET cancel_requested = TRUE WHERE id = $1")
            .bind(id)
            .execute(&db)
            .await
            .expect("cancel requested");

        let outcome = set_waiting(
            &db,
            id,
            "me",
            &serde_json::json!({}),
            "call_1",
            "the question?",
            &[],
        )
        .await
        .expect("suspension write");
        assert_eq!(outcome, Some(true), "cancel wins over parking");
        let (status, pending_call, pending_question, locked_by): (
            String,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = sqlx::query_as(
            "SELECT status, pending_call_id, pending_question, locked_by FROM agent_runs WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&db)
        .await
        .expect("row");
        assert_eq!(status, "cancelled", "never parked in waiting");
        assert_eq!(pending_call, None, "pending columns cleared");
        assert_eq!(pending_question, None, "pending columns cleared");
        assert_eq!(locked_by, None, "lease cleared");

        // The healthy path still parks.
        let healthy = insert_run(&db, "running").await;
        sqlx::query("UPDATE agent_runs SET locked_by = 'me', locked_at = NOW() WHERE id = $1")
            .bind(healthy)
            .execute(&db)
            .await
            .expect("lease");
        let outcome = set_waiting(
            &db,
            healthy,
            "me",
            &serde_json::json!({}),
            "call_2",
            "another question?",
            &[],
        )
        .await
        .expect("suspension write");
        assert_eq!(outcome, Some(false), "no cancel: parks in waiting");
        assert_eq!(status_of(&db, healthy).await, "waiting");
    }

    /// `waiting -> cancelled` without clearing both pending columns violates
    /// the `waiting_has_pending_ask` CHECK and fails loudly: a missed clear
    /// can never leave a lying row behind.
    #[tokio::test]
    async fn waiting_cancel_must_clear_pending_columns() {
        let db = test_db().await;
        let id = insert_run(&db, "waiting").await;

        let unclean = sqlx::query("UPDATE agent_runs SET status = 'cancelled' WHERE id = $1")
            .bind(id)
            .execute(&db)
            .await;
        assert!(
            unclean.is_err(),
            "the CHECK rejects a cancelled row that still carries a pending ask"
        );

        // Clearing both columns in the SAME update is accepted.
        sqlx::query(
            "UPDATE agent_runs SET status = 'cancelled', \
             pending_call_id = NULL, pending_question = NULL WHERE id = $1",
        )
        .bind(id)
        .execute(&db)
        .await
        .expect("clean transition");
    }
}
