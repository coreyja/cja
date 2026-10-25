//! Opinionated [rig](https://github.com/0xPlaygrounds/rig) setup for cja apps,
//! keyless via the tailnet LLM gateway.
//!
//! The gateway (`https://llm.rooster-mermaid.ts.net`) identifies callers by
//! their tailnet node identity and holds the real provider keys, so apps carry
//! no credentials at all. This crate wires rig's OpenAI-shaped client to the
//! gateway through one shared HTTP transport with retries, timeouts, and
//! tracing, and exposes a thin convenience layer on top.
//!
//! ```rust,no_run
//! use cja_llm::models;
//!
//! #[tokio::main]
//! async fn main() -> cja_llm::Result<()> {
//!     let llm = cja_llm::Llm::from_env()?;
//!     let reply = llm.agent(models::DEFAULT).build().prompt("Pick a color.").await?;
//!     println!("{}", reply.output());
//!     Ok(())
//! }
//! ```

pub mod rig {
    //! The pinned rig crates, re-exported so apps never name rig-core directly
    //! or pull a different rig version than the one cja-llm was built against.
    //!
    //! Apps writing tools with `#[rig_tool]` must also depend on `rig-agent`
    //! with the exact version from the README's version table — see the tools
    //! section there for why the macro needs it in the app's own manifest.
    pub use rig_agent; // Agent, AgentBuilder, AgentRunner, rig_tool (derive feature), ...
    pub use rig_core; // providers::openai, completion, streaming, http_client, ...
}

pub mod config;
pub mod error;
pub mod models;
#[cfg(feature = "testing")]
pub mod testing;
mod transport;

pub use config::{DialerChoice, LlmConfig, RetryPolicy};
pub use error::{Error, Result};

use std::time::Duration;

use futures::StreamExt as _;
use rig_core::completion::Message;
use rig_core::http_client::DynHttpClient;
use rig_core::providers::openai::Route;

/// The built client: one rig OpenAI-shaped provider client on one shared
/// transport, plus the convenience surface. Cheap to clone.
#[derive(Clone)]
pub struct Llm {
    client: rig_core::providers::openai::OpenAI,
}

impl std::fmt::Debug for Llm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Llm")
            .field("base_url", &self.client.config().base_url)
            .finish_non_exhaustive()
    }
}

impl Llm {
    /// Wires the transport, the header middleware, and rig's `OpenAI` client
    /// (chat-completions route, gateway base URL) together.
    pub fn new(config: &LlmConfig) -> Result<Self> {
        #[cfg(feature = "tailnet")]
        if std::env::var("K_SERVICE").is_ok() && matches!(config.dialer, DialerChoice::Host) {
            tracing::warn!(
                "Cloud Run (K_SERVICE set) with the host dialer: the host \
                 network cannot reach the tailnet gateway; join via \
                 cja_tailnet::Tailnet::join, or pass cja-secret's \
                 Refresher::tailnet() to LlmConfig::tailnet"
            );
        }
        // Validated first, so an empty app name can never reach the wire or
        // a span: the X-App fallback is never empty, by contract.
        let app = config.app_header_value()?;
        let http = DynHttpClient::new(transport::GatewayHttp::new(config)?)
            .with_middleware(transport::GatewayHeaders { app });
        let openai = rig_core::providers::openai::OpenAIConfig::new(PLACEHOLDER_KEY)
            .with_base_url(config.openai_base_url())
            .with_route(Route::Chat)
            .connect(http);
        Ok(Self { client: openai })
    }

    /// [`LlmConfig::from_env`] then [`Llm::new`].
    pub fn from_env() -> Result<Self> {
        Self::new(&LlmConfig::from_env()?)
    }

    /// The wired rig client, for everything cja-llm doesn't wrap.
    pub fn client(&self) -> &rig_core::providers::openai::OpenAI {
        &self.client
    }

    /// An agent builder with the house defaults: `max_tokens` 4096
    /// (Anthropic upstreams require one), the named model, a tool-loop turn
    /// cap of 8, and rig's content telemetry pinned off. Every setter still
    /// applies; `prompt` is on the built agent: `.build().prompt(..)`.
    pub fn agent(&self, model: impl Into<String>) -> rig_agent::AgentBuilder {
        rig_agent::AgentBuilder::new(self.client.completion(model))
            .max_tokens(DEFAULT_MAX_TOKENS)
            .default_max_turns(DEFAULT_MAX_TURNS)
            // rig records prompt/response/preamble/tool content on its GenAI
            // spans only when this is true (default false). Pinned explicitly
            // off so a rig bump can't flip it.
            .record_content_telemetry(false)
    }

