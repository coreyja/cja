# cja-llm

Opinionated [rig](https://github.com/0xPlaygrounds/rig) setup for cja apps,
keyless via the tailnet LLM gateway. The gateway identifies callers by their
tailnet node identity and holds the real provider keys, so an app needs
nothing to configure — no API key, no secret, no env var.

```rust
use cja_llm::models;

#[tokio::main]
async fn main() -> cja_llm::Result<()> {
    let llm = cja_llm::Llm::from_env()?;
    let reply = llm.agent(models::DEFAULT).build().prompt("Pick a color.").await?;
    println!("{}", reply.output());
    Ok(())
}
```

Everything goes through one shared transport: `https://llm.rooster-mermaid.ts.net`
(`CJA_LLM_URL` overrides it — origin only, non-secret), retries, timeouts,
and tracing included. The rig crates are re-exported as `cja_llm::rig`, so
apps never name `rig-core` directly or drift to a different rig version.

## The no-agent case: `complete`, `extract`, `stream`

```rust,no_run
use cja_llm::{Llm, models};
use serde::Deserialize;

#[derive(Deserialize, schemars::JsonSchema)]
struct Answer { answer: u32 }

# async fn example(llm: &Llm) -> cja_llm::Result<()> {
// Text in, text out.
let text = llm.complete(models::DEFAULT, "a color?").await?;

// Structured output: any T: DeserializeOwned + JsonSchema.
let answer: Answer = llm.extract(models::DEFAULT, "the answer to everything?").await?;

// Text deltas; lazy, and dropping the stream cancels the request.
use futures::StreamExt as _;
let mut stream = llm.stream(models::DEFAULT, "a color?");
while let Some(delta) = stream.next().await {
    println!("{}", delta?);
}
# Ok(())
# }
```

A parse failure returns `Error::SchemaParse` carrying the model's raw text.

Models are Bifrost `provider/model` strings, e.g. `anthropic/claude-haiku-5-5`
(see `cja_llm::models` for the named constants). A bare model name falls back
to the gateway's catalog resolver and can fail — always send `provider/model`.

## Tools ()

Tools come from rig's derive macro. rig's macro resolves its generated paths
by reading **the expanding crate's own `Cargo.toml`**, so an app that depends
only on cja-llm does not compile — add exactly one dependency line, copied
from the version table below:

```toml
rig-agent = { version = "=0.45.0", default-features = false, features = ["derive"] }
```

| cja-llm | rig-core | rig-agent |
| --- | --- | --- |
| 0.0.0 | `=0.45.0` | `=0.45.0` |

Then import the macro from cja-llm's re-export and register the generated
unit struct:

```rust,no_run
use cja_llm::rig::rig_agent::rig_tool;
use cja_llm::{Llm, models};

#[rig_tool(description = "Add two integers")]
async fn add(a: i32, b: i32) -> Result<i32, cja_llm::rig::rig_core::tool::ToolExecutionError> {
    Ok(a + b)
}

# async fn example(llm: &Llm) -> cja_llm::Result<()> {
let reply = llm
    .agent(models::DEFAULT)
    .tool(Add)
    .build()
    .prompt("What is 2 + 3?")
    .await?;
println!("{}", reply.output());
# Ok(())
# }
```

A pin drift fails compilation at `.tool(...)` registration (trait mismatch) —
that is the signal to re-copy the table. Host state in a tool rides the run's
`ToolContext` (a `#[rig(context)] &mut ToolContext` parameter plus
`AgentRunner::tool_context`), since the macro generates unit structs. The
escape hatch, needing no extra dependency, is a hand-written
`impl cja_llm::rig::rig_core::tool::Tool`.

## Streaming

`llm.stream(model, prompt)` yields text deltas (see above). House settings:
the total request deadline covers everything through the response head;
after that each chunk has its own idle timeout (`LlmConfig::stream_idle_timeout`,
default 60s). A streamed request retries only before its first response byte.

## Testing: the `testing` feature

`cja_llm::testing::FakeGateway` is an in-process scripted OpenAI
chat-completions server on loopback. Requests go through the real transport,
so header stripping, retries, timeouts, and SSE parsing behave exactly as in
production — with no network. Script replies FIFO (text, JSON, SSE deltas,
tool calls, error statuses, delayed replies, dropped connections), and read
back every request the fake saw:

```rust,no_run
use std::time::Duration;
use cja_llm::config::RetryPolicy;
use cja_llm::{Llm, LlmConfig, models};

# async fn example() -> cja_llm::Result<()> {
let fake = cja_llm::testing::FakeGateway::start().await;
fake.chat_completion(models::DEFAULT, "pong");

let url = fake.url();
let config = LlmConfig::new(url)
    .app_name("my-app")
    .connect_timeout(Duration::from_millis(250))
    .request_timeout(Duration::from_millis(250))
    .retries(RetryPolicy { max_attempts: 3, initial_backoff: Duration::from_millis(10), ..RetryPolicy::default() });
let llm = Llm::new(&config)?;

assert_eq!(llm.complete(models::DEFAULT, "ping").await?, "pong");
assert_eq!(fake.requests().len(), 1);
# Ok(())
# }
```

## Durable agent runs (the `durable` feature)

A durable agent run is a **cja background job, never in the request path**.
Its whole conversation is persisted in Postgres (`agent_runs` /
`agent_messages`), so a run survives deploys, crashes, and job retries,
pauses for human input, and resumes exactly where it left off.

### Enabling

```toml
cja-llm = { path = "../crates/cja-llm", features = ["durable"] }
# plus, only if the app writes #[rig_tool] tools (see the tools section):
# rig-agent = { version = "=0.45.0", default-features = false, features = ["derive"] }
```

At boot: build the engine, store it in your `AppState`, run the migrations
**after** `cja::db::run_migrations`, and register `AdvanceRunJob` — the
plain struct, no type parameter:

```rust,no_run
use cja_llm::durable::{AdvanceRunJob, AgentKind, Ask, Durable, RunLimits};
use cja_llm::{LlmConfig, models};

# fn main() -> cja_llm::Result<()> {
static SUMMARIZE: AgentKind = AgentKind {
    name: "summarize",
    model: models::DEFAULT,
    limits: RunLimits::default(),
    unhandled_invalid_tool_call: rig_agent::run::UnhandledInvalidToolCall::Fail,
    build: |llm, model| {
        llm.agent(model)
            .preamble("You summarize things.")
            .tool(Ask)
            // .tool(MyTool) ...
    },
};

let durable = Durable::new(&LlmConfig::from_env()?)?.kind(&SUMMARIZE)?;
// then, after building the pool:
// cja_llm::durable::run_migrations(&pool).await?;
// and in the job registry: impl_job_registry!(AppState, ..., AdvanceRunJob);
# Ok(())
# }
```

### The full loop

```rust,no_run
use cja_llm::durable::{self, AdvanceRunJob, AgentKind, Ask, Durable, RunLimits};
# #[derive(Clone)] struct AppState { /* db, durable, ... */ }
# impl cja::app_state::AppState for AppState { fn db(&self) -> &sqlx::PgPool { todo!() } fn version(&self) -> &str { "" } fn cookie_key(&self) -> &cja::server::cookies::CookieKey { todo!() } }
# impl durable::HasDurable for AppState { fn durable(&self) -> &Durable { todo!() } }
# async fn example(state: &AppState, db: &sqlx::PgPool) -> Result<(), cja_llm::durable::RunError> {
# static COUNTER: AgentKind = AgentKind { name: "counter", model: cja_llm::models::DEFAULT, limits: RunLimits::default(), unhandled_invalid_tool_call: rig_agent::run::UnhandledInvalidToolCall::Fail, build: |_llm, _model| todo!() };
// Define the kind once (see above); the model called a tool that asked the
// human a question, so the run is suspended:
let run = durable::start(state, "counter", "count to five").await?;

// ... a job worker (or your test) drives it; poll or react to your own UI:
let view = durable::get_run(db, run).await?;             // status: `Waiting`
let pending = view.pending_ask.unwrap();                  // the question

durable::answer(state, run, "yes, go ahead").await?;      // resumes as a tool result

let view = durable::get_run(db, run).await?;              // status: `Succeeded`
let transcript = durable::transcript(db, run).await?;     // ordered rig Messages
# let _ = (&pending, &transcript, &COUNTER, &AdvanceRunJob.NAME);
# Ok(())
# }
```

### Kinds are static

Preamble, static tools (including `Ask`), and static context documents
only. `dynamic_tools`, `dynamic_context` (RAG), and typed output are
unsupported on the durable path: rig runs them in its runner's private
request builder, which durable runs cannot drive. `start` rejects a kind
whose built agent carries an `output_schema`. Want structured output?
Parse inside a tool, or use a separate non-durable `Llm::extract` call.

### Tool idempotency

Tool execution is **at-least-once**: a tool whose result wasn't persisted
re-executes on resume (as does a model call after a crash — re-issues
consume no turn but can double-bill; that is documented rig behavior). For
exactly-once behavior, every tool invocation receives its idempotency key
in its context:

```rust
fn tool_body(ctx: &mut rig_core::tool::ToolContext) -> Result<(), rig_core::tool::ToolExecutionError> {
    let key: cja_llm::durable::ToolCallKey = ctx
        .require()
        .map_err(|e| rig_core::tool::ToolExecutionError::other(e.to_string()))?;
    // key.run_id + key.call_id — a *receipt*, not deduplication: persist
    // your own attempt record keyed by it before any side effect.
    Ok(())
}
```

### Asking semantics

`answer(run_id, text)` resolves **exactly the ask recorded in
`pending_call_id`/`pending_question`** — the provider's tool-call id of the
suspended `ask`, answered as that call's tool result (a plain user message
cannot close an open tool call). A batch containing several asks suspends
sequentially, one question at a time, in call order. An `ask` whose
arguments don't include a string `question` never suspends: it is answered
with a corrective failure and the run continues. Answering a run that is
not `waiting` returns a typed error.

