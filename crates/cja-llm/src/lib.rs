//! Opinionated [rig](https://github.com/0xPlaygrounds/rig) setup for cja apps,
//! keyless via the tailnet LLM gateway.
//!
//! The gateway (`https://llm.rooster-mermaid.ts.net`) identifies callers by
//! their tailnet node identity and holds the real provider keys, so apps carry
//! no credentials at all. This crate wires rig's OpenAI-shaped client to the
//! gateway through one shared HTTP transport with retries, timeouts, and
//! tracing, and exposes a thin convenience layer on top.
//!
//! ```rust,ignore
//! // Wired up in a later step: `Llm` lands with the client surface.
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
mod transport;
#[cfg(feature = "testing")]
pub mod testing;




use std::time::Duration;

/// Placeholder credential rig requires; never sent (stripped before the wire).
pub(crate) const PLACEHOLDER_KEY: &str = "cja-llm-keyless";
pub const DEFAULT_GATEWAY_URL: &str = "https://llm.rooster-mermaid.ts.net";
pub const DEFAULT_MAX_TOKENS: u64 = 4096;
pub const DEFAULT_MAX_TURNS: usize = 8;
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
pub const DEFAULT_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