    /// Model and prompt in, text out.
    pub async fn complete(&self, model: &str, prompt: impl Into<Message>) -> Result<String> {
        Ok(self.agent(model).build().prompt(prompt).await?.output())
    }

    /// Model and prompt in, a deserialized `T` out, through rig's
    /// structured output. A parse failure returns [`Error::SchemaParse`]
    /// carrying the model's raw text.
    pub async fn extract<T>(&self, model: &str, prompt: impl Into<Message>) -> Result<T>
    where
        T: schemars::JsonSchema + serde::de::DeserializeOwned + Send + 'static,
    {
        Ok(self
            .agent(model)
            .build()
            .prompt_typed::<T>(prompt)
            .await?
            .output)
    }

    /// Text deltas from a streamed reply. Lazy: nothing runs until the first
    /// poll, and dropping the stream cancels the request.
    pub fn stream(
        &self,
        model: &str,
        prompt: impl Into<Message>,
    ) -> impl futures::Stream<Item = Result<String>> {
        let inner =
            self.agent(model)
                .build()
                .prompt(prompt)
                .stream()
                .filter_map(|item| async move {
                    match item {
                        Ok(rig_agent::agent::MultiTurnStreamItem::StreamAssistantItem(
                            rig_core::streaming::Item::Event(
                                rig_core::streaming::StreamEvent::Text { text, .. },
                            ),
                        )) => Some(Ok(text)),
                        // Non-text events, tool items, usage calls, and the final
                        // response are not text deltas; errors classify.
                        Ok(_) => None,
                        Err(error) => Some(Err(Error::from(error))),
                    }
                });
        Box::pin(inner)
    }
}