### Lease lifecycle and concurrency

At most one worker advances a run at a time. Each attempt claims the run
(`queued`, or `running` with a stale lease) with a fresh identity and
heartbeats every `lease_takeover / 3` (default 90s window, so 30s
heartbeats) while a model call or tool runs. `lease_takeover` **must stay
below cja's `reclaim_window`** (default 120s), or a legitimately retried
job cannot re-claim its run after an ungraceful death. Every retryable
error releases the lease before returning, so cja's backoff re-claims
immediately. A duplicate job that finds the row leased by a live owner
errors (backoff) instead of completing — it can never delete the job the
live owner depends on. A worker that loses its lease mid-run stops
silently; the current owner continues.

### Cancel vs shutdown

- **User `cancel(run_id)`** marks a queued/waiting run (or a `running` run
  under a dead lease — the orphaned-run escape hatch) `cancelled` directly;
  a live `running` run is flagged and its owner marks it `cancelled` at the
  next step boundary. The transcript is preserved in every case. Cancelling
  an already-`cancelled` run is `Ok`; other terminal statuses are rejected.
- **Deploy shutdown never terminates a run.** The worker's shutdown token
  aborts the in-flight step, releases the run lease within cja's
  `shutdown_drain_timeout` (default 2s; keep any custom drain ≥ the 1s
  release bound), and errors the job attempt so cja retries at ~2s backoff;
  the post-deploy worker claims the unleased row and resumes at the last
  persist. If the handler is dropped anyway or the process is `SIGKILL`ed,
  recovery falls back to lease expiry (90s) / cja reclaim (~120s) — delayed,
  never lost.

