//! One agent, one tool, one prompt — against the live tailnet gateway.
//!
//! Compiled by CI, never executed there. Run it by hand on a tailnet host
//! (byte-vm, attic) with a policy grant to the gateway:
//!
//! ```sh
//! cargo run -p cja-llm --example llm_hello
//! ```

use cja_llm::models;
use cja_llm::rig::rig_agent::rig_tool;
use cja_llm::rig::rig_core::tool::ToolExecutionError;

/// Report the current time, so the model can use it.
#[rig_tool(description = "The current local time, as a string")]
async fn now() -> Result<String, ToolExecutionError> {
    // A tiny async touch (a lock would do the same in a real tool) so the
    // generated async trait impl genuinely awaits.
    tokio::task::yield_now().await;
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| format!("{} seconds since the Unix epoch", elapsed.as_secs()))
        .map_err(|e| ToolExecutionError::other(e.to_string()))
}

#[tokio::main]
async fn main() -> cja_llm::Result<()> {
    let llm = cja_llm::Llm::from_env()?;
    let reply = llm
        .agent(models::DEFAULT)
        .preamble("You are a concise assistant. Use the now tool before answering time questions.")
        .tool(Now)
        .build()
        .prompt("What time is it? Answer in one sentence.")
        .await?;
    println!("{}", reply.output());
    Ok(())
}
