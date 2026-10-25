//! Durable agent-run integration tests.
//!
//! Each test gets an isolated database (cja migrations + cja-llm durable
//! migrations) and drives runs against `FakeGateway` — no network. The
//! manual two-migrator setup is deliberate: a bare `#[sqlx::test]` attribute
//! applies only one migrations directory and would miss cja's `jobs` table
//! (the cja-passkey precedent).

#![allow(clippy::too_many_lines)]

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
use sqlx::Row;

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
    SEEN_KEYS
        .lock()
        .expect("key log lock")
        .push(key.clone());
    Ok(a + b)
}

static COUNTER: AgentKind = AgentKind {
    name: "counter",
    model: models::DEFAULT,
    limits: RunLimits { max_turns: 20, max_total_tokens: 200_000 },
    unhandled_invalid_tool_call: UnhandledInvalidToolCall::Fail,
    build: |llm, model| llm.agent(model).tool(Ask).tool(Add),
};

static KEYED: AgentKind = AgentKind {
    name: "keyed",
    model: models::DEFAULT,
    limits: RunLimits { max_turns: 20, max_total_tokens: 200_000 },
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
    limits: RunLimits { max_turns: 20, max_total_tokens: 200_000 },
    unhandled_invalid_tool_call: UnhandledInvalidToolCall::Ignore,
    build: |llm, model| llm.agent(model).tool(Ask).tool(Add),
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

    fn version(&self) -> &str {
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
                arrived = park
                    .1
                    .wait(arrived)
                    .expect("parking interrupted");
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

async fn setup_with(
    kinds: &[&'static AgentKind],
    takeover: Duration,
) -> Env {
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
    durable::run_migrations(&pool).await.expect("durable migrations");

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

    let mut durable = Durable::new(&config).expect("durable engine").lease_takeover(takeover);
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
) -> (
    tokio::task::JoinHandle<cja::Result<()>>,
    CancellationToken,
) {
    let token = CancellationToken::new();
    let handle = tokio::spawn(job_worker(
        state.clone(),
        Jobs,
        sleep,
        20,
        token.clone(),
        JobWorkerConfig {
            heartbeat_interval: Duration::from_millis(50),
            reclaim_window: Duration::from_millis(150),
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
    durable::get_run(db, run)
        .await
        .expect("run row")
        .status
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
    fake.tool_call(models::DEFAULT, "call_1", "add", &serde_json::json!({"a": 2, "b": 3}));
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
    // Two model turns x (persist-before-call + persist-after-response) +
    // persist-after-tool-result. K counts persist calls, not messages: some
    // persists project zero new rows by design (rig commits a tool batch's
    // results only when the batch completes).
    assert!(k >= 5, "expected at least 5 persists, got {k}");

    for n in 1..=k {
        let env = setup_with(&[&COUNTER], Duration::from_millis(1)).await;
        script_happy(&env.fake);
        env.state.durable.fail_after_persist(n);
        let run = durable::start(&env.state, "counter", "add 2 and 3")
            .await
            .expect("start");

        let mut attempts = 0;
        loop {
            attempts += 1;
            assert!(attempts <= 20, "run did not complete within 20 attempts (crash at persist {n})");
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
            role == "assistant" && serde_json::to_string(msg).unwrap_or_default().contains("call_ask")
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
    assert!(answer_text.contains("yes"), "answer text in result: {answer_text}");
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
    drive(&env.state, suspended).await.expect("winner completes");

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
    durable::cancel(&env.state, queued).await.expect("cancel queued");
    assert_eq!(status_of(&env.db, queued).await, RunStatus::Cancelled);
    drive(&env.state, queued).await.expect("job no-ops Ok");
    assert_eq!(status_of(&env.db, queued).await, RunStatus::Cancelled, "run untouched");

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
    durable::cancel(&env.state, waiting).await.expect("cancel waiting");
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
    let db = env.db.clone();
    let live_run = live;
    let driver = tokio::spawn(async move { drive(&state, live_run).await });
    // One request came from the `waiting` part; the live run's model call is
    // the second.
    wait_until(|| async {
        (env.fake.requests().len() == 2).then_some(())
    })
    .await;
    durable::cancel(&env.state, live).await.expect("request cancel");
    let result = tokio::time::timeout(Duration::from_secs(15), driver)
        .await
        .expect("driver returned")
        .expect("join")
        .expect("job completes Ok after cancel");
    let _ = result;
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
    durable::cancel(&env.state, orphan).await.expect("cancel orphan");
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
    drive(&env.state, failed_env_run).await.expect("fail on budget");
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
    assert_eq!(status_of(&env.db, run).await, RunStatus::Running, "not failed");
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
    assert_eq!(env.fake.requests().len(), 1, "exactly one transport attempt");
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
            AdvanceRunJob { run_id: run_for_task.0 }
                .run_with_cancellation(state, token)
                .await
        })
    };
    wait_until(|| async {
        (env.fake.requests().len() == 1).then_some(())
    })
    .await;

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

    wait_until(|| async {
        (env.fake.requests().len() == 1).then_some(())
    })
    .await;
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
        assert!(Instant::now() < deadline, "run did not complete after shutdown");
        // The worker's retry backoff is 2s at error_count 1; drive the
        // attempt directly to keep the test fast, exactly like a retry does.
        let _ = drive(&env.state, run).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