### Limits and failure classification

Per-kind caps: `RunLimits { max_turns: 20, max_total_tokens: 200_000 }` by
default; exceeding one fails the run with `fail_reason` `turn_budget` /
`token_budget`, transcript preserved. `fail_reason` values: `turn_budget`,
`token_budget`, `unknown_kind`, `corrupt_state`, `gateway`, `protocol`.

- **Retried by cja backoff** (lease released, job errors): connection
  failures, 429, 5xx.
- **Failed immediately** (job completes, no retry storm): gateway identity
  rejection, budget exceeded (HTTP 402), admin-only route, schema failures,
  timeouts (the attempt may have been billed; re-calling would double-bill
  and fork the transcript), unknown kind, corrupt run state, protocol
  errors.

A rig version bump changes the serialized `AgentRun` format; a
golden-fixture test pins the current format, and a skewed resume fails
loudly (`fail_reason = corrupt_state`) instead of corrupting.

### The enqueue/wake contract

`start` and `answer` enqueue `AdvanceRunJob` through cja's
`Job::enqueue_on` **on the same transaction as the row write**, and call
`state.job_enqueued()` only after commit — the same span/receipt/INSERT as
every cja enqueue. If you ever enqueue `AdvanceRunJob` yourself (recovery
tooling after a dead-letter), use `enqueue_on` on your own connection and
wake after commit the same way. The failure mode of skipping this is
silent: a `queued`/`running` run with no job to advance it, latched until
someone re-enqueues or `cancel()`s it.

