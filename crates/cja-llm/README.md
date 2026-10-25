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
add the same three directives:

```rust,ignore
filter
    .add_directive("rig::completions=debug".parse()?)
    .add_directive("rig::streaming=debug".parse()?)
    .add_directive("rig_agent::agent::typed=error".parse()?);
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
