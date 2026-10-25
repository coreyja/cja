//! Durable agent-run integration tests.
//!
//! Each test gets an isolated database (cja migrations + cja-llm durable
//! migrations) and drives runs against `FakeGateway` — no network. The
//! manual two-migrator setup is deliberate: a bare `#[sqlx::test]` attribute
//! applies only one migrations directory and would miss cja's `jobs` table
//! (the cja-passkey precedent).

#![allow(clippy::too_many_lines)]
// `#[rig_tool]` generates the async dispatch boundary itself: a tool fn
// without an interior await trips `unused_async`/`unnecessary_wraps` no
// matter which signature shape is chosen (sync fns trip the generated impl
// instead). These test tools mirror the README's documented shape.
#![allow(clippy::unused_async, clippy::unnecessary_wraps)]

use std::error::Error as _;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use cja::app_state::AppState;
use cja::jobs::worker::job_worker;
use cja::jobs::{CancellationToken, Job, JobWorkerConfig};
use cja::server::cookies::CookieKey;
use cja_llm::config::RetryPolicy;
use cja_llm::durable::{
    self, AdvanceRunJob, AgentKind, Ask, Durable, FailReason, PendingAsk, RunError, RunId,
    RunLimits, RunStatus, ToolCallKey,
};
use cja_llm::rig::rig_agent::rig_tool;
use cja_llm::rig::rig_agent::run::UnhandledInvalidToolCall;
use cja_llm::testing::FakeGateway;
use cja_llm::{LlmConfig, models};

cja::impl_job_registry!(TestState, AdvanceRunJob);

// ---------------------------------------------------------------------------
// Shared fixtures
// ---------------------------------------------------------------------------

#[rig_tool(description = "Add two integers")]
async fn add(a: i32, b: i32) -> Result<i32, rig_core::tool::ToolExecutionError> {
    Ok(a + b)
}

/// Every `ToolCallKey` the `keyed_add` tool ever received, across attempts.
static SEEN_KEYS: Mutex<Vec<ToolCallKey>> = Mutex::new(Vec::new());

#[rig_tool(description = "Record the durable tool call key, then add")]
async fn keyed_add(
    #[rig(context)] context: &mut rig_core::tool::ToolContext,
    a: i32,
    b: i32,
) -> Result<i32, rig_core::tool::ToolExecutionError> {
    let key = context
        .require::<ToolCallKey>()
        .map_err(|e| rig_core::tool::ToolExecutionError::other(e.to_string()))?;
    SEEN_KEYS.lock().expect("key log lock").push(key.clone());
    Ok(a + b)
}

static COUNTER: AgentKind = AgentKind {
    name: "counter",
    model: models::DEFAULT,
    limits: RunLimits {
        max_turns: 20,
        max_total_tokens: 200_000,
    },
    unhandled_invalid_tool_call: UnhandledInvalidToolCall::Fail,
    build: |llm, model| llm.agent(model).tool(Ask).tool(Add),
};

static KEYED: AgentKind = AgentKind {
    name: "keyed",
    model: models::DEFAULT,
    limits: RunLimits {
        max_turns: 20,
        max_total_tokens: 200_000,
    },
    unhandled_invalid_tool_call: UnhandledInvalidToolCall::Fail,
    build: |llm, model| llm.agent(model).tool(Ask).tool(KeyedAdd),
};

static TWO_TURNS: AgentKind = AgentKind {
    name: "two_turns",
    model: models::DEFAULT,
    limits: RunLimits {
        max_turns: 2,
        max_total_tokens: 200_000,
    },
    unhandled_invalid_tool_call: UnhandledInvalidToolCall::Fail,
    build: |llm, model| llm.agent(model).tool(Ask).tool(Add),
};

static TINY_TOKENS: AgentKind = AgentKind {
    name: "tiny_tokens",
    model: models::DEFAULT,
    limits: RunLimits {
        max_turns: 20,
        // The fake reports 3 input + 2 output tokens per reply: 5 > 4.
        max_total_tokens: 4,
    },
    unhandled_invalid_tool_call: UnhandledInvalidToolCall::Fail,
    build: |llm, model| llm.agent(model).tool(Ask).tool(Add),
};

static IGNORE_BAD_TOOLS: AgentKind = AgentKind {
    name: "ignore_bad_tools",
    model: models::DEFAULT,
    limits: RunLimits {
        max_turns: 20,
        max_total_tokens: 200_000,
    },
    unhandled_invalid_tool_call: UnhandledInvalidToolCall::Ignore,
    build: |llm, model| llm.agent(model).tool(Ask).tool(Add),
};

/// A tool whose output carries a U+0000 — Postgres cannot store it, so the
/// worker must sanitize it to U+FFFD before the result enters the run.
#[rig_tool(description = "Return a string containing a NUL byte")]
async fn nul_text() -> Result<String, rig_core::tool::ToolExecutionError> {
    Ok("a\0b".to_owned())
}

static NUL_TOOL: AgentKind = AgentKind {
    name: "nul_tool",
    model: models::DEFAULT,
    limits: RunLimits {
        max_turns: 20,
        max_total_tokens: 200_000,
    },
    unhandled_invalid_tool_call: UnhandledInvalidToolCall::Fail,
    build: |llm, model| llm.agent(model).tool(Ask).tool(NulText),
};

#[derive(Clone)]
struct TestState {
    db: sqlx::PgPool,
    cookie_key: Arc<CookieKey>,
    durable: Durable,
    wakes: Arc<AtomicUsize>,
    /// When set, `job_enqueued` signals arrival and then parks until the
    /// test releases it (test 20's ordering proof).
    park: Option<Arc<(Mutex<bool>, Condvar)>>,
}

impl AppState for TestState {
    fn db(&self) -> &sqlx::PgPool {
        &self.db
    }

    fn version(&self) -> &'static str {
        "test"
    }

    fn cookie_key(&self) -> &CookieKey {
        &self.cookie_key
    }

    fn job_enqueued(&self) {
        self.wakes.fetch_add(1, Ordering::SeqCst);
        if let Some(park) = &self.park {
            let mut arrived = park.0.lock().expect("park lock");
            *arrived = true;
            park.1.notify_all();
            while *arrived {
                arrived = park.1.wait(arrived).expect("parking interrupted");
            }
        }
    }
}

impl durable::HasDurable for TestState {
    fn durable(&self) -> &Durable {
        &self.durable
    }
}

struct Env {
    state: TestState,
    db: sqlx::PgPool,
    fake: FakeGateway,
    _guard: cja::testing::test_db::TestDatabaseGuard,
}

