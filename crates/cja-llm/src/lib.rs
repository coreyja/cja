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
        let http = DynHttpClient::new(transport::GatewayHttp::new(config)?).with_middleware(
            transport::GatewayHeaders {
                app: config.app_header_value()?,
            },
        );
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
        self.agent(model)
            .build()
            .prompt(prompt)
            .stream()
            .filter_map(|item| async move {
                match item {
                    Ok(rig_agent::agent::MultiTurnStreamItem::StreamAssistantItem(
                        rig_core::streaming::Item::Event(rig_core::streaming::StreamEvent::Text {
                            text,
                            ..
                        }),
                    )) => Some(Ok(text)),
                    // Non-text events, tool items, usage calls, and the final
                    // response are not text deltas; errors classify.
                    Ok(_) => None,
                    Err(error) => Some(Err(Error::from(error))),
                }
            })
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