### Registration drift, shutdown wiring, and ops

- **Registration drift**: forgetting `AdvanceRunJob` in
  `impl_job_registry!` compiles fine and fails only at runtime with
  `Unknown job type`. One-line app test:
  `assert!(Jobs::job_names().contains(&AdvanceRunJob::NAME));`
- **Shutdown wiring**: cancel-vs-shutdown only works if the app wires
  `SIGTERM` → one shared `CancellationToken` → `job_worker` (see the
  reference `main.rs` in `crates/home-server`). An unwired token means
  `SIGTERM` kills the process mid-step — recovery still works (lease
  expiry), just slower.
- **Ops sizing**: a long agent run occupies one worker slot for its
  duration — size worker count accordingly. Worst-case resume delay after
  an ungraceful death is cja's reclaim window (~120s), not the run lease;
  after a graceful shutdown it is cja's retry backoff (~2s).
- **Tool authority**: app tools run in-process with the app's full
  authority, driven by model output — file-style tools must validate paths
  (reject absolute and `..`); execution-style tools need a subprocess
  boundary.
- **Migrations**: call `cja_llm::durable::run_migrations`, never copy the
  SQL. Runtime-checked SQL compiles fine against a missing table and fails
  only at runtime (`relation "agent_runs" does not exist`). For dev
  databases: drop and recreate; never `sqlx migrate revert` (revert walks
  back through many migrations and half-destroys a DB).

## The two transports

* **Host** (default): `setec::TcpDialer` over the host network. The host must
  be on the tailnet (`tailscaled`): byte-vm, attic. On a shared host every
  container egresses as the host's node — **one identity key and one $20
  monthly budget for all of them**; `X-App` is only a log dimension.
* **In-process tailnet** (`tailnet` feature): the app joins the tailnet
  itself as an ephemeral tagged node and gets its own identity. On Cloud
  Run, cja-secret already joins (`SETEC_TRANSPORT=tailnet`) — share that
  node instead of joining a second one:

```rust,no_run
use std::sync::Arc;
use cja_llm::{Llm, LlmConfig};

# async fn example(refresher: &cja_secret::Refresher) -> cja_llm::Result<()> {
let config = LlmConfig::from_env()?.tailnet(refresher.tailnet().expect("tailnet transport"));
let llm = Llm::new(&config)?;
# let _ = llm;
# Ok(())
# }
```

cja-secret owns the node's shutdown; if it shuts down while an LLM request
is in flight, that request fails with a connect-class error — expected at
process shutdown. `Llm::new` warns when `K_SERVICE` is set and the dialer is
Host: the host network cannot reach the tailnet gateway there.

`http://` gateway URLs are tests-only (the loopback fake); production is
always `https`.

## Errors and retries

Gateway replies classify into `cja_llm::Error` variants — identically
whether they surface from `complete`/`extract`/`stream` or from a raw rig
agent (`?` does it for you):

* `IdentityRejected` — 403 identity: check the tailnet policy grant to
  `llm:443` (a grant to `tag:llm` on port 443). A node *without* the grant
  cannot even connect and fails with `Connect` instead, whose message says
  the same thing.
* `BudgetExceeded` — 402: the monthly budget ($20 per identity key by
  default, gateway-configured) is spent. Non-retryable.