async fn setup_with(kinds: &[&'static AgentKind], takeover: Duration) -> Env {
    setup_full(kinds, takeover, |_| {}).await
}

async fn setup_full(
    kinds: &[&'static AgentKind],
    takeover: Duration,
    configure: impl FnOnce(&mut LlmConfig),
) -> Env {
    let (pool, guard) = cja::testing::test_db::create_test_database("cja_llm_durable_")
        .await
        .expect("create test database");
    sqlx::migrate!("../cja/migrations")
        .run(&pool)
        .await
        .expect("cja migrations");
    durable::run_migrations(&pool)
        .await
        .expect("durable migrations");

    let fake = FakeGateway::start().await;
    let mut config = LlmConfig::new(fake.url())
        .app_name("durable-test")
        .connect_timeout(Duration::from_millis(250))
        .request_timeout(Duration::from_secs(5))
        .retries(RetryPolicy {
            max_attempts: 1,
            initial_backoff: Duration::from_millis(10),
            ..RetryPolicy::default()
        })
        .dialer(Arc::new(setec::TcpDialer));
    configure(&mut config);

    let mut durable = Durable::new(&config)
        .expect("durable engine")
        .lease_takeover(takeover);
    for kind in kinds {
        durable = durable.kind(kind).expect("register kind");
    }

    let state = TestState {
        db: pool.clone(),
        cookie_key: Arc::new(CookieKey::generate()),
        durable,
        wakes: Arc::new(AtomicUsize::new(0)),
        park: None,
    };
    Env {
        state,
        db: pool,
        fake,
        _guard: guard,
    }
}

async fn setup(kinds: &[&'static AgentKind]) -> Env {
    setup_with(kinds, Duration::from_secs(90)).await
}

/// Drive one job attempt directly (no worker): the unit of "one retry".
async fn drive(state: &TestState, run: RunId) -> cja::color_eyre::Result<()> {
    AdvanceRunJob { run_id: run.0 }
        .run_with_cancellation(state.clone(), CancellationToken::new())
        .await
}

async fn spawn_worker(
    state: &TestState,
    sleep: Duration,
) -> (tokio::task::JoinHandle<cja::Result<()>>, CancellationToken) {
    let token = CancellationToken::new();
    let handle = tokio::spawn(job_worker(
        state.clone(),
        Jobs,
        sleep,
        20,
        token.clone(),
        JobWorkerConfig {
            heartbeat_interval: Duration::from_millis(50),
            // 20 heartbeats of slack, not cja's bare 3-heartbeat minimum:
            // under full-suite load a 150ms window lets the worker reclaim
            // its OWN in-flight job (charging an attempt and parking the
            // run behind `Leased` backoff) — harness noise the tests do
            // not intend to exercise, and in the retry-budget test an
            // actual false `error_count` violation. No assertion depends
            // on a short reclaim; nothing here recovers via reclaim.
            reclaim_window: Duration::from_secs(1),
            shutdown_drain_timeout: Duration::ZERO,
            idle_poll_gate: None,
        },
    ));
    (handle, token)
}

async fn wait_until<F, Fut, T>(mut check: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(value) = check().await {
            return value;
        }
        assert!(Instant::now() < deadline, "condition not met within 10s");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn status_of(db: &sqlx::PgPool, run: RunId) -> RunStatus {
    durable::get_run(db, run).await.expect("run row").status
}

async fn view_of(db: &sqlx::PgPool, run: RunId) -> durable::RunView {
    durable::get_run(db, run).await.expect("run row")
}

async fn jobs_for(db: &sqlx::PgPool, run: RunId) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM jobs WHERE name = 'AdvanceRunJob' AND payload->>'run_id' = $1",
    )
    .bind(run.0.to_string())
    .fetch_one(db)
    .await
    .expect("count jobs")
}

async fn locked_by_of(db: &sqlx::PgPool, run: RunId) -> Option<String> {
    sqlx::query_scalar::<_, Option<String>>("SELECT locked_by FROM agent_runs WHERE id = $1")
        .bind(run.0)
        .fetch_one(db)
        .await
        .expect("locked_by")
}

/// (seq, role, message) transcript rows.
async fn transcript_rows(db: &sqlx::PgPool, run: RunId) -> Vec<(i64, String, serde_json::Value)> {
    sqlx::query_as::<_, (i64, String, serde_json::Value)>(
        "SELECT seq, role, message FROM agent_messages WHERE run_id = $1 ORDER BY seq",
    )
    .bind(run.0)
    .fetch_all(db)
    .await
    .expect("transcript rows")
}

/// Script the reference happy path: `add(2,3)` then `"5"`.
fn script_happy(fake: &FakeGateway) {
    fake.tool_call(
        models::DEFAULT,
        "call_1",
        "add",
        &serde_json::json!({"a": 2, "b": 3}),
    );
    fake.chat_completion(models::DEFAULT, "5");
}

/// The happy path against the `keyed` kind, whose tool is `keyed_add`.
fn script_keyed(fake: &FakeGateway) {
    fake.tool_call(
        models::DEFAULT,
        "call_1",
        "keyed_add",
        &serde_json::json!({"a": 2, "b": 3}),
    );
    fake.chat_completion(models::DEFAULT, "5");
}

// ---------------------------------------------------------------------------
// 1. Happy path round trip
// ---------------------------------------------------------------------------

#[tokio::test]
async fn happy_path_round_trip() {
    let env = setup(&[&COUNTER]).await;
    script_happy(&env.fake);

    let run = durable::start(&env.state, "counter", "add 2 and 3")
        .await
        .expect("start");
    drive(&env.state, run).await.expect("drive");

    let view = view_of(&env.db, run).await;
    assert_eq!(view.status, RunStatus::Succeeded);
    assert_eq!(view.output.as_deref(), Some("5"));
    assert_eq!(view.fail_reason, None);
    assert_eq!(view.error, None);
    assert_eq!(view.turns, 2, "two model calls");
    assert_eq!(view.input_tokens, 6, "3 prompt tokens x 2 replies");
    assert_eq!(view.output_tokens, 4, "2 completion tokens x 2 replies");
    assert_eq!(view.model, models::DEFAULT);
    assert_eq!(view.app, "durable-test");
    assert_eq!(view.kind, "counter");

    let rows = transcript_rows(&env.db, run).await;
    let roles: Vec<_> = rows.iter().map(|(_, role, _)| role.as_str()).collect();
    assert_eq!(roles, ["user", "assistant", "user", "assistant"]);
    assert_eq!(env.fake.requests().len(), 2, "exactly two model requests");
}

// ---------------------------------------------------------------------------
// 2. Crash at every step resumes identically
// ---------------------------------------------------------------------------

/// The happy-path script for a crash injected at the ENTRY of persist call
/// `n` (so on disk is everything persist `n - 1` wrote). The fake is FIFO,
/// and a resume re-issues any model call whose reply straddled the death
/// window — so that reply must appear again, in position:
/// * `n == 2`: death between pre-call t0 (1) and post-response t0 (2) —
///   the tool-call reply is served twice.
/// * `n == 5`: death between pre-call t1 (4) and post-response t1 (5) —
///   the final completion is served twice.
fn script_crash(fake: &FakeGateway, n: usize) {
    let tool_call_reply = |fake: &FakeGateway| {
        fake.tool_call(
            models::DEFAULT,
            "call_1",
            "add",
            &serde_json::json!({"a": 2, "b": 3}),
        );
    };
    tool_call_reply(fake);
    if n == 2 {
        tool_call_reply(fake);
    }
    fake.chat_completion(models::DEFAULT, "5");
    if n == 5 {
        fake.chat_completion(models::DEFAULT, "5");
    }
}

#[tokio::test]
async fn crash_at_every_step_resumes_identically() {
    // Reference: one uninterrupted attempt.
    let reference_env = setup(&[&COUNTER]).await;
    script_happy(&reference_env.fake);
    let reference_run = durable::start(&reference_env.state, "counter", "add 2 and 3")
        .await
        .expect("start reference");
    drive(&reference_env.state, reference_run)
        .await
        .expect("reference drive");
    let reference = transcript_rows(&reference_env.db, reference_run).await;
    let reference_requests = reference_env.fake.requests().len();
    let k = reference_env.state.durable.persist_count();
    // The reference flow's persist rhythm, in order:
    //   1 pre-call t0, 2 post-response t0, 3 post-tool-result,
    //   4 pre-call t1, 5 post-response t1, 6 done-path.
    // `script_crash` below maps crash positions onto re-issued model calls
    // in terms of these exact boundaries, so pin the count: if rig's commit
    // rhythm ever changes, the mapping must be revisited with it.
    assert_eq!(k, 6, "reference persist rhythm changed");

    for n in 1..=k {
        let env = setup_with(&[&COUNTER], Duration::from_millis(1)).await;
        script_crash(&env.fake, n);
        env.state.durable.fail_after_persist(n);
        let run = durable::start(&env.state, "counter", "add 2 and 3")
            .await
            .expect("start");

        let mut attempts = 0;
        loop {
            attempts += 1;
            assert!(
                attempts <= 20,
                "run did not complete within 20 attempts (crash at persist {n})"
            );
            // The injected failure leaves the lease held (simulated process
            // death); the 1ms takeover window lets the next attempt re-claim.
            tokio::time::sleep(Duration::from_millis(2)).await;
            let result = drive(&env.state, run).await;
            if result.is_ok() {
                let status = status_of(&env.db, run).await;
                if status == RunStatus::Succeeded {
                    break;
                }
                assert!(
                    status == RunStatus::Running || status == RunStatus::Queued,
                    "crash-at-{n} left status {status:?}"
                );
            }
            // Err = injected failure surfaced (or a retryable); keep going.
        }

        let rows = transcript_rows(&env.db, run).await;
        assert_eq!(
            rows, reference,
            "crash at persist {n}: transcript must equal the uninterrupted run's"
        );
        assert!(
            env.fake.requests().len() >= reference_requests,
            "crash at persist {n}: model was re-called at least the reference number of times"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. Two concurrent workers, one run: no duplicated transcript
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_workers_one_run_no_duplicate_transcript() {
    // Reference for transcript comparison.
    let reference_env = setup(&[&COUNTER]).await;
    script_happy(&reference_env.fake);
    let reference_run = durable::start(&reference_env.state, "counter", "add 2 and 3")
        .await
        .expect("start reference");
    drive(&reference_env.state, reference_run)
        .await
        .expect("reference drive");
    let reference = transcript_rows(&reference_env.db, reference_run).await;

    let env = setup_with(&[&COUNTER], Duration::from_secs(1)).await;
    // A slow first model reply: the first worker's claim is protected by the
    // guard's heartbeat while the duplicate churns.
    env.fake.delay(Duration::from_secs(3));
    env.fake.tool_call(
        models::DEFAULT,
        "call_1",
        "add",
        &serde_json::json!({"a": 2, "b": 3}),
    );
    env.fake.chat_completion(models::DEFAULT, "5");
    let run = durable::start(&env.state, "counter", "add 2 and 3")
        .await
        .expect("start");

    // The one sanctioned raw-SQL jobs INSERT in the repo: a test fixture
    // simulating an ops mistake (a duplicate job row for the same run).
    sqlx::query(
        "INSERT INTO jobs (job_id, name, payload, priority, run_at, created_at, context)
         VALUES ($1, $2, $3, 0, NOW(), NOW(), 'duplicate-fixture')",
    )
    .bind(uuid::Uuid::new_v4())
    .bind(AdvanceRunJob::NAME)
    .bind(serde_json::json!({ "run_id": run.0 }))
    .execute(&env.db)
    .await
    .expect("insert duplicate job row");

    let (worker_a, token_a) = spawn_worker(&env.state, Duration::from_millis(10)).await;
    let (worker_b, token_b) = spawn_worker(&env.state, Duration::from_millis(10)).await;

    let done = tokio::time::timeout(Duration::from_secs(30), async {
        wait_until(|| async {
            let status = status_of(&env.db, run).await;
            (status == RunStatus::Succeeded).then_some(status)
        })
        .await;
    })
    .await;
    assert!(done.is_ok(), "run did not succeed under two workers");

    token_a.cancel();
    token_b.cancel();
    // Explicit handle awaits: a panicked worker would hang a silent join.
    tokio::time::timeout(Duration::from_secs(10), async {
        let _ = worker_a.await;
    })
    .await
    .expect("worker a exited");
    tokio::time::timeout(Duration::from_secs(10), async {
        let _ = worker_b.await;
    })
    .await
    .expect("worker b exited");

    let rows = transcript_rows(&env.db, run).await;
    assert_eq!(rows, reference, "no duplicated or lost transcript rows");
    // (run_id, seq) is the primary key, so uniqueness is structural; the
    // equality above is the stronger claim.
}

// ---------------------------------------------------------------------------
// 2b. Unstorable content (U+0000): sanitize tool output, fail model NULs
// ---------------------------------------------------------------------------

/// A tool output containing U+0000 is sanitized to U+FFFD BEFORE it enters
/// the run: the run succeeds, the stored tool-result row carries the
/// replacement, and a crash between the tool's execution and its persisted
/// result resumes to an IDENTICAL transcript (the re-executed tool gets
/// the same sanitized output — the in-memory run and the stored transcript
/// never diverge). On the pre-fix code the persist of the NUL payload
/// failed as retryable, the tool re-executed forever, and the job
/// dead-lettered, orphaning the run.
#[tokio::test]
async fn tool_output_nul_is_sanitized_and_crash_resume_identical() {
    // Reference: one uninterrupted run.
    let reference_env = setup(&[&NUL_TOOL]).await;
    reference_env.fake.tool_call(
        models::DEFAULT,
        "call_1",
        "nul_text",
        &serde_json::json!({}),
    );
    reference_env.fake.chat_completion(models::DEFAULT, "done");
    let reference_run = durable::start(&reference_env.state, "nul_tool", "give me a nul")
        .await
        .expect("start reference");
    drive(&reference_env.state, reference_run)
        .await
        .expect("reference drive");
    assert_eq!(
        status_of(&reference_env.db, reference_run).await,
        RunStatus::Succeeded
    );
    let reference = transcript_rows(&reference_env.db, reference_run).await;

    // The stored tool-result row contains U+FFFD, never a NUL escape.
    let result_json = serde_json::to_string(&reference[2].2).expect("tool result json");
    assert!(result_json.contains('\u{FFFD}'), "sanitized: {result_json}");
    assert!(
        !result_json.contains("\\u0000"),
        "no NUL escape: {result_json}"
    );

    // Crash after the tool executed but before its result persisted (the
    // persist right after `run.answer` — persist 3 of the reference
    // rhythm), then resume: the tool re-executes and the final transcript
    // is identical to the reference.
    let env = setup_with(&[&NUL_TOOL], Duration::from_millis(1)).await;
    env.fake.tool_call(
        models::DEFAULT,
        "call_1",
        "nul_text",
        &serde_json::json!({}),
    );
    env.fake.chat_completion(models::DEFAULT, "done");
    let run = durable::start(&env.state, "nul_tool", "give me a nul")
        .await
        .expect("start");
    env.state.durable.fail_after_persist(3);
    let mut attempts = 0;
    loop {
        attempts += 1;
        assert!(attempts <= 20, "crash variant did not complete");
        tokio::time::sleep(Duration::from_millis(2)).await;
        if drive(&env.state, run).await.is_ok()
            && status_of(&env.db, run).await == RunStatus::Succeeded
        {
            break;
        }
    }
    assert_eq!(
        transcript_rows(&env.db, run).await,
        reference,
        "crash-resume transcript identical to the uninterrupted run's"
    );
}

/// Model-produced content containing a U+0000 cannot be sanitized (it IS
/// the conversation): the run fails permanently with
/// `unstorable_content`, keeping the last good `run_state` and transcript;
/// the job completes (row deleted, no retry), and the tool before the
/// poisoned reply is NOT re-executed. On the pre-fix code the persist
/// failure was classified retryable: every resume re-ran the tool and
/// re-committed the same NUL until dead-letter orphaned the run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_content_nul_fails_the_run_unstorable() {
    SEEN_KEYS.lock().expect("key log").clear();
    let env = setup(&[&KEYED]).await;
    // The tool call, then the model's reply carrying a NUL in its text.
    env.fake.tool_call(
        models::DEFAULT,
        "call_1",
        "keyed_add",
        &serde_json::json!({"a": 2, "b": 3}),
    );
    env.fake.chat_completion(models::DEFAULT, "a\u{0}b");
    let run = durable::start(&env.state, "keyed", "poison me")
        .await
        .expect("start");

    let (worker, token) = spawn_worker(&env.state, Duration::from_millis(10)).await;
    wait_until(|| async {
        let status = status_of(&env.db, run).await;
        (status == RunStatus::Failed).then_some(status)
    })
    .await;
    token.cancel();
    tokio::time::timeout(Duration::from_secs(10), async {
        let _ = worker.await;
    })
    .await
    .expect("worker exited");

    let view = view_of(&env.db, run).await;
    assert_eq!(view.status, RunStatus::Failed);
    assert_eq!(view.fail_reason, Some(FailReason::UnstorableContent));
    assert_eq!(
        jobs_for(&env.db, run).await,
        0,
        "job completed, not retried"
    );
    assert_eq!(
        env.fake.requests().len(),
        2,
        "the poisoned reply was never re-requested"
    );
    assert_eq!(
        SEEN_KEYS.lock().expect("key log").len(),
        1,
        "the tool executed exactly once — never re-executed"
    );

    // Only the rows written before the failure: user prompt, assistant
    // tool call, user tool result — and none contains a NUL escape.
    let rows = transcript_rows(&env.db, run).await;
    assert_eq!(
        rows.len(),
        3,
        "no writes after the unstorable one: {rows:?}"
    );
    let roles: Vec<_> = rows.iter().map(|(_, role, _)| role.as_str()).collect();
    assert_eq!(roles, ["user", "assistant", "user"]);
    for (_, _, message) in &rows {
        let text = serde_json::to_string(message).unwrap_or_default();
        assert!(!text.contains("\\u0000"), "no NUL stored: {text}");
    }
}

// ---------------------------------------------------------------------------
// 4. Ask suspends; answer resumes as a tool result
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ask_suspends_and_answer_resumes_as_tool_result() {
    let env = setup(&[&COUNTER]).await;
    env.fake.tool_call(
        models::DEFAULT,
        "call_ask",
        "ask",
        &serde_json::json!({"question": "should I continue?"}),
    );
    env.fake.chat_completion(models::DEFAULT, "ok");

    let run = durable::start(&env.state, "counter", "go")
        .await
        .expect("start");

    // Drive via a real job worker so the job row's lifecycle is real: only
    // this path can honestly assert a waiting run holds no worker and no job.
    let (worker, token) = spawn_worker(&env.state, Duration::from_millis(10)).await;
    wait_until(|| async {
        let status = status_of(&env.db, run).await;
        (status == RunStatus::Waiting).then_some(status)
    })
    .await;
    token.cancel();
    tokio::time::timeout(Duration::from_secs(10), async {
        let _ = worker.await;
    })
    .await
    .expect("worker exited");

    let view = view_of(&env.db, run).await;
    assert_eq!(
        view.pending_ask,
        Some(PendingAsk {
            call_id: "call_ask".to_owned(),
            question: "should I continue?".to_owned(),
        })
    );
    assert_eq!(
        locked_by_of(&env.db, run).await,
        None,
        "a waiting run holds no lease"
    );
    assert_eq!(
        jobs_for(&env.db, run).await,
        0,
        "a waiting run holds no job"
    );

    // The last committed assistant message carries the ask tool call.
    let rows = transcript_rows(&env.db, run).await;
    let last_assistant = rows
        .iter()
        .rev()
        .find(|(_, role, _)| role == "assistant")
        .expect("assistant message with the ask call");
    assert!(
        serde_json::to_string(&last_assistant.2)
            .expect("assistant json")
            .contains("call_ask"),
        "assistant message carries the ask call id"
    );

    durable::answer(&env.state, run, "yes")
        .await
        .expect("answer");
    assert_eq!(status_of(&env.db, run).await, RunStatus::Running);
    drive(&env.state, run).await.expect("drive to completion");
    assert_eq!(status_of(&env.db, run).await, RunStatus::Succeeded);

    // The message after the ask assistant message is a `user` message whose
    // content is the tool result for that call id.
    let rows = transcript_rows(&env.db, run).await;
    let ask_pos = rows
        .iter()
        .position(|(_, role, msg)| {
            role == "assistant"
                && serde_json::to_string(msg)
                    .unwrap_or_default()
                    .contains("call_ask")
        })
        .expect("ask assistant message");
    let result_msg = &rows[ask_pos + 1];
    assert_eq!(result_msg.1, "user");
    let call_json = serde_json::to_string(&result_msg.2["content"][0]["call"])
        .expect("tool result call id json");
    assert!(
        call_json.contains("call_ask"),
        "tool result answers the ask call: {call_json}"
    );
    let answer_text =
        serde_json::to_string(&result_msg.2["content"][0]["content"]).unwrap_or_default();
    assert!(
        answer_text.contains("yes"),
        "answer text in result: {answer_text}"
    );
}

// ---------------------------------------------------------------------------
// 5. Answer requires waiting
// ---------------------------------------------------------------------------

#[tokio::test]
async fn answer_requires_waiting() {
    let env = setup(&[&COUNTER, &TINY_TOKENS]).await;

    let queued = durable::start(&env.state, "counter", "x")
        .await
        .expect("start queued");
    let err = durable::answer(&env.state, queued, "no")
        .await
        .expect_err("queued is not waiting");
    assert!(matches!(err, RunError::NotWaiting(_, RunStatus::Queued)));

    // Suspend a run, then race two answers: exactly one Ok.
    let suspended = durable::start(&env.state, "counter", "y")
        .await
        .expect("start suspended");
    env.fake.tool_call(
        models::DEFAULT,
        "c1",
        "ask",
        &serde_json::json!({"question": "q"}),
    );
    env.fake.chat_completion(models::DEFAULT, "done");
    drive(&env.state, suspended).await.expect("suspend");
    assert_eq!(status_of(&env.db, suspended).await, RunStatus::Waiting);

    let state_a = env.state.clone();
    let state_b = env.state.clone();
    let a = durable::answer(&state_a, suspended, "first");
    let b = durable::answer(&state_b, suspended, "second");
    let (a, b) = tokio::join!(a, b);
    let oks = [a.is_ok(), b.is_ok()].iter().filter(|x| **x).count();
    assert_eq!(oks, 1, "exactly one answer wins");
    let loser = if a.is_ok() { b } else { a };
    assert!(matches!(loser, Err(RunError::NotWaiting(..))));
    // Drive the winner to completion so its scripted reply is consumed and
    // the queue is clean for the next part.
    drive(&env.state, suspended)
        .await
        .expect("winner completes");

    // Cancel a waiting run, then answer: NotWaiting.
    let cancelled = durable::start(&env.state, "counter", "z")
        .await
        .expect("start cancelled");
    env.fake.tool_call(
        models::DEFAULT,
        "c2",
        "ask",
        &serde_json::json!({"question": "q"}),
    );
    drive(&env.state, cancelled).await.expect("suspend");
    durable::cancel(&env.state, cancelled)
        .await
        .expect("cancel waiting");
    let err = durable::answer(&env.state, cancelled, "no")
        .await
        .expect_err("cancelled run is not waiting");
    assert!(matches!(err, RunError::NotWaiting(_, RunStatus::Cancelled)));
}

// ---------------------------------------------------------------------------
// 6. Cancel paths
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancel_paths() {
    let env = setup(&[&COUNTER, &TINY_TOKENS]).await;

    // queued -> cancelled; the job later no-ops (no model call).
    let queued = durable::start(&env.state, "counter", "x")
        .await
        .expect("start");
    durable::cancel(&env.state, queued)
        .await
        .expect("cancel queued");
    assert_eq!(status_of(&env.db, queued).await, RunStatus::Cancelled);
    drive(&env.state, queued).await.expect("job no-ops Ok");
    assert_eq!(
        status_of(&env.db, queued).await,
        RunStatus::Cancelled,
        "run untouched"
    );

    // waiting -> cancelled, both pending columns cleared; answer then fails.
    let waiting = durable::start(&env.state, "counter", "y")
        .await
        .expect("start");
    env.fake.tool_call(
        models::DEFAULT,
        "c1",
        "ask",
        &serde_json::json!({"question": "q?"}),
    );
    drive(&env.state, waiting).await.expect("suspend");
    durable::cancel(&env.state, waiting)
        .await
        .expect("cancel waiting");
    assert_eq!(status_of(&env.db, waiting).await, RunStatus::Cancelled);
    let (call_id, question): (Option<String>, Option<String>) =
        sqlx::query_as("SELECT pending_call_id, pending_question FROM agent_runs WHERE id = $1")
            .bind(waiting.0)
            .fetch_one(&env.db)
            .await
            .expect("row");
    assert_eq!(call_id, None, "pending_call_id cleared");
    assert_eq!(question, None, "pending_question cleared");
    assert!(matches!(
        durable::answer(&env.state, waiting, "no").await,
        Err(RunError::NotWaiting(..))
    ));

    // running with a live lease: flagged; owner marks cancelled at the next
    // boundary, transcript preserved.
    let live = durable::start(&env.state, "counter", "z")
        .await
        .expect("start");
    // Under the 5s request timeout, over a heartbeat period: the cancel
    // lands while the reply is genuinely in flight.
    env.fake.delay(Duration::from_secs(3));
    env.fake.chat_completion(models::DEFAULT, "late");
    let state = env.state.clone();
    let live_run = live;
    let driver = tokio::spawn(async move { drive(&state, live_run).await });
    // One request came from the `waiting` part; the live run's model call is
    // the second.
    wait_until(|| async { (env.fake.requests().len() == 2).then_some(()) }).await;
    durable::cancel(&env.state, live)
        .await
        .expect("request cancel");
    tokio::time::timeout(Duration::from_secs(15), driver)
        .await
        .expect("driver returned")
        .expect("join")
        .expect("job completes Ok after cancel");
    assert_eq!(status_of(&env.db, live).await, RunStatus::Cancelled);
    assert!(
        !transcript_rows(&env.db, live).await.is_empty(),
        "transcript preserved on cancel"
    );

    // running with a dead lease and no job: cancelled directly.
    let orphan = durable::start(&env.state, "counter", "o")
        .await
        .expect("start orphan");
    sqlx::query("DELETE FROM jobs WHERE name = 'AdvanceRunJob' AND payload->>'run_id' = $1")
        .bind(orphan.0.to_string())
        .execute(&env.db)
        .await
        .expect("delete job");
    sqlx::query(
        "UPDATE agent_runs SET status = 'running', locked_by = 'dead-worker', \
         locked_at = NOW() - interval '1 hour' WHERE id = $1",
    )
    .bind(orphan.0)
    .execute(&env.db)
    .await
    .expect("fake dead lease");
    durable::cancel(&env.state, orphan)
        .await
        .expect("cancel orphan");
    assert_eq!(status_of(&env.db, orphan).await, RunStatus::Cancelled);

    // Idempotent + terminal refusals + missing.
    durable::cancel(&env.state, orphan)
        .await
        .expect("already cancelled is Ok");
    let done = durable::start(&env.state, "counter", "d")
        .await
        .expect("start done");
    env.fake.chat_completion(models::DEFAULT, "done");
    drive(&env.state, done).await.expect("finish");
    assert!(matches!(
        durable::cancel(&env.state, done).await,
        Err(RunError::InvalidTransition { .. })
    ));
    let failed_env_run = durable::start(&env.state, "tiny_tokens", "f")
        .await
        .expect("start failed");
    env.fake.chat_completion(models::DEFAULT, "hi");
    drive(&env.state, failed_env_run)
        .await
        .expect("fail on budget");
    assert_eq!(status_of(&env.db, failed_env_run).await, RunStatus::Failed);
    assert!(matches!(
        durable::cancel(&env.state, failed_env_run).await,
        Err(RunError::InvalidTransition { .. })
    ));
    assert!(matches!(
        durable::cancel(&env.state, RunId(uuid::Uuid::new_v4())).await,
        Err(RunError::NotFound(_))
    ));
}

// ---------------------------------------------------------------------------
// 6b. Cancel during an in-flight ask reply (operator review round 3, item 3)
// ---------------------------------------------------------------------------

/// A `cancel()` that lands while the ask-returning model reply is in
/// flight must not be lost: the run ends `cancelled` (never parked in
/// `waiting` behind the question), both pending columns are NULL, the
/// transcript is kept, and no job remains. The review's requested
/// end-to-end scenario: usually the cancel is caught by the post-response
/// persist's `cancel_requested` RETURNING (one step earlier — the same
/// user-visible contract); the narrow window between that persist and the
/// suspension write is pinned deterministically by the in-module
/// `set_waiting_with_cancel_requested_finishes_cancelled` test.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_during_in_flight_ask_reply_cancels_the_run() {
    let env = setup(&[&COUNTER]).await;
    env.fake.delay(Duration::from_secs(3));
    env.fake.tool_call(
        models::DEFAULT,
        "call_ask",
        "ask",
        &serde_json::json!({"question": "continue?"}),
    );
    env.fake.chat_completion(models::DEFAULT, "never reached");
    let run = durable::start(&env.state, "counter", "go")
        .await
        .expect("start");

    // A real worker, but with a reclaim window comfortably larger than the
    // 3s in-flight reply: the default test worker's 150ms reclaim would
    // re-claim (and charge) the job while its handler is legitimately
    // parked on the delayed reply.
    let token = CancellationToken::new();
    let worker = tokio::spawn(job_worker(
        env.state.clone(),
        Jobs,
        Duration::from_millis(10),
        20,
        token.clone(),
        JobWorkerConfig {
            heartbeat_interval: Duration::from_millis(100),
            reclaim_window: Duration::from_secs(10),
            shutdown_drain_timeout: Duration::from_secs(2),
            idle_poll_gate: None,
        },
    ));
    wait_until(|| async { (env.fake.requests().len() == 1).then_some(()) }).await;
    durable::cancel(&env.state, run)
        .await
        .expect("cancel while the ask reply is in flight returns Ok");
    wait_until(|| async {
        let status = status_of(&env.db, run).await;
        (status == RunStatus::Cancelled).then_some(status)
    })
    .await;
    token.cancel();
    tokio::time::timeout(Duration::from_secs(10), async {
        let _ = worker.await;
    })
    .await
    .expect("worker exited");

    let view = view_of(&env.db, run).await;
    assert_eq!(view.status, RunStatus::Cancelled);
    assert_eq!(view.error.as_deref(), Some("cancelled by request"));
    assert_eq!(view.pending_ask, None, "pending columns cleared");
    assert_eq!(jobs_for(&env.db, run).await, 0, "no job remains");
    // The transcript keeps everything committed before the cancel: rig
    // exposes the reply's assistant message to `messages()` only at the
    // next `next_step`, so the post-response persist boundary — where this
    // cancel is caught — has exactly the pre-call projection. The turn and
    // token counters DO carry the reply (it was processed), and the
    // suspended-ask columns stay empty: the run never parked.
    assert_eq!(view.turns, 1, "the reply was processed before cancelling");
    let rows = transcript_rows(&env.db, run).await;
    assert_eq!(rows.len(), 1, "committed transcript kept: {rows:?}");
    assert_eq!(rows[0].1, "user");
}

// ---------------------------------------------------------------------------
// 7. Limits
// ---------------------------------------------------------------------------

#[tokio::test]
async fn turn_budget_fails_the_run() {
    let env = setup(&[&TWO_TURNS]).await;
    // The model keeps calling tools; the third model call exceeds the budget.
    for _ in 0..2 {
        env.fake.tool_call(
            models::DEFAULT,
            "c",
            "add",
            &serde_json::json!({"a": 1, "b": 1}),
        );
    }

    let run = durable::start(&env.state, "two_turns", "x")
        .await
        .expect("start");
    drive(&env.state, run).await.expect("job completes Ok");

    let view = view_of(&env.db, run).await;
    assert_eq!(view.status, RunStatus::Failed);
    assert_eq!(view.fail_reason, Some(FailReason::TurnBudget));
    assert!(
        !transcript_rows(&env.db, run).await.is_empty(),
        "transcript preserved"
    );
}

#[tokio::test]
async fn token_budget_fails_the_run() {
    let env = setup(&[&TINY_TOKENS]).await;
    env.fake.chat_completion(models::DEFAULT, "hello");

    let run = durable::start(&env.state, "tiny_tokens", "x")
        .await
        .expect("start");
    drive(&env.state, run).await.expect("job completes Ok");

    let view = view_of(&env.db, run).await;
    assert_eq!(view.status, RunStatus::Failed);
    assert_eq!(view.fail_reason, Some(FailReason::TokenBudget));
    assert!(
        !transcript_rows(&env.db, run).await.is_empty(),
        "transcript preserved"
    );
}

// ---------------------------------------------------------------------------
// 8. Failure classification
// ---------------------------------------------------------------------------

#[tokio::test]
async fn retryable_gateway_errors_fail_the_attempt_not_the_run() {
    // 429: job attempt errors (retryable), lease released, run not failed.
    let env = setup(&[&COUNTER]).await;
    env.fake.status(429, r#"{"type":"rate_limit_exceeded"}"#);
    let run = durable::start(&env.state, "counter", "x")
        .await
        .expect("start");
    let result = drive(&env.state, run).await;
    assert!(result.is_err(), "429 is retryable: the attempt errors");
    assert_eq!(locked_by_of(&env.db, run).await, None, "lease released");
    assert_eq!(
        status_of(&env.db, run).await,
        RunStatus::Running,
        "not failed"
    );
}

#[tokio::test]
async fn budget_exceeded_fails_the_run_immediately() {
    let env = setup(&[&COUNTER]).await;
    env.fake.status(402, r#"{"type":"policy_budget_exceeded"}"#);
    let run = durable::start(&env.state, "counter", "x")
        .await
        .expect("start");
    drive(&env.state, run)
        .await
        .expect("job completes Ok: no retry storm");

    let view = view_of(&env.db, run).await;
    assert_eq!(view.status, RunStatus::Failed);
    assert_eq!(view.fail_reason, Some(FailReason::Gateway));
    assert_eq!(
        env.fake.requests().len(),
        1,
        "exactly one transport attempt"
    );
}

#[tokio::test]
async fn connect_failure_is_retryable_with_lease_released() {
    let dialer = cja_llm::testing::CountingDialer::refusing();
    let env = setup_full(&[&COUNTER], Duration::from_secs(90), |config| {
        *config = config.clone().dialer(dialer.clone());
    })
    .await;
    let run = durable::start(&env.state, "counter", "x")
        .await
        .expect("start");
    let result = drive(&env.state, run).await;
    assert!(result.is_err(), "connection failure is retryable");
    assert!(dialer.dials() >= 1);
    assert_eq!(locked_by_of(&env.db, run).await, None, "lease released");
    assert_eq!(status_of(&env.db, run).await, RunStatus::Running);
}

#[tokio::test]
async fn identity_rejection_fails_the_run_with_gateway_reason() {
    let env = setup(&[&COUNTER]).await;
    env.fake.status(
        403,
        r#"{"error":"forbidden: caller identity could not be determined"}"#,
    );
    let run = durable::start(&env.state, "counter", "x")
        .await
        .expect("start");
    drive(&env.state, run).await.expect("job completes Ok");

    let view = view_of(&env.db, run).await;
    assert_eq!(view.status, RunStatus::Failed);
    assert_eq!(view.fail_reason, Some(FailReason::Gateway));
    let error = view.error.unwrap_or_default();
    assert!(error.contains("identity"), "identity detail: {error}");
}

// ---------------------------------------------------------------------------
// 8b. Gateway Timeout fails the run (no retry — double-billing guard)
// ---------------------------------------------------------------------------

/// A gateway request that outlives `request_timeout` is classified
/// `Error::Timeout`, which is deliberately NON-retryable (the attempt may
/// have been billed upstream; re-calling would double-bill and fork the
/// transcript): the run fails immediately with the gateway reason and its
/// detail, the job completes (row deleted, exactly one request), and no
/// retry ever re-issues the call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gateway_timeout_fails_the_run_without_retry() {
    let env = setup_full(&[&COUNTER], Duration::from_secs(90), |config| {
        *config = config
            .clone()
            .request_timeout(Duration::from_millis(300))
            .retries(RetryPolicy {
                max_attempts: 1,
                initial_backoff: Duration::from_millis(10),
                ..RetryPolicy::default()
            });
    })
    .await;
    // The reply would land at 2s — far past the 300ms request timeout.
    env.fake.delay(Duration::from_secs(2));
    env.fake.chat_completion(models::DEFAULT, "too late");
    let run = durable::start(&env.state, "counter", "x")
        .await
        .expect("start");

    let (worker, token) = spawn_worker(&env.state, Duration::from_millis(10)).await;
    wait_until(|| async {
        let status = status_of(&env.db, run).await;
        (status == RunStatus::Failed).then_some(status)
    })
    .await;
    token.cancel();
    tokio::time::timeout(Duration::from_secs(10), async {
        let _ = worker.await;
    })
    .await
    .expect("worker exited");

    let view = view_of(&env.db, run).await;
    assert_eq!(view.status, RunStatus::Failed);
    assert_eq!(view.fail_reason, Some(FailReason::Gateway));
    assert!(
        view.error.as_deref().unwrap_or("").contains("timed out"),
        "the timeout detail is stored: {:?}",
        view.error
    );
    assert_eq!(jobs_for(&env.db, run).await, 0, "job row gone: no retry");
    assert_eq!(
        env.fake.requests().len(),
        1,
        "the call was issued exactly once — never re-issued"
    );
    assert!(
        !transcript_rows(&env.db, run).await.is_empty(),
        "transcript kept"
    );
}

// ---------------------------------------------------------------------------
// 9. Retryable error and shutdown release the lease (the regression test)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn retryable_error_releases_lease_and_retry_completes() {
    let env = setup_with(&[&COUNTER], Duration::from_secs(30)).await;
    // 503 past the transport's internal retries (max_attempts: 1).
    env.fake.status(503, r#"{"error":"upstream down"}"#);
    let run = durable::start(&env.state, "counter", "x")
        .await
        .expect("start");

    let result = drive(&env.state, run).await;
    assert!(result.is_err(), "503 is retryable");
    assert_eq!(
        locked_by_of(&env.db, run).await,
        None,
        "lease released before the Err return"
    );

    // Immediately (no sleep): a fresh attempt with a now-working fake.
    env.fake.tool_call(
        models::DEFAULT,
        "call_1",
        "add",
        &serde_json::json!({"a": 2, "b": 3}),
    );
    env.fake.chat_completion(models::DEFAULT, "5");
    drive(&env.state, run).await.expect("retry succeeds");
    assert_eq!(status_of(&env.db, run).await, RunStatus::Succeeded);
}

#[tokio::test]
async fn shutdown_mid_model_call_aborts_promptly_and_releases_lease() {
    let env = setup_with(&[&COUNTER], Duration::from_secs(30)).await;
    env.fake.delay(Duration::from_secs(5));
    env.fake.chat_completion(models::DEFAULT, "5");
    let run = durable::start(&env.state, "counter", "x")
        .await
        .expect("start");

    // Drive with a token we cancel once the request is in flight.
    let token = CancellationToken::new();
    let state = env.state.clone();
    let run_for_task = run;
    let task = {
        let token = token.clone();
        tokio::spawn(async move {
            AdvanceRunJob {
                run_id: run_for_task.0,
            }
            .run_with_cancellation(state, token)
            .await
        })
    };
    wait_until(|| async { (env.fake.requests().len() == 1).then_some(()) }).await;

    let started = Instant::now();
    token.cancel();
    let result = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("attempt returned")
        .expect("join");
    let elapsed = started.elapsed();
    assert!(result.is_err(), "shutdown mid-step errors the attempt");
    assert!(
        elapsed < Duration::from_secs(3),
        "guard's shutdown arm fired mid-call, not at a loop boundary ({elapsed:?})"
    );
    assert_eq!(locked_by_of(&env.db, run).await, None, "lease released");

    // A fresh attempt with a fresh token completes the run.
    drive(&env.state, run).await.expect("resume after shutdown");
    assert_eq!(status_of(&env.db, run).await, RunStatus::Succeeded);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_through_real_worker_releases_lease_and_job_survives() {
    let env = setup_with(&[&COUNTER], Duration::from_secs(30)).await;
    env.fake.delay(Duration::from_secs(5));
    env.fake.tool_call(
        models::DEFAULT,
        "call_1",
        "add",
        &serde_json::json!({"a": 2, "b": 3}),
    );
    env.fake.chat_completion(models::DEFAULT, "5");
    let run = durable::start(&env.state, "counter", "x")
        .await
        .expect("start");

    // shutdown_drain_timeout: the cja default 2s, so the handler's bounded
    // (1s) release lands inside the drain.
    let token = CancellationToken::new();
    let worker = tokio::spawn(job_worker(
        env.state.clone(),
        Jobs,
        Duration::from_millis(10),
        20,
        token.clone(),
        JobWorkerConfig {
            heartbeat_interval: Duration::from_millis(50),
            reclaim_window: Duration::from_millis(150),
            shutdown_drain_timeout: Duration::from_secs(2),
            idle_poll_gate: None,
        },
    ));

    wait_until(|| async { (env.fake.requests().len() == 1).then_some(()) }).await;
    token.cancel();
    tokio::time::timeout(Duration::from_secs(10), async {
        let _ = worker.await;
    })
    .await
    .expect("worker exited");

    assert_eq!(
        locked_by_of(&env.db, run).await,
        None,
        "the handler returned Err inside the drain; the lease was released"
    );
    assert_eq!(
        jobs_for(&env.db, run).await,
        1,
        "the job row survives shutdown (retry scheduled, not deleted)"
    );

    // A fresh worker with a fresh token completes the run without any
    // takeover-scale wait.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = status_of(&env.db, run).await;
        if status == RunStatus::Succeeded {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "run did not complete after shutdown"
        );
        // The worker's retry backoff is 2s at error_count 1; drive the
        // attempt directly to keep the test fast, exactly like a retry does.
        let _ = drive(&env.state, run).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// 9b. Retry-budget accounting (operator review round 3, item 1)
// ---------------------------------------------------------------------------

/// Errors separated by persisted progress must never add up on the run's
/// only job row: an attempt that has advanced the run and then hit a
/// retryable error hands the run to a FRESH `AdvanceRunJob` (release +
/// enqueue in one transaction, `Ok` return), so cja deletes the errored
/// row and the fresh row starts at `error_count = 0`. Driving through a
/// real worker makes `error_count` real; the test polls it after every
/// attempt. On the pre-fix design every error charged the same row, so its
/// `error_count` reached 2 here (2^(n+1)s backoff; 20 would dead-letter
/// the run's only job).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn progress_separated_errors_requeue_on_a_fresh_job_row() {
    // Reference transcript for the identical-run assertion.
    let reference_env = setup(&[&COUNTER]).await;
    script_happy(&reference_env.fake);
    let reference_run = durable::start(&reference_env.state, "counter", "add 2 and 3")
        .await
        .expect("start reference");
    drive(&reference_env.state, reference_run)
        .await
        .expect("reference drive");
    let reference = transcript_rows(&reference_env.db, reference_run).await;

    // Each model call succeeds only after one retryable failure: 503,
    // tool-call reply, 503, final completion. The first 503 hits an
    // attempt with NO completed step (only the pre-call checkpoint), so it
    // keeps the plain Err path (error_count 1 on the original row); the
    // second 503 hits an attempt that has committed the tool turn, so it
    // must requeue on a fresh row instead of charging error_count 2.
    let env = setup(&[&COUNTER]).await;
    env.fake.status(503, r#"{"error":"upstream down"}"#);
    env.fake.tool_call(
        models::DEFAULT,
        "call_1",
        "add",
        &serde_json::json!({"a": 2, "b": 3}),
    );
    env.fake.status(503, r#"{"error":"upstream down"}"#);
    env.fake.chat_completion(models::DEFAULT, "5");
    let run = durable::start(&env.state, "counter", "add 2 and 3")
        .await
        .expect("start");

    let (worker, token) = spawn_worker(&env.state, Duration::from_millis(10)).await;
    let mut max_error_count = 0;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "run did not succeed in time");
        let counts: Vec<i32> = sqlx::query_scalar(
            "SELECT error_count FROM jobs WHERE name = 'AdvanceRunJob' AND payload->>'run_id' = $1",
        )
        .bind(run.0.to_string())
        .fetch_all(&env.db)
        .await
        .expect("read job rows");
        max_error_count = max_error_count.max(counts.iter().copied().max().unwrap_or(0));
        if status_of(&env.db, run).await == RunStatus::Succeeded {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    token.cancel();
    tokio::time::timeout(Duration::from_secs(10), async {
        let _ = worker.await;
    })
    .await
    .expect("worker exited");

    assert!(
        max_error_count <= 1,
        "no job row may carry error_count > 1 when errors are separated by \
         progress (max observed: {max_error_count})"
    );
    assert_eq!(
        transcript_rows(&env.db, run).await,
        reference,
        "the run reaches the reference transcript"
    );
    assert_eq!(
        env.fake.requests().len(),
        4,
        "exactly the four scripted requests (no retry storm)"
    );
    assert_eq!(
        jobs_for(&env.db, run).await,
        0,
        "job row deleted on success"
    );
}

/// The complement: repeated failures with ZERO progress keep the plain
/// Err path, and cja's backoff charges the SAME row — consecutive
/// no-progress failures still escalate toward dead-letter.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zero_progress_failures_share_one_backing_off_row() {
    let env = setup(&[&COUNTER]).await;
    for _ in 0..5 {
        env.fake.status(503, r#"{"error":"upstream down"}"#);
    }
    let run = durable::start(&env.state, "counter", "x")
        .await
        .expect("start");

    let (worker, token) = spawn_worker(&env.state, Duration::from_millis(10)).await;
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut first_job_id: Option<uuid::Uuid> = None;
    loop {
        assert!(
            Instant::now() < deadline,
            "error_count never escalated on the same row"
        );
        let rows: Vec<(uuid::Uuid, i32)> = sqlx::query_as(
            "SELECT job_id, error_count FROM jobs WHERE name = 'AdvanceRunJob' AND payload->>'run_id' = $1",
        )
        .bind(run.0.to_string())
        .fetch_all(&env.db)
        .await
        .expect("read job rows");
        if let Some((job_id, _)) = rows.first() {
            if first_job_id.is_none() {
                first_job_id = Some(*job_id);
            }
            assert_eq!(
                rows.len(),
                1,
                "zero-progress failures never requeue: {rows:?}"
            );
            assert_eq!(Some(*job_id), first_job_id, "the same row is retried");
            if rows[0].1 >= 3 {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    token.cancel();
    tokio::time::timeout(Duration::from_secs(10), async {
        let _ = worker.await;
    })
    .await
    .expect("worker exited");

    assert_eq!(
        status_of(&env.db, run).await,
        RunStatus::Running,
        "not failed"
    );
    assert_eq!(locked_by_of(&env.db, run).await, None, "lease released");
}

// ---------------------------------------------------------------------------
// 10. Read API
// ---------------------------------------------------------------------------

#[tokio::test]
async fn read_api_and_parsing_round_trips() {
    let env = setup(&[&COUNTER]).await;
    script_happy(&env.fake);
    let run = durable::start(&env.state, "counter", "read me")
        .await
        .expect("start");
    drive(&env.state, run).await.expect("drive");

    let view = durable::get_run(&env.db, run).await.expect("view");
    assert!(view.created_at <= chrono::Utc::now());
    assert!(view.started_at.is_some());
    assert!(view.finished_at.is_some());

    let transcript = durable::transcript(&env.db, run).await.expect("transcript");
    let seqs: Vec<i64> = transcript.iter().map(|m| m.seq).collect();
    let mut sorted = seqs.clone();
    sorted.sort_unstable();
    assert_eq!(seqs, sorted, "ordered by seq");
    assert_eq!(transcript.len(), 4);

    assert!(matches!(
        durable::get_run(&env.db, RunId(uuid::Uuid::new_v4())).await,
        Err(RunError::NotFound(_))
    ));

    // FailReason round-trips every variant.
    for reason in [
        FailReason::TurnBudget,
        FailReason::TokenBudget,
        FailReason::UnknownKind,
        FailReason::CorruptState,
        FailReason::Gateway,
        FailReason::Protocol,
        FailReason::UnstorableContent,
    ] {
        let text = reason.as_str();
        assert_eq!(text.parse::<FailReason>().expect(text), reason);
    }
    for status in [
        RunStatus::Queued,
        RunStatus::Running,
        RunStatus::Waiting,
        RunStatus::Succeeded,
        RunStatus::Failed,
        RunStatus::Cancelled,
    ] {
        let text = status.as_str();
        assert_eq!(text.parse::<RunStatus>().expect(text), status);
    }
    assert!(
        "Running".parse::<RunStatus>().is_err(),
        "wrong case rejected"
    );
    assert!("nope".parse::<RunStatus>().is_err(), "unknown rejected");

    // A manually stored unknown status or fail_reason fails the read: the
    // manual FromRow's parse failure, one decoding path. The status CHECK
    // prevents this via ordinary writes, so drop it first — the case being
    // pinned is corruption / forward-version skew (a newer cja-llm wrote a
    // status this binary doesn't know).
    sqlx::query("ALTER TABLE agent_runs DROP CONSTRAINT agent_runs_status_check")
        .execute(&env.db)
        .await
        .expect("drop status check");
    sqlx::query("UPDATE agent_runs SET status = 'QUEUED' WHERE id = $1")
        .bind(run.0)
        .execute(&env.db)
        .await
        .expect("corrupt status");
    assert!(matches!(
        durable::get_run(&env.db, run).await,
        Err(RunError::Storage(_))
    ));
    sqlx::query("UPDATE agent_runs SET status = 'succeeded', fail_reason = 'boom' WHERE id = $1")
        .bind(run.0)
        .execute(&env.db)
        .await
        .expect("corrupt fail_reason");
    assert!(matches!(
        durable::get_run(&env.db, run).await,
        Err(RunError::Storage(_))
    ));
}

// ---------------------------------------------------------------------------
// 13. Stored errors are content-free
// ---------------------------------------------------------------------------

const PROMPT_SENTINEL: &str = "SENTINEL_PROMPT_9f2a";
const REPLY_SENTINEL: &str = "SENTINEL_REPLY_7c1b";

#[tokio::test]
async fn stored_errors_are_content_free() {
    // (a) turn budget with sentinels in the prompt and replies.
    let env = setup(&[&TWO_TURNS]).await;
    for _ in 0..2 {
        env.fake.tool_call(
            models::DEFAULT,
            "c",
            "add",
            &serde_json::json!({"a": 1, "b": 1}),
        );
    }
    let run = durable::start(&env.state, "two_turns", PROMPT_SENTINEL)
        .await
        .expect("start");
    drive(&env.state, run).await.expect("job Ok");
    let view = view_of(&env.db, run).await;
    assert_eq!(view.status, RunStatus::Failed);
    assert!(
        view.error
            .as_deref()
            .unwrap_or("")
            .find(PROMPT_SENTINEL)
            .is_none()
    );
    assert!(
        view.error
            .as_deref()
            .unwrap_or("")
            .find(REPLY_SENTINEL)
            .is_none()
    );

    // The stored run_state DOES contain the prompt (it must, to resume);
    // only the error/detail surfaces must be content-free.
    let state_text: String =
        sqlx::query_scalar("SELECT run_state::text FROM agent_runs WHERE id = $1")
            .bind(run.0)
            .fetch_one(&env.db)
            .await
            .expect("run_state");
    assert!(
        state_text.contains(PROMPT_SENTINEL),
        "run_state keeps the conversation"
    );

    // (b) a 402 whose body carries a sentinel.
    let env2 = setup(&[&COUNTER]).await;
    env2.fake
        .status(402, &format!("{{\"note\":\"{REPLY_SENTINEL}\"}}"));
    let run2 = durable::start(&env2.state, "counter", PROMPT_SENTINEL)
        .await
        .expect("start");
    drive(&env2.state, run2).await.expect("job Ok");
    let view2 = view_of(&env2.db, run2).await;
    assert_eq!(view2.status, RunStatus::Failed);
    let err_text = view2.error.unwrap_or_default();
    assert!(
        !err_text.contains(REPLY_SENTINEL),
        "error column: {err_text}"
    );
    assert!(
        !err_text.contains(PROMPT_SENTINEL),
        "error column: {err_text}"
    );

    // Walk the whole RunError chain: no link carries a sentinel.
    fn assert_chain_content_free(error: &(dyn std::error::Error + 'static)) {
        let mut cur: Option<&dyn std::error::Error> = Some(error);
        while let Some(e) = cur {
            let display = e.to_string();
            let debug = format!("{e:?}");
            assert!(
                !display.contains(REPLY_SENTINEL),
                "chain Display: {display}"
            );
            assert!(
                !display.contains(PROMPT_SENTINEL),
                "chain Display: {display}"
            );
            assert!(!debug.contains(REPLY_SENTINEL), "chain Debug: {debug}");
            cur = e.source();
        }
    }
    let corrupt_error = durable::answer(&env2.state, run2, "x")
        .await
        .expect_err("not waiting");
    assert_chain_content_free(&corrupt_error);
    let _ = &corrupt_error;

    // (c) corrupted run_state: the Corrupt variant renders only the serde
    // classification + position (no source that quotes the value).
    sqlx::query(
        "UPDATE agent_runs SET status = 'waiting', pending_call_id = 'x', \
                 pending_question = 'y', run_state = '{\"format\": 999}' WHERE id = $1",
    )
    .bind(run2.0)
    .execute(&env2.db)
    .await
    .expect("corrupt run_state");
    env2.fake.chat_completion(models::DEFAULT, REPLY_SENTINEL);
    let bad = durable::answer(&env2.state, run2, "z").await;
    match bad {
        Err(err @ RunError::Corrupt(..)) => {
            let display = err.to_string();
            assert!(
                !display.contains(REPLY_SENTINEL),
                "Corrupt Display: {display}"
            );
            assert!(err.source().is_none(), "Corrupt carries no source");
        }
        other => panic!("expected Corrupt, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 14. ToolCallKey idempotency
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_call_key_idempotency() {
    SEEN_KEYS.lock().expect("key log").clear();

    // Reference: one tool call, key matches the scripted call id.
    let env = setup(&[&KEYED]).await;
    script_keyed(&env.fake);
    let run = durable::start(&env.state, "keyed", "add 2 and 3")
        .await
        .expect("start");
    drive(&env.state, run).await.expect("drive");
    let keys = SEEN_KEYS.lock().expect("key log").clone();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].run_id, run);
    assert_eq!(keys[0].call_id, "call_1");

    // Crash variant: crash after the tool executed but before its result
    // persisted (the persist right after run.answer). The reference run's
    // persist order: pre-call(1), post-response(2), post-answer(3) —
    // crashing at 3 leaves the tool executed with no result row.
    let env2 = setup_with(&[&KEYED], Duration::from_millis(1)).await;
    script_keyed(&env2.fake);
    let run2 = durable::start(&env2.state, "keyed", "add 2 and 3")
        .await
        .expect("start");
    SEEN_KEYS.lock().expect("key log").clear();
    env2.state.durable.fail_after_persist(3);
    let mut attempts = 0;
    loop {
        attempts += 1;
        assert!(attempts <= 20, "crash variant did not complete");
        tokio::time::sleep(Duration::from_millis(2)).await;
        if drive(&env2.state, run2).await.is_ok() {
            break;
        }
    }
    let keys = SEEN_KEYS.lock().expect("key log").clone();
    assert!(
        keys.len() >= 2,
        "the tool re-executed after the crash (at-least-once): {keys:?}"
    );
    for key in &keys {
        assert_eq!(key.run_id, run2);
        assert_eq!(key.call_id, "call_1", "identical key on every execution");
    }

    // Final transcript identical to the reference shape.
    let rows = transcript_rows(&env2.db, run2).await;
    let roles: Vec<_> = rows.iter().map(|(_, role, _)| role.as_str()).collect();
    assert_eq!(roles, ["user", "assistant", "user", "assistant"]);
}

// ---------------------------------------------------------------------------
// 15. Invalid tool call policy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn invalid_tool_call_policy() {
    // Default (Fail): the run fails with protocol, transcript preserved.
    let env = setup(&[&COUNTER]).await;
    env.fake.tool_call(
        models::DEFAULT,
        "c1",
        "no_such_tool",
        &serde_json::json!({"x": 1}),
    );
    let run = durable::start(&env.state, "counter", "x")
        .await
        .expect("start");
    drive(&env.state, run).await.expect("job Ok");
    let view = view_of(&env.db, run).await;
    assert_eq!(view.status, RunStatus::Failed);
    assert_eq!(view.fail_reason, Some(FailReason::Protocol));
    assert!(
        !transcript_rows(&env.db, run).await.is_empty(),
        "transcript preserved"
    );

    // Ignore: the call is dropped, the run proceeds.
    let env2 = setup(&[&IGNORE_BAD_TOOLS]).await;
    env2.fake.tool_call(
        models::DEFAULT,
        "c1",
        "no_such_tool",
        &serde_json::json!({"x": 1}),
    );
    env2.fake.chat_completion(models::DEFAULT, "recovered");
    let run2 = durable::start(&env2.state, "ignore_bad_tools", "x")
        .await
        .expect("start");
    drive(&env2.state, run2).await.expect("drive to completion");
    assert_eq!(status_of(&env2.db, run2).await, RunStatus::Succeeded);
}

// ---------------------------------------------------------------------------
// 16. Multiple asks suspend sequentially
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multiple_asks_suspend_sequentially() {
    let env = setup(&[&COUNTER]).await;
    let q1 = serde_json::json!({"question": "first question?"});
    let q2 = serde_json::json!({"question": "second question?"});
    env.fake.tool_calls(
        models::DEFAULT,
        &[("call_a", "ask", &q1), ("call_b", "ask", &q2)],
    );
    env.fake.chat_completion(models::DEFAULT, "done");

    let run = durable::start(&env.state, "counter", "two questions")
        .await
        .expect("start");
    let (worker, token) = spawn_worker(&env.state, Duration::from_millis(10)).await;

    wait_until(|| async {
        let view = view_of(&env.db, run).await;
        match view.status {
            RunStatus::Waiting => view.pending_ask,
            _ => None,
        }
    })
    .await;
    let view = view_of(&env.db, run).await;
    assert_eq!(
        view.pending_ask.as_ref().map(|a| a.call_id.as_str()),
        Some("call_a"),
        "first ask in call order suspends first"
    );
    assert_eq!(
        view.pending_ask.as_ref().map(|a| a.question.as_str()),
        Some("first question?")
    );

    durable::answer(&env.state, run, "first answer")
        .await
        .expect("answer first");
    wait_until(|| async {
        let view = view_of(&env.db, run).await;
        match view.status {
            RunStatus::Waiting => view.pending_ask,
            _ => None,
        }
    })
    .await;
    let view = view_of(&env.db, run).await;
    assert_eq!(
        view.pending_ask.as_ref().map(|a| a.call_id.as_str()),
        Some("call_b"),
        "the sibling ask re-suspends with its own columns"
    );
    assert_eq!(
        view.pending_ask.as_ref().map(|a| a.question.as_str()),
        Some("second question?")
    );

    durable::answer(&env.state, run, "second answer")
        .await
        .expect("answer second");
    wait_until(|| async {
        let status = status_of(&env.db, run).await;
        (status == RunStatus::Succeeded).then_some(status)
    })
    .await;
    token.cancel();
    tokio::time::timeout(Duration::from_secs(10), async {
        let _ = worker.await;
    })
    .await
    .expect("worker exited");

    // Tool results for BOTH call ids, in call order. rig commits a whole
    // tool batch as ONE user message with one content item per result, so
    // flatten the content items rather than counting messages.
    let rows = transcript_rows(&env.db, run).await;
    let result_call_ids: Vec<String> = rows
        .iter()
        .filter(|(_, role, _)| role == "user")
        .flat_map(|(_, _, msg)| {
            msg["content"]
                .as_array()
                .expect("user message content array")
                .iter()
                .filter(|item| item["type"] == "toolresult")
                .map(|item| serde_json::to_string(&item["call"]).expect("call id json"))
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(result_call_ids.len(), 2, "{result_call_ids:?}");
    assert!(result_call_ids[0].contains("call_a"), "{result_call_ids:?}");
    assert!(result_call_ids[1].contains("call_b"), "{result_call_ids:?}");
}

// ---------------------------------------------------------------------------
// 19. Ask with invalid arguments never suspends
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ask_with_invalid_args_fails_and_continues() {
    let env = setup(&[&COUNTER]).await;
    env.fake
        .tool_call(models::DEFAULT, "call_bad", "ask", &serde_json::json!({}));
    env.fake.chat_completion(models::DEFAULT, "recovered");

    let run = durable::start(&env.state, "counter", "bad ask")
        .await
        .expect("start");
    drive(&env.state, run).await.expect("drive");

    assert_eq!(status_of(&env.db, run).await, RunStatus::Succeeded);
    let rows = transcript_rows(&env.db, run).await;
    let result_row = rows
        .iter()
        .find(|(_, role, msg)| {
            *role == "user"
                && serde_json::to_string(msg)
                    .unwrap_or_default()
                    .contains("call_bad")
        })
        .expect("corrective tool result for call_bad");
    let text = serde_json::to_string(&result_row.2).unwrap_or_default();
    assert!(
        text.contains("ask requires a string `question`"),
        "corrective failure text: {text}"
    );
    let result_pos = rows.iter().position(|(_, _, msg)| {
        serde_json::to_string(msg)
            .unwrap_or_default()
            .contains("call_bad")
    });
    let last_role = rows.last().map(|(_, role, _)| role.as_str());
    assert_eq!(
        last_role,
        Some("assistant"),
        "model recovered after the failure"
    );
    assert!(result_pos.is_some());
}

// ---------------------------------------------------------------------------
// 20. Enqueue wakes once, after commit
// ---------------------------------------------------------------------------

/// Run `op` (a `start`/`answer` call) with the parking hook installed, on a
/// dedicated OS thread with its own single-threaded runtime and a lazy
/// second pool into the same test DB. The hook signals arrival post-commit
/// and parks that thread; while it stays parked, `mid_park` runs on the
/// MAIN runtime and asserts what a fresh worker would see through its own
/// connection. A dedicated thread is required: blocking a worker of the
/// shared runtime stalls its I/O driver — the visibility checks (even a
/// `sleep`) would deadlock — while blocking the dedicated thread parks only
/// that thread's own runtime, which has nothing else to do.
async fn parked_op<O, F, Fut, M>(
    template: &TestState,
    park: Arc<(Mutex<bool>, Condvar)>,
    op: F,
    mid_park: M,
) -> O
where
    O: Send + 'static,
    F: FnOnce(TestState) -> Fut + Send + 'static,
    Fut: Future<Output = O> + Send + 'static,
    M: Future<Output = ()>,
{
    // The op's own pool: connections are established lazily on the dedicated
    // runtime, so they bind to it (sharing the main pool across runtimes
    // would leave socket readiness registered with the wrong reactor).
    let state = TestState {
        db: sqlx::postgres::PgPoolOptions::new()
            .connect_lazy_with((*template.db.connect_options().clone()).clone()),
        cookie_key: template.cookie_key.clone(),
        durable: template.durable.clone(),
        wakes: template.wakes.clone(),
        park: Some(park.clone()),
    };
    *park.0.lock().expect("park lock") = false;
    let thread = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("dedicated runtime");
        rt.block_on(op(state))
    });
    // Wait for the hook's arrival signal without blocking anything.
    loop {
        if *park.0.lock().expect("park lock") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    // Parked, and — if the wake-after-commit contract holds — committed:
    // this is the moment a worker would be woken into. Had `job_enqueued`
    // fired inside the transaction, the op would be parked holding it open
    // and neither row would be visible from this fresh connection.
    mid_park.await;
    {
        let mut guard = park.0.lock().expect("park lock");
        *guard = false;
        park.1.notify_all();
    }
    thread.join().expect("op thread join")
}

async fn count_rows(db: &sqlx::PgPool, what: &str) -> i64 {
    let sql = match what {
        "runs" => "SELECT count(*) FROM agent_runs",
        "jobs" => "SELECT count(*) FROM jobs WHERE name = 'AdvanceRunJob'",
        other => panic!("unknown count target {other}"),
    };
    sqlx::query_scalar(sql).fetch_one(db).await.expect("count")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enqueue_wakes_once_after_commit() {
    let env = setup(&[&COUNTER]).await;
    let park = Arc::new((Mutex::new(false), Condvar::new()));

    // start: while its wake hook is parked, the run row AND its
    // AdvanceRunJob row must already be committed and visible. This run is
    // never driven, so it needs no scripted replies (start never calls the
    // model).
    let db = env.db.clone();
    let run = parked_op(
        &env.state,
        park.clone(),
        |state| async move { durable::start(&state, "counter", "x").await },
        async {
            assert_eq!(
                count_rows(&db, "runs").await,
                1,
                "run row committed before wake"
            );
            assert_eq!(
                count_rows(&db, "jobs").await,
                1,
                "job row committed before wake"
            );
        },
    )
    .await
    .expect("start");
    let _ = run;
    assert_eq!(
        env.state.wakes.load(Ordering::SeqCst),
        1,
        "exactly one wake"
    );

    // answer: suspend a second run, then park through answer's wake and
    // check its enqueued job is committed before the wake too.
    env.state.wakes.store(0, Ordering::SeqCst);
    env.fake.tool_call(
        models::DEFAULT,
        "call_a",
        "ask",
        &serde_json::json!({"question": "q?"}),
    );
    let db = env.db.clone();
    let suspended = parked_op(
        &env.state,
        park.clone(),
        |state| async move { durable::start(&state, "counter", "y").await },
        async {
            assert_eq!(count_rows(&db, "runs").await, 2);
            assert_eq!(count_rows(&db, "jobs").await, 2);
        },
    )
    .await
    .expect("start suspended run");
    drive(&env.state, suspended).await.expect("suspend");
    assert_eq!(status_of(&env.db, suspended).await, RunStatus::Waiting);

    env.state.wakes.store(0, Ordering::SeqCst);
    let db = env.db.clone();
    let suspended_answer = suspended;
    parked_op(
        &env.state,
        park.clone(),
        move |state| async move { durable::answer(&state, suspended_answer, "yes").await },
        async {
            assert_eq!(count_rows(&db, "runs").await, 2);
            assert_eq!(
                count_rows(&db, "jobs").await,
                3,
                "answer's fresh AdvanceRunJob committed before its wake"
            );
        },
    )
    .await
    .expect("answer");
    assert_eq!(env.state.wakes.load(Ordering::SeqCst), 1);

    // Negative cases with a plain (non-parking) state.
    let plain_env = setup(&[&COUNTER]).await;
    assert!(matches!(
        durable::start(&plain_env.state, "no_such_kind", "x").await,
        Err(RunError::UnknownKind(_))
    ));
    let run = durable::start(&plain_env.state, "counter", "x")
        .await
        .expect("start");
    assert!(matches!(
        durable::answer(&plain_env.state, run, "no").await,
        Err(RunError::NotWaiting(..))
    ));
    assert_eq!(
        plain_env.state.wakes.load(Ordering::SeqCst),
        1,
        "only the successful start woke the worker"
    );
}

// ---------------------------------------------------------------------------
// 11. Spans carry metadata, never content
// ---------------------------------------------------------------------------

mod span_capture {
    //! One global capture for this test binary. The subscriber deliberately
    //! CAPS rig's raw-wire TRACE targets (`rig::completions`, `rig::streaming`)
    //! at DEBUG and `rig_agent::agent::typed` at ERROR — the same stack cja's
    //! production setup ships — because those targets unconditionally dump
    //! whole request/reply JSON. Asserting only after capping would be vacuous
    //! in the other direction with an unfiltered TRACE capture: rig's wire
    //! dumps (not our spans) would trip the sentinel check. The capped stack
    //! is the honest production-equivalent surface.
    use std::io;
    use std::sync::Arc;
    use std::sync::Mutex;

    use tracing_subscriber::fmt::MakeWriter;
    use tracing_subscriber::fmt::format::FmtSpan;
    use tracing_subscriber::layer::SubscriberExt as _;

    #[derive(Clone, Default)]
    pub struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    pub struct SharedWrite(Arc<Mutex<Vec<u8>>>);

    impl SharedWriter {
        pub fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().expect("capture buffer")).into_owned()
        }
    }

    impl<'a> MakeWriter<'a> for SharedWriter {
        type Writer = SharedWrite;

        fn make_writer(&'a self) -> Self::Writer {
            SharedWrite(self.0.clone())
        }
    }

    impl io::Write for SharedWrite {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .expect("capture buffer")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    static CAPTURE: std::sync::OnceLock<SharedWriter> = std::sync::OnceLock::new();

    pub fn capture() -> SharedWriter {
        let writer = CAPTURE.get_or_init(SharedWriter::default).clone();
        static INSTALL: std::sync::Once = std::sync::Once::new();
        INSTALL.call_once(|| {
            let filter = tracing_subscriber::filter::Targets::new()
                .with_default(tracing::Level::TRACE)
                // rig unconditionally dumps raw request/reply JSON on these
                // targets at TRACE (`providers::internal::trace_json`):
                // wire debugging, not GenAI telemetry, and not something a
                // library can gate from inside.
                .with_target("rig::completions", tracing::Level::DEBUG)
                .with_target("rig::streaming", tracing::Level::DEBUG)
                // The one WARN that prints raw model output.
                .with_target("rig_agent::agent::typed", tracing::Level::ERROR);
            let subscriber = tracing_subscriber::fmt()
                .with_writer(writer.clone())
                .with_span_events(FmtSpan::FULL)
                .with_ansi(false)
                .finish()
                .with(filter);
            tracing::subscriber::set_global_default(subscriber)
                .expect("no other global tracing subscriber in this binary");
        });
        writer
    }
}

#[tokio::test]
async fn spans_carry_metadata_never_content() {
    static TRACED: AgentKind = AgentKind {
        name: "traced",
        model: models::DEFAULT,
        limits: RunLimits {
            max_turns: 20,
            max_total_tokens: 200_000,
        },
        unhandled_invalid_tool_call: UnhandledInvalidToolCall::Fail,
        build: |llm, model| llm.agent(model).tool(Ask).tool(Add),
    };

    let writer = span_capture::capture();
    let env = setup(&[&TRACED]).await;

    const SPAN_PROMPT_SENTINEL: &str = "SPAN_SENTINEL_PROMPT_5f31";
    const SPAN_TOOL_SENTINEL: &str = "SPAN_SENTINEL_TOOL_9a77";
    const SPAN_REPLY_SENTINEL: &str = "SPAN_SENTINEL_REPLY_2c4e";

    // The sentinel rides in the tool arguments as an ignored extra field and
    // in the final reply text: any surface that echoes model I/O carries it.
    env.fake.tool_call(
        models::DEFAULT,
        "call_s",
        "add",
        &serde_json::json!({"a": 1, "b": 2, "note": SPAN_TOOL_SENTINEL}),
    );
    env.fake
        .chat_completion(models::DEFAULT, SPAN_REPLY_SENTINEL);

    let run = durable::start(&env.state, "traced", SPAN_PROMPT_SENTINEL)
        .await
        .expect("start");
    drive(&env.state, run).await.expect("drive");
    assert_eq!(status_of(&env.db, run).await, RunStatus::Succeeded);

    let text = writer.text();
    // Positive: the step span exists with its metadata fields, on one line.
    let step_lines: Vec<&str> = text
        .lines()
        .filter(|line| line.contains("agent_run.step") && line.contains("run.kind=traced"))
        .collect();
    assert!(!step_lines.is_empty(), "agent_run.step span lines captured");
    assert!(
        step_lines.iter().any(|l| l.contains("run.id=")
            && l.contains("run.turn=")
            && l.contains("run.input_tokens=")),
        "span carries run id, turn, and token fields: {}",
        step_lines.first().copied().unwrap_or_default()
    );
    // Negative: no content sentinel anywhere in the capture.
    for sentinel in [
        SPAN_PROMPT_SENTINEL,
        SPAN_TOOL_SENTINEL,
        SPAN_REPLY_SENTINEL,
    ] {
        assert!(
            !text.contains(sentinel),
            "content leaked into telemetry: {sentinel}"
        );
    }
}

// ---------------------------------------------------------------------------
// PR review pins (round 1)
// ---------------------------------------------------------------------------

/// `answer()` commits the ask's tool-result message into the run (a
/// single-ask batch completes on answer) and must project it into
/// `agent_messages` in the same transaction (plan step 7: "project any newly
/// committed messages"). If the projection uses the wrong `seq`, the row is
/// silently dropped by `ON CONFLICT DO NOTHING`; a `cancel()` before the
/// worker's next persist then loses the human's answer from the transcript
/// permanently.
#[tokio::test]
async fn answer_projects_tool_result_into_transcript() {
    let env = setup(&[&COUNTER]).await;
    env.fake.tool_call(
        models::DEFAULT,
        "call_ask",
        "ask",
        &serde_json::json!({"question": "continue?"}),
    );
    let run = durable::start(&env.state, "counter", "go")
        .await
        .expect("start");
    drive(&env.state, run).await.expect("drive to suspension");
    assert_eq!(status_of(&env.db, run).await, RunStatus::Waiting);
    let before = transcript_rows(&env.db, run).await;
    assert_eq!(before.len(), 2, "user prompt + assistant ask call");

    durable::answer(&env.state, run, "yes")
        .await
        .expect("answer");

    let after = transcript_rows(&env.db, run).await;
    assert_eq!(
        after.len(),
        3,
        "answer() must project the committed tool-result message: {after:?}"
    );
    assert_eq!(after[2].0, 2, "tool result lands at the next seq");
    assert_eq!(after[2].1, "user");
    assert!(
        serde_json::to_string(&after[2].2)
            .expect("json")
            .contains("call_ask"),
        "the projected message is the ask's tool result"
    );

    // Cancelling before any worker persists must keep the answer.
    durable::cancel(&env.state, run).await.expect("cancel");
    assert_eq!(status_of(&env.db, run).await, RunStatus::Cancelled);
    assert_eq!(
        transcript_rows(&env.db, run).await.len(),
        3,
        "a cancelled run's transcript keeps the human's answer"
    );
}

/// Plan Decision 4: every way an attempt can end leaves the run finished or
/// re-claimable by the next cja job attempt. A transient DB error on the
/// heartbeat is not proof of a lost lease; returning `Ok` there makes cja
/// delete the only job row while the run is still `running`, orphaning it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn heartbeat_db_error_does_not_complete_the_job() {
    let env = setup_with(&[&COUNTER], Duration::from_millis(300)).await;
    env.fake.delay(Duration::from_secs(3));
    env.fake.chat_completion(models::DEFAULT, "5");
    let run = durable::start(&env.state, "counter", "x")
        .await
        .expect("start");

    let state = env.state.clone();
    let task = tokio::spawn(async move {
        AdvanceRunJob { run_id: run.0 }
            .run_with_cancellation(state, CancellationToken::new())
            .await
    });
    wait_until(|| async { (env.fake.requests().len() == 1).then_some(()) }).await;

    // Make the next heartbeat UPDATE fail with a DB error (not "zero rows").
    sqlx::query("ALTER TABLE agent_runs RENAME TO agent_runs_hidden")
        .execute(&env.db)
        .await
        .expect("hide table");
    let result = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("attempt returned")
        .expect("join");
    sqlx::query("ALTER TABLE agent_runs_hidden RENAME TO agent_runs")
        .execute(&env.db)
        .await
        .expect("restore table");

    let status = status_of(&env.db, run).await;
    assert!(
        result.is_err() || status != RunStatus::Running,
        "attempt returned Ok (cja deletes the job) while the run is still \
         `running` under our lease: orphaned run (status {status:?}, locked_by {:?})",
        locked_by_of(&env.db, run).await
    );
}

/// Round 3 pin: a heartbeat that TIMES OUT — here a real Postgres row
/// lock (`FOR UPDATE` from a second connection) pinning the heartbeat
/// UPDATE past the 5s lease-write bound — has not proven the lease lost.
/// Returning `Ok` there would make cja delete the only job row while the
/// run is still `running` under this worker's lease: the same orphan the
/// DB-error pin above prevents. The timeout must error the attempt
/// (`fail_retryable`: bounded release + cja backoff); only a provably
/// lost lease (zero rows) may complete the job.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn heartbeat_timeout_does_not_complete_the_job() {
    // Reference: the uninterrupted transcript this run must reach after
    // recovery (plain completion, no delay).
    let reference_env = setup(&[&COUNTER]).await;
    reference_env.fake.chat_completion(models::DEFAULT, "5");
    let reference_run = durable::start(&reference_env.state, "counter", "x")
        .await
        .expect("start reference");
    drive(&reference_env.state, reference_run)
        .await
        .expect("reference drive");
    let reference = transcript_rows(&reference_env.db, reference_run).await;

    // 3s takeover -> 1s heartbeat period. The delayed reply keeps the
    // model call in flight across several heartbeat ticks; the second
    // completion is for the retry (the first reply is consumed by the
    // interrupted call and its result dropped with it).
    let env = setup_with(&[&COUNTER], Duration::from_secs(3)).await;
    env.fake.delay(Duration::from_secs(3));
    env.fake.chat_completion(models::DEFAULT, "5");
    env.fake.chat_completion(models::DEFAULT, "5");
    let run = durable::start(&env.state, "counter", "x")
        .await
        .expect("start");

    let state = env.state.clone();
    let task = tokio::spawn(async move {
        AdvanceRunJob { run_id: run.0 }
            .run_with_cancellation(state, CancellationToken::new())
            .await
    });
    wait_until(|| async { (env.fake.requests().len() == 1).then_some(()) }).await;

    // Hold the run row FOR UPDATE from this (second) connection, past the
    // 5s WRITE_BOUND plus margin: the next heartbeat UPDATE blocks on the
    // row lock and times out without proving anything about ownership.
    let mut holder = env.db.begin().await.expect("begin lock holder");
    sqlx::query("SELECT id FROM agent_runs WHERE id = $1 FOR UPDATE")
        .bind(run.0)
        .fetch_one(&mut *holder)
        .await
        .expect("lock run row");
    tokio::time::sleep(Duration::from_secs(7)).await;
    holder.rollback().await.expect("release row lock");

    let result = tokio::time::timeout(Duration::from_secs(20), task)
        .await
        .expect("attempt returned")
        .expect("join");

    // The attempt errored, so cja keeps the job row and retries it. The run
    // is NOT terminal, and nothing was written after the timeout: the only
    // committed projection is still the pre-call user message, and no
    // finish/persist landed. (The bounded release could not commit under
    // the row lock; the lease is either still held-and-stale or cleared —
    // both re-claimable.)
    assert!(
        result.is_err(),
        "heartbeat timeout must error the attempt, not complete the job"
    );
    assert_eq!(jobs_for(&env.db, run).await, 1, "job row not deleted");
    assert_eq!(status_of(&env.db, run).await, RunStatus::Running);
    let rows = transcript_rows(&env.db, run).await;
    assert_eq!(rows.len(), 1, "no writes after the timeout: {rows:?}");
    assert_eq!(rows[0].1, "user", "only the pre-call user message");

    // With the lock released, a later attempt re-claims (cleared lease or
    // stale-lease takeover — both are past the 3s window) and drives the
    // run to `succeeded` with the reference transcript.
    drive(&env.state, run)
        .await
        .expect("retry re-claims and completes");
    assert_eq!(status_of(&env.db, run).await, RunStatus::Succeeded);
    assert_eq!(
        transcript_rows(&env.db, run).await,
        reference,
        "recovered transcript equals the uninterrupted run's"
    );
}

/// A concurrent queued -> running transition must not turn a valid run into
/// `NotFound`. The row remains present and cancellation is valid in both
/// states; `NotFound` is reserved for a missing run id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_racing_claim_never_reports_existing_run_as_not_found() {
    let env = setup(&[&COUNTER]).await;
    let run = durable::start(&env.state, "counter", "go")
        .await
        .expect("start");

    let mut tx = env.db.begin().await.expect("begin claim transaction");
    sqlx::query("SELECT id FROM agent_runs WHERE id = $1 FOR UPDATE")
        .bind(run.0)
        .fetch_one(&mut *tx)
        .await
        .expect("lock run row");

    let state = env.state.clone();
    let cancel_task = tokio::spawn(async move { durable::cancel(&state, run).await });
    wait_until(|| async {
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM pg_stat_activity \
             WHERE datname = current_database() AND wait_event_type = 'Lock' \
             AND query LIKE '%WITH target AS%')",
        )
        .fetch_one(&env.db)
        .await
        .expect("inspect blocked cancellation")
        .then_some(())
    })
    .await;

    sqlx::query(
        "UPDATE agent_runs SET status = 'running', locked_by = 'other', locked_at = NOW() \
         WHERE id = $1",
    )
    .bind(run.0)
    .execute(&mut *tx)
    .await
    .expect("claim run");
    tx.commit().await.expect("commit claim");

    let result = cancel_task.await.expect("join cancellation");
    assert!(result.is_ok(), "cancel() lost a valid run: {result:?}");
    assert_eq!(status_of(&env.db, run).await, RunStatus::Running);
    let requested: bool =
        sqlx::query_scalar("SELECT cancel_requested FROM agent_runs WHERE id = $1")
            .bind(run.0)
            .fetch_one(&env.db)
            .await
            .expect("existing run");
    assert!(requested, "the live worker must receive the cancellation");
}