/// Placeholder credential rig requires; never sent (stripped before the wire).
pub(crate) const PLACEHOLDER_KEY: &str = "cja-llm-keyless";
pub const DEFAULT_GATEWAY_URL: &str = "https://llm.rooster-mermaid.ts.net";
pub const DEFAULT_MAX_TOKENS: u64 = 4096;
pub const DEFAULT_MAX_TURNS: usize = 8;
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
pub const DEFAULT_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use std::error::Error as _;

    use rig_agent::completion::PromptError;
    use rig_agent::rig_tool;
    use serde::Deserialize;

    use super::*;
    use crate::config::RetryPolicy;
    use crate::testing::{FakeGateway, Ran, test_config};

    #[rig_tool(description = "Add two integers")]
    async fn add(
        #[rig(context)] context: &mut rig_core::tool::ToolContext,
        a: i32,
        b: i32,
    ) -> Result<i32, rig_core::tool::ToolExecutionError> {
        if let Some(ran) = context.scope::<Ran>() {
            *ran.0.lock().await = true;
        }
        Ok(a + b)
    }

    #[rig_tool(description = "Echo the text back")]
    pub(crate) async fn echo(
        #[rig(context)] context: &mut rig_core::tool::ToolContext,
        text: String,
    ) -> Result<String, rig_core::tool::ToolExecutionError> {
        if let Some(ran) = context.scope::<Ran>() {
            *ran.0.lock().await = true;
        }
        Ok(text)
    }

    /// The model string every test scripts, so spans and wire bodies agree.
    const MODEL: &str = models::DEFAULT;

    fn config(url: &url::Url) -> LlmConfig {
        test_config(url.clone())
    }

    fn fast_config(url: &url::Url, request_timeout: Duration) -> LlmConfig {
        config(url).request_timeout(request_timeout)
    }

    // ------------------------------------------------------------------
    // Transport criteria
    // ------------------------------------------------------------------

    /// The mandated wire-level absence test: no key-shaped header leaves the
    /// process, and `X-App` is stamped from the config.
    #[tokio::test]
    async fn no_key_leaves_the_process() {
        let fake = FakeGateway::start().await;
        fake.chat_completion(MODEL, "pong");
        let llm = Llm::new(&config(&fake.url())).unwrap();

        let text = llm.complete(MODEL, "ping").await.unwrap();
        assert_eq!(text, "pong");

        let requests = fake.requests();
        assert_eq!(requests.len(), 1);
        for stripped in ["authorization", "x-api-key", "api-key", "x-goog-api-key"] {
            assert!(
                requests[0].headers.get(stripped).is_none(),
                "{stripped} must never leave the process"
            );
        }
        assert_eq!(
            requests[0]
                .headers
                .get("x-app")
                .and_then(|value| value.to_str().ok()),
            Some("fake-test")
        );
    }

    /// The no-key/`X-App` guarantees hold on the retry path and the
    /// streaming path too — every recorded request, not just the first
    /// unary one. The config's tiny backoffs keep the 429 retry fast.
    #[tokio::test]
    async fn no_key_leaves_the_process_on_stream_and_retry() {
        let fake = FakeGateway::start().await;
        fake.status(429, r#"{"type":"rate_limit_exceeded"}"#);
        fake.chat_completion(MODEL, "second try");
        let llm = Llm::new(&config(&fake.url())).unwrap();
        assert_eq!(
            llm.complete(MODEL, "ping").await.unwrap(),
            "second try",
            "the 429 must retry (two requests so far)"
        );

        fake.chat_stream(MODEL, &["a", "b"]);
        let joined: String = llm
            .stream(MODEL, "ping")
            .map(|delta| delta.unwrap())
            .collect::<Vec<String>>()
            .await
            .concat();
        assert_eq!(joined, "ab");

        let requests = fake.requests();
        assert_eq!(requests.len(), 3, "429 + retry, then the stream");
        for request in &requests {
            for stripped in ["authorization", "x-api-key", "api-key", "x-goog-api-key"] {
                assert!(
                    request.headers.get(stripped).is_none(),
                    "{stripped} must never leave the process on {}",
                    request.path
                );
            }
            assert_eq!(
                request
                    .headers
                    .get("x-app")
                    .and_then(|value| value.to_str().ok()),
                Some("fake-test"),
                "X-App on {}",
                request.path
            );
        }
    }

    /// A trailing `/` on the gateway URL (the `CJA_LLM_URL` hazard) must not
    /// produce `//v1` on the wire: the gateway 400s non-canonical paths.
    #[tokio::test]
    async fn no_double_slash_in_path() {
        let fake = FakeGateway::start().await;
        fake.chat_completion(MODEL, "pong");
        let slashed = url::Url::parse(&format!("{}/", fake.url())).unwrap();
        let llm = Llm::new(&config(&slashed)).unwrap();

        llm.complete(MODEL, "ping").await.unwrap();

        assert_eq!(fake.requests()[0].path, "/v1/chat/completions");
    }

    #[tokio::test]
    async fn retries_429_then_succeeds() {
        let fake = FakeGateway::start().await;
        fake.status(429, r#"{"type":"rate_limit_exceeded"}"#);
        fake.chat_completion(MODEL, "second try");
        let llm = Llm::new(&config(&fake.url())).unwrap();

        let text = llm.complete(MODEL, "ping").await.unwrap();
        assert_eq!(text, "second try");
        assert_eq!(fake.requests().len(), 2);
    }

    #[tokio::test]
    async fn retries_502_upstream_unavailable_then_succeeds() {
        let fake = FakeGateway::start().await;
        fake.status(502, r#"{"error":"upstream unavailable"}"#);
        fake.chat_completion(MODEL, "second try");
        let llm = Llm::new(&config(&fake.url())).unwrap();

        let text = llm.complete(MODEL, "ping").await.unwrap();
        assert_eq!(text, "second try");
        assert_eq!(fake.requests().len(), 2);
    }

    #[tokio::test]
    async fn does_not_retry_402() {
        let fake = FakeGateway::start().await;
        fake.status(
            402,
            r#"{"type":"budget_exceeded","error":{"message":"spend cap"}}"#,
        );
        let llm = Llm::new(&config(&fake.url())).unwrap();

        let error = llm.complete(MODEL, "ping").await.unwrap_err();
        assert!(matches!(error, Error::BudgetExceeded { .. }), "{error:?}");
        assert_eq!(fake.requests().len(), 1, "402 is terminal");
    }

    #[tokio::test]
    async fn identity_403_is_terminal_and_typed() {
        let fake = FakeGateway::start().await;
        fake.status(
            403,
            r#"{"error":"forbidden: caller identity could not be determined"}"#,
        );
        let llm = Llm::new(&config(&fake.url())).unwrap();

        let error = llm.complete(MODEL, "ping").await.unwrap_err();
        assert!(matches!(error, Error::IdentityRejected { .. }), "{error:?}");
        assert!(
            error.to_string().contains("tag:llm"),
            "the message points at the tailnet policy grant: {error}"
        );
        assert_eq!(fake.requests().len(), 1);
    }

    #[tokio::test]
    async fn admin_403_is_terminal() {
        let fake = FakeGateway::start().await;
        fake.status(403, r#"{"error":"admin only"}"#);
        let llm = Llm::new(&config(&fake.url())).unwrap();

        let error = llm.complete(MODEL, "ping").await.unwrap_err();
        assert!(matches!(error, Error::AdminOnly { .. }), "{error:?}");
        assert_eq!(fake.requests().len(), 1);
    }

    #[tokio::test]
    async fn stale_virtual_key_retries_once() {
        // 401 access_not_found then success: exactly two requests, one retry.
        let fake = FakeGateway::start().await;
        fake.status(401, r#"{"type":"access_not_found"}"#);
        fake.chat_completion(MODEL, "healed");
        let llm = Llm::new(&config(&fake.url())).unwrap();
        let text = llm.complete(MODEL, "ping").await.unwrap();
        assert_eq!(text, "healed");
        assert_eq!(fake.requests().len(), 2);

        // A second stale-key body never triggers another retry: after the
        // once-only retry, the error surfaces as Upstream, never as the
        // identity 403.
        let fake = FakeGateway::start().await;
        fake.status(401, r#"{"type":"access_not_found"}"#);
        fake.status(401, r#"{"type":"access_not_found"}"#);
        let llm = Llm::new(&config(&fake.url())).unwrap();
        let error = llm.complete(MODEL, "ping").await.unwrap_err();
        assert!(
            matches!(error, Error::Upstream { status: 401, .. }),
            "{error:?}"
        );
        assert_eq!(fake.requests().len(), 2);

        // With a single attempt allowed, the stale-key retry never happens.
        let fake = FakeGateway::start().await;
        fake.status(403, r#"{"type":"access_blocked"}"#);
        let config = config(&fake.url()).retries(RetryPolicy {
            max_attempts: 1,
            ..RetryPolicy::default()
        });
        let llm = Llm::new(&config).unwrap();
        let error = llm.complete(MODEL, "ping").await.unwrap_err();
        assert!(
            matches!(error, Error::Upstream { status: 403, .. }),
            "{error:?}"
        );
        assert_eq!(fake.requests().len(), 1);
    }

    /// A timeout after the request was sent is never retried: the gateway
    /// drains and bills abandoned attempts. The fake still saw the request.
    #[tokio::test]
    async fn timeout_after_send_is_not_retried() {
        let fake = FakeGateway::start().await;
        fake.delay(Duration::from_secs(5));
        fake.chat_completion(MODEL, "too late");
        let llm = Llm::new(&fast_config(&fake.url(), Duration::from_millis(250))).unwrap();

        let error = llm.complete(MODEL, "ping").await.unwrap_err();
        assert!(matches!(error, Error::Timeout { .. }), "{error:?}");
        assert_eq!(
            fake.requests().len(),
            1,
            "the fake saw the request; the timeout did not retry it"
        );
    }

    /// A reset while awaiting the response head is post-queue and terminal:
    /// the request may have been written and billed.
    #[tokio::test]
    async fn post_queue_reset_is_terminal() {
        let fake = FakeGateway::start().await;
        fake.drop_connection();
        let llm = Llm::new(&config(&fake.url())).unwrap();

        let error = llm.complete(MODEL, "ping").await.unwrap_err();
        assert!(
            !matches!(error, Error::Connect { .. } | Error::Timeout { .. }),
            "post-queue failures are neither connect nor timeout: {error:?}"
        );
        assert_eq!(fake.requests().len(), 1, "no retry after the reset");
    }

    /// A node without the tailnet grant cannot even open TCP: the failure is
    /// a retryable Connect (never a 403, never an endless retry), bounded by
    /// the connect timeout, with the grant hint in the message. The dialer
    /// count is the attempt-count oracle.
    #[tokio::test]
    async fn connect_timeout_is_connect_not_timeout_and_retries() {
        let dialer = crate::testing::CountingDialer::never_completes();
        let config = config(&url::Url::parse("https://llm.rooster-mermaid.ts.net").unwrap())
            .connect_timeout(Duration::from_millis(250))
            .retries(RetryPolicy {
                max_attempts: 2,
                initial_backoff: Duration::from_millis(10),
                ..RetryPolicy::default()
            })
            .dialer(dialer.clone());
        let llm = Llm::new(&config).unwrap();

        let error = llm.complete(MODEL, "ping").await.unwrap_err();
        assert!(matches!(error, Error::Connect { .. }), "{error:?}");
        assert!(
            error.to_string().contains("llm:443"),
            "the connect message mentions the grant: {error}"
        );
        assert_eq!(dialer.dials(), 2, "every attempt dials once, then stops");
    }

    /// The total request deadline also bounds the connect phase: a short
    /// `request_timeout` cannot be overrun by a long `connect_timeout` on a
    /// hanging dialer. The deadline-clamped failure is still pre-send, so it
    /// stays a Connect-class error (never Timeout's "may have billed"
    /// wording).
    #[tokio::test]
    async fn connect_is_bounded_by_the_request_deadline() {
        let dialer = crate::testing::CountingDialer::never_completes();
        let config = config(&url::Url::parse("https://llm.rooster-mermaid.ts.net").unwrap())
            .connect_timeout(Duration::from_secs(10))
            .request_timeout(Duration::from_millis(250))
            .retries(RetryPolicy {
                max_attempts: 1,
                ..RetryPolicy::default()
            })
            .dialer(dialer.clone());
        let llm = Llm::new(&config).unwrap();

        let started = std::time::Instant::now();
        let error = llm.complete(MODEL, "ping").await.unwrap_err();
        let elapsed = started.elapsed();

        assert!(matches!(error, Error::Connect { .. }), "{error:?}");
        assert!(
            elapsed < Duration::from_secs(5),
            "the request deadline, not the 10s connect timeout, bounded the call: {elapsed:?}"
        );
    }

    /// Idle between stream chunks ends the stream with a timeout; nothing
    /// retries after the first byte.
    #[tokio::test]
    async fn stream_idle_timeout_ends_stream() {
        let fake = FakeGateway::start().await;
        fake.chat_stream_with_gap(MODEL, &["first", "second"], Duration::from_secs(5));
        let llm = Llm::new(&config(&fake.url())).unwrap();

        let mut stream = llm.stream(MODEL, "ping");
        let first = stream.next().await.unwrap().unwrap();
        assert_eq!(first, "first");
        let second = stream.next().await.unwrap();
        assert!(matches!(second, Err(Error::Timeout { .. })), "{second:?}");
        drop(stream);
        assert_eq!(fake.requests().len(), 1);
    }

    /// Dropping the stream cancels the request: the fake observes the
    /// disconnect in a bounded window.
    ///
    /// Mutation-checked (PR review round 1, both directions): with the
    /// stream guard's abort disabled this test still passes, and so does a
    /// stream consumed fully to its end — hyper 1.12 closes the http/1.1
    /// connection itself when the response body is dropped (mid-stream) or
    /// completes with the sender already dropped. No wire-observable
    /// behavior distinguishes the guard, which stays as defense in depth
    /// for determinism (an immediate abort rather than hyper's lazy close);
    /// this test pins the observable contract (prompt disconnect), not the
    /// guard's internals.
    #[tokio::test]
    async fn dropping_stream_cancels() {
        let fake = FakeGateway::start().await;
        fake.chat_stream_with_gap(MODEL, &["first", "second"], Duration::from_secs(5));
        let llm = Llm::new(&config(&fake.url())).unwrap();

        let mut stream = llm.stream(MODEL, "ping");
        assert_eq!(stream.next().await.unwrap().unwrap(), "first");
        drop(stream);

        fake.wait_until_disconnected().await;
    }

    /// A stream consumed fully to its end must not leak its connection
    /// either: after the last chunk the socket must close promptly (the
    /// guard aborts the driver at end-of-stream), not sit idle in
    /// keep-alive — see `dropping_stream_cancels`' mutation note for why
    /// hyper alone usually closes it too.
    #[tokio::test]
    async fn ended_stream_does_not_leak_the_connection() {
        let fake = FakeGateway::start().await;
        fake.chat_stream(MODEL, &["a", "b"]);
        let llm = Llm::new(&config(&fake.url())).unwrap();

        let mut stream = llm.stream(MODEL, "ping");
        while stream.next().await.is_some() {}
        drop(stream);

        fake.wait_until_disconnected().await;
    }

    // ------------------------------------------------------------------
    // Opinionated-rig criteria
    // ------------------------------------------------------------------

    /// The mandated rig-agent end-to-end test: a tool call round-trips
    /// through the fake — the model calls `add`, rig executes it, and the
    /// final reply reports the result. Its app-shaped twin (macro expansion
    /// without rig-core in the manifest) lives in cja-llm-app-fixture.
    #[tokio::test]
    async fn agent_round_trips_a_tool_call() {
        let fake = FakeGateway::start().await;
        fake.tool_call(MODEL, "call-1", "add", &serde_json::json!({"a": 2, "b": 3}));
        fake.chat_completion(MODEL, "5");
        let llm = Llm::new(&config(&fake.url())).unwrap();
        let ran = Arc::new(Ran(tokio::sync::Mutex::new(false)));

        let reply = llm
            .agent(MODEL)
            .tool(Add)
            .build()
            .prompt("add 2 and 3")
            .tool_context(rig_core::tool::ToolContext::default().with_scope(ran.clone()))
            .await
            .unwrap();

        assert!(reply.output().contains('5'), "{}", reply.output());
        assert!(*ran.0.lock().await, "the tool ran");
        assert_eq!(
            fake.requests().len(),
            2,
            "the tool call, then the final reply"
        );
    }

    #[derive(Debug, Deserialize, schemars::JsonSchema)]
    pub(crate) struct Answer {
        pub(crate) answer: u32,
    }

    #[tokio::test]
    async fn complete_stream_extract_happy_paths() {
        let fake = FakeGateway::start().await;
        let llm = Llm::new(&config(&fake.url())).unwrap();

        fake.chat_completion(MODEL, "a reply");
        assert_eq!(llm.complete(MODEL, "hi").await.unwrap(), "a reply");

        fake.chat_stream(MODEL, &["Hello ", "world"]);
        let joined: String = llm
            .stream(MODEL, "hi")
            .map(|delta| delta.unwrap())
            .collect::<Vec<String>>()
            .await
            .concat();
        assert_eq!(joined, "Hello world");

        fake.chat_json(MODEL, &serde_json::json!({"answer": 42}));
        let answer: Answer = llm.extract(MODEL, "the answer?").await.unwrap();
        assert_eq!(answer.answer, 42);
    }

    #[tokio::test]
    async fn extract_parse_failure_carries_raw_text() {
        let fake = FakeGateway::start().await;
        fake.chat_completion(MODEL, "not json");
        let llm = Llm::new(&config(&fake.url())).unwrap();

        let error = llm
            .extract::<Answer>(MODEL, "the answer?")
            .await
            .unwrap_err();
        let Error::SchemaParse { raw, .. } = &error else {
            panic!("unexpected {error:?}");
        };
        assert_eq!(raw, "not json");
        assert!(error.source().is_some(), "the serde error is the source");
    }

    /// The house defaults reach the wire: `max_tokens` 4096 on the request
    /// body, and the tool-loop turn cap at 8 model calls.
    #[tokio::test]
    async fn house_defaults_are_set() {
        let fake = FakeGateway::start().await;
        fake.chat_completion(MODEL, "defaults");
        let llm = Llm::new(&config(&fake.url())).unwrap();
        llm.complete(MODEL, "ping").await.unwrap();
        assert_eq!(fake.requests()[0].json()["max_tokens"], 4096);

        // Nine tool-call replies: the run must stop at 8 model calls.
        let fake = FakeGateway::start().await;
        for round in 1..=9 {
            fake.tool_call(
                MODEL,
                &format!("call-{round}"),
                "add",
                &serde_json::json!({"a": 1, "b": 2}),
            );
        }
        let llm = Llm::new(&config(&fake.url())).unwrap();
        let ran = Arc::new(Ran(tokio::sync::Mutex::new(false)));
        let Err(prompt_error) = llm
            .agent(MODEL)
            .tool(Add)
            .build()
            .prompt("loop forever")
            .tool_context(rig_core::tool::ToolContext::default().with_scope(ran.clone()))
            .await
        else {
            panic!("nine tool calls must exhaust the turn cap");
        };
        assert!(
            matches!(prompt_error, PromptError::MaxTurns { max_turns: 8, .. }),
            "{prompt_error}"
        );
        assert_eq!(fake.requests().len(), 8, "the turn cap stopped the loop");
    }

    /// `?` in app agent code classifies gateway errors through the
    /// hand-written From, not into Rig.
    #[tokio::test]
    async fn question_mark_classifies_gateway_errors() {
        async fn ask(llm: &Llm) -> Result<String> {
            Ok(llm.agent(MODEL).build().prompt("x").await?.output())
        }

        let fake = FakeGateway::start().await;
        fake.status(402, r#"{"type":"budget_exceeded"}"#);
        let llm = Llm::new(&config(&fake.url())).unwrap();
        assert!(matches!(ask(&llm).await, Err(Error::BudgetExceeded { .. })));

        let fake = FakeGateway::start().await;
        fake.status(
            403,
            r#"{"error":"forbidden: caller identity could not be determined"}"#,
        );
        let llm = Llm::new(&config(&fake.url())).unwrap();
        assert!(matches!(
            ask(&llm).await,
            Err(Error::IdentityRejected { .. })
        ));
    }

    /// Timeout and connect failures surface through raw agents too — the
    /// relayed Report path the classifier's markers exist for.
    #[tokio::test]
    async fn timeout_and_connect_surface_through_agents_too() {
        let fake = FakeGateway::start().await;
        fake.delay(Duration::from_secs(5));
        fake.chat_completion(MODEL, "too late");
        let llm = Llm::new(&fast_config(&fake.url(), Duration::from_millis(250))).unwrap();
        let Err(prompt_error) = llm.agent(MODEL).build().prompt("x").await else {
            panic!("the delayed reply must time out");
        };
        let error = Error::from(prompt_error);
        assert!(matches!(error, Error::Timeout { .. }), "{error:?}");
        assert_eq!(fake.requests().len(), 1);

        let dialer = crate::testing::CountingDialer::never_completes();
        let config = config(&url::Url::parse("https://llm.rooster-mermaid.ts.net").unwrap())
            .connect_timeout(Duration::from_millis(250))
            .dialer(dialer.clone());
        let llm = Llm::new(&config).unwrap();
        let Err(prompt_error) = llm.agent(MODEL).build().prompt("x").await else {
            panic!("the dead dialer must fail");
        };
        let error = Error::from(prompt_error);
        assert!(matches!(error, Error::Connect { .. }), "{error:?}");
    }
}

#[cfg(test)]
mod tracing_tests {
    use std::io;
    use std::sync::Arc;
    use std::sync::Mutex;

    use tracing_subscriber::fmt::MakeWriter;
    use tracing_subscriber::fmt::format::FmtSpan;

    use super::tests::{Answer, Echo};
    use super::*;
    use crate::testing::{FakeGateway, Ran, test_config};

    const MODEL: &str = models::DEFAULT;
    const SENTINEL_PREAMBLE: &str = "SENTINEL-PREAMBLE-7f3a";
    const SENTINEL_PROMPT: &str = "SENTINEL-PROMPT-9b2c";
    const SENTINEL_TOOL: &str = "SENTINEL-TOOL-4d1e";
    const SENTINEL_REPLY: &str = "SENTINEL-REPLY-8c9d";
    const SENTINEL_COMPLETE: &str = "SENTINEL-COMPLETE-1a2b";
    const SENTINEL_STREAM: &str = "SENTINEL-STREAM-3c4d";
    const SENTINEL_EXTRACT: &str = "SENTINEL-EXTRACT-5e6f";

    /// A `MakeWriter` capturing everything the subscriber prints into a
    /// shared buffer.
    #[derive(Clone, Default)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    struct SharedWrite(Arc<Mutex<Vec<u8>>>);

    impl SharedWriter {
        fn text(&self) -> String {
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

    /// The process-wide capture: one buffer, one global subscriber. A
    /// thread-local `set_default` loses spans when the test harness's
    /// thread pool interleaves with tokio's current-thread runtimes under
    /// parallel test execution, so the capture must be global to be
    /// reliable. Only this test's runs carry sentinels, so the negative
    /// assertion stays exact.
    static CAPTURE: std::sync::OnceLock<SharedWriter> = std::sync::OnceLock::new();

    fn capture() -> SharedWriter {
        let writer = CAPTURE.get_or_init(SharedWriter::default).clone();
        static INSTALL: std::sync::Once = std::sync::Once::new();
        INSTALL.call_once(|| {
            let subscriber = tracing_subscriber::fmt()
                .with_writer(writer.clone())
                .with_max_level(tracing::Level::TRACE)
                .with_span_events(FmtSpan::FULL)
                .with_ansi(false)
                .finish();
            // Another global default (a test binary may only ever set one)
            // would make the capture vacuous; a failure here fails every
            // assertion in this test loudly instead.
            tracing::subscriber::set_global_default(subscriber)
                .expect("no other global tracing subscriber");
        });
        writer
    }

    /// Spans carry model, app, status, latency, and token counts — and never
    /// prompt, response, preamble, or tool content, whichever surface ran.
    /// The capture is deliberately unfiltered (TRACE level, span events on)
    /// so a future default that turns rig's content telemetry on fails this
    /// test instead of hiding behind a filter.
    ///
    /// Mutation-checked: flipping `Llm::agent`'s
    /// `record_content_telemetry(false)` to `true` fails this test on the
    /// first sentinel.
    #[tokio::test]
    async fn span_has_metadata_but_never_content() {
        let writer = capture();

        let fake = FakeGateway::start().await;
        let llm = Llm::new(&test_config(fake.url())).unwrap();

        // (1) An agent tool round-trip with a sentinel in every content
        // position: preamble, prompt, tool arguments, tool result, reply.
        fake.tool_call(
            MODEL,
            "call-1",
            "echo",
            &serde_json::json!({ "text": SENTINEL_TOOL }),
        );
        fake.chat_completion(MODEL, SENTINEL_REPLY);
        let ran = Arc::new(Ran(tokio::sync::Mutex::new(false)));
        let reply = llm
            .agent(MODEL)
            .tool(Echo)
            .preamble(SENTINEL_PREAMBLE)
            .build()
            .prompt(SENTINEL_PROMPT)
            .tool_context(rig_core::tool::ToolContext::default().with_scope(ran.clone()))
            .await
            .unwrap();
        assert_eq!(reply.output(), SENTINEL_REPLY);
        assert!(*ran.0.lock().await, "the tool ran");

        // (2) complete().
        fake.chat_completion(MODEL, SENTINEL_COMPLETE);
        assert_eq!(
            llm.complete(MODEL, SENTINEL_PROMPT).await.unwrap(),
            SENTINEL_COMPLETE
        );

        // (3) stream(), consumed to the end.
        fake.chat_stream(MODEL, &[SENTINEL_STREAM]);
        let joined: String = llm
            .stream(MODEL, SENTINEL_PROMPT)
            .map(|delta| delta.unwrap())
            .collect::<Vec<String>>()
            .await
            .concat();
        assert_eq!(joined, SENTINEL_STREAM);

        // (4) extract(), against a JSON reply carrying its own sentinel.
        fake.chat_json(
            MODEL,
            &serde_json::json!({ "answer": 42, "note": SENTINEL_EXTRACT }),
        );
        let answer: Answer = llm.extract(MODEL, SENTINEL_PROMPT).await.unwrap();
        assert_eq!(answer.answer, 42);

        let text = writer.text();
        // Every positive assertion runs on `checked` — the capture after
        // dropping rig's raw-wire TRACE records — so none of them can be
        // satisfied by the very records the strip removes.
        let checked = strip_raw_wire_trace_records(&text);

        // Positive assertions: the capture works and the metadata is
        // there, from cja-llm's own records.
        //
        // Agent runs open rig's `rig::agent_chat` span (rig-agent-0.45.0
        // src/agent/telemetry.rs:15: `completion_parent_span!(target:
        // "rig::agent_chat", ..)`) — the span an agent run actually
        // opens, unlike the TRACE wire dumps on `rig::completions` that
        // the strip removes. Its lifecycle line proves rig's telemetry
        // was captured at all.
        assert!(
            checked.contains(" rig::agent_chat: new")
                || checked.contains(" rig::agent_chat: close"),
            "rig's agent-chat span lifecycle line; capture {} bytes, {} lines; head: {}",
            checked.len(),
            checked.lines().count(),
            checked.lines().take(3).collect::<Vec<_>>().join(" | ")
        );
        // Exactly one completion event per HTTP call, and the asserted
        // fields come from cja-llm's own event — target `cja_llm::http`
        // on the same line — so rig's `gen_ai.usage.*` fields on other
        // lines cannot satisfy these assertions.
        let finished: Vec<&str> = checked
            .lines()
            .filter(|line| line.contains("gateway http call finished"))
            .collect();
        assert_eq!(
            finished.len(),
            fake.requests().len(),
            "exactly one completion event per HTTP call"
        );
        for line in &finished {
            assert!(
                line.contains("cja_llm::http"),
                "cja-llm's own event: {line}"
            );
            assert!(line.contains("app=fake-test"), "the app name: {line}");
            assert!(
                line.contains("model=anthropic/claude-haiku-5-5"),
                "the model: {line}"
            );
            assert!(line.contains("status=200"), "the status: {line}");
            assert!(line.contains("input_tokens=3"), "input tokens: {line}");
            assert!(line.contains("output_tokens=2"), "output tokens: {line}");
        }

        // Negative assertion: no content sentinel anywhere except rig's
        // raw-wire TRACE dumps. rig 0.45 unconditionally logs the whole
        // request and reply JSON on the `rig::completions` and
        // `rig::streaming` targets when they are TRACE-enabled
        // (`providers::internal::trace_json`) — wire debugging, not GenAI
        // telemetry, and nothing a library can gate from inside. Every
        // span and every other event stays in the capture, so flipping the
        // content-telemetry gate still fails this test; cja's tracing
        // setup caps those targets in production (see `cja::setup`), and
        // the README tells apps with their own subscriber to do the same.
        for sentinel in [
            SENTINEL_PREAMBLE,
            SENTINEL_PROMPT,
            SENTINEL_TOOL,
            SENTINEL_REPLY,
            SENTINEL_COMPLETE,
            SENTINEL_STREAM,
            SENTINEL_EXTRACT,
        ] {
            assert!(
                !checked.contains(sentinel),
                "content leaked into telemetry: {sentinel}"
            );
        }
    }

    /// Drops TRACE records on rig's raw-wire debug targets, whose pretty
    /// JSON bodies span multiple lines. A record starts at a timestamped
    /// line; continuation lines carry none.
    fn strip_raw_wire_trace_records(text: &str) -> String {
        let mut kept = String::new();
        let mut dropping = false;
        for line in text.lines() {
            let is_record_start = line.starts_with("20");
            if is_record_start {
                dropping = line.contains(" TRACE ")
                    && (line.contains(": rig::completions: ")
                        || line.contains(": rig::streaming: "));
            }
            if !dropping {
                kept.push_str(line);
                kept.push('\n');
            }
        }
        kept
    }
}