* `AdminOnly` — 403 `admin only`: a programming error (wrong route).
* `Upstream` — any other provider/gateway reply, with status and body.
* `Timeout` — the total deadline or a stream's idle timeout. The gateway
  runs inference to completion upstream even when the client disconnects
  (bounded at 10 minutes) **and bills it**, so timeouts are never retried.
* `Connect` — dial/TLS/handshake failure or connect timeout: nothing was
  sent; retryable within the policy.
* `SchemaParse` — structured output failed to parse; carries the raw text.

The retry policy (`LlmConfig::retries`, default 3 attempts): connect-phase
failures (nothing sent) and 429s retry with exponential backoff, jitter,
and a seconds-form `Retry-After` honored as a floor (clamped to the
backoff ceiling). 5xx *replies* retry too, per the DEV-1923 policy: the
gateway's two 502 bodies (`gateway budget/key provisioning failed`,
`upstream unavailable`) mean no upstream call was made, so retrying them
can't double-bill — but any other 5xx (500/503/504 from Bifrost or a
provider) may follow upstream work, so a retry can double-bill. A stale
Bifrost virtual key (401 `access_not_found` / 403 `access_blocked`) gets
exactly one ordinary, attempt-consuming retry. Everything else — 402,
the wrapper 403s, other 4xx — never retries. Transport failures after
the request reached the connection (timeouts, resets) never retry either,
because the gateway may have billed the attempt.

Budgets have no in-flight reservation: N concurrent requests can each pass
the accrued-spend check before any settles. cja-llm adds no client-side
budgeting.

## Telemetry

Every HTTP call gets a `cja_llm.http` span carrying app name, model, attempt,
status, latency, and token counts — plus a completion event. **Prompt and
response content is never recorded**: `Llm::agent` pins rig's content
telemetry off, and a test enforces it. Do not enable it.

One more thing: rig 0.45 logs raw content on three targets regardless of
the content-telemetry setting — whole request/reply wire JSON at `TRACE`
on `rig::completions` and `rig::streaming`, and the raw structured-output
text at `WARN` on `rig_agent::agent::typed`. cja's tracing setup
(`cja::setup`) always caps those targets (`rig::completions` and
`rig::streaming` at DEBUG, `rig_agent::agent::typed` at ERROR), so an app
using `cja::setup` is covered. An app that builds its own subscriber must
add the same cap as a **separate `Targets` filter layer ANDed onto the
`EnvFilter`** — not `EnvFilter::add_directive`. `add_directive` only
replaces *static* directives: a span-scoped directive like
`RUST_LOG=info,[request]=trace` enables every event at or below its level
inside that span, whatever the target, because `EnvFilter` consults
span-scope directives before static ones. A stacked filter layer vetoes
instead — every layer must enable an event — and no `RUST_LOG` value can
raise it:

```rust,ignore
use tracing_subscriber::filter::{LevelFilter, Targets};

let rig_cap = Targets::new()
    .with_default(LevelFilter::TRACE)
    .with_target("rig::completions", LevelFilter::DEBUG)
    .with_target("rig::streaming", LevelFilter::DEBUG)
    .with_target("rig_agent::agent::typed", LevelFilter::ERROR);

// Stack it next to the EnvFilter — both must enable an event:
tracing_subscriber::registry()
    .with(/* your writer layers */)
    .with(env_filter)
    .with(rig_cap)
    .init();
```

`cja_llm::Error`'s `Debug` and `Display` — and every link of its
`source()` chain, which is what color_eyre and eyre reports print — never
print content (reply bodies, model output, or serde value fragments): the
`source()` of every error is a sanitized wrapper whose own `source()` is
`None`. The `raw` and `body` fields still carry content, as do the
originals behind the `Error::prompt_error()` and `Error::serde_error()`
accessors — don't log those.

No secret is read, stored, or logged by this crate. The placeholder
credential rig requires never leaves the process: the transport middleware
strips it, and the transport strips it again as a backstop.

## House defaults

`Llm::agent` sets `max_tokens` 4096 (Anthropic upstreams require one), a
named default model (`cja_llm::models::DEFAULT`), a tool-loop turn cap of 8
(`PromptError::MaxTurns` beyond it), and content telemetry off. Every setter
on the returned builder still applies.
