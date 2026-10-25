//! Compile-and-run proof of the app-side `#[rig_tool]` rule from cja-llm's
//! README: an app that depends on cja-llm and rig-agent — but never on
//! rig-core — can define tools with the macro and run a tool round-trip
//! against the testing fake gateway.
//!
//! rig-derive resolves generated paths via `proc_macro_crate`, which reads
//! the *expanding crate's* manifest: with only cja-llm (which re-exports
//! rig), the macro's `::rig_core` path does not resolve. Naming rig-agent
//! (whose version must be copied from cja-llm's README table) makes the
//! expansion resolve through `::rig_agent::__core` instead. Apps never
//! name rig-core; they reach rig types through `cja_llm::rig`.

#[cfg(test)]
use std::time::Duration;

#[cfg(test)]
use cja_llm::LlmConfig;
#[cfg(test)]
use cja_llm::config::RetryPolicy;
use cja_llm::rig::rig_agent::rig_tool;
use cja_llm::rig::rig_core::tool::{ToolContext, ToolExecutionError};

/// Host-side side-effect flag, attached to the run's tool context so the
/// macro-generated (unit-struct) tool can reach it through its
/// `#[rig(context)]` parameter. Behind a mutex the tool locks (and so
/// genuinely awaits) when it runs.
pub struct Ran(tokio::sync::Mutex<bool>);

/// The README's add tool, with a side-effect flag the round-trip test can
/// observe.
#[rig_tool(description = "Add two integers")]
pub async fn add(
    #[rig(context)] context: &mut ToolContext,
    a: i32,
    b: i32,
) -> Result<i32, ToolExecutionError> {
    if let Some(ran) = context.scope::<Ran>() {
        *ran.0.lock().await = true;
    }
    Ok(a + b)
}

/// An app-shaped config against the fake gateway: tiny timeouts, host
/// dialer, and the one rig dependency the app's manifest names.
#[cfg(test)]
fn fixture_config(url: &url::Url) -> LlmConfig {
    LlmConfig::new(url.clone())
        .app_name("cja-llm-app-fixture")
        .connect_timeout(Duration::from_millis(250))
        .request_timeout(Duration::from_secs(2))
        .retries(RetryPolicy {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(10),
            ..RetryPolicy::default()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use cja_llm::Llm;
    use cja_llm::models;
    use cja_llm::testing::FakeGateway;

    /// A full agent tool round-trip from an app's dependency shape: the
    /// macro tool registers on cja-llm's agent, the model's tool call
    /// executes on the host, and the follow-up turn carries the result.
    #[tokio::test]
    async fn tool_round_trips_from_an_app_dependency_shape() {
        let fake = FakeGateway::start().await;
        fake.tool_call(
            models::DEFAULT,
            "call-1",
            "add",
            &serde_json::json!({"a": 2, "b": 3}),
        );
        fake.chat_completion(models::DEFAULT, "5");

        let llm = Llm::new(&fixture_config(&fake.url())).unwrap();
        let ran = Arc::new(Ran(tokio::sync::Mutex::new(false)));
        let reply = llm
            .agent(models::DEFAULT)
            .tool(Add)
            .build()
            .prompt("What is 2 + 3?")
            .tool_context(ToolContext::default().with_scope(ran.clone()))
            .await
            .unwrap();

        assert_eq!(reply.output(), "5");
        assert!(*ran.0.lock().await, "the tool ran on the host");
        let requests = fake.requests();
        assert_eq!(requests.len(), 2, "tool call, then the follow-up turn");
    }
}
