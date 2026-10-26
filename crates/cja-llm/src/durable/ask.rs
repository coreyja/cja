//! The built-in human-in-the-loop suspension tool.

use rig_core::tool::{Tool, ToolContext, ToolOutput};

/// Ask the human a question and wait for their answer. Registered on a kind
/// with `.tool(durable::Ask)`; the durable worker intercepts the call before
/// execution, so the tool body never runs on the durable path — it suspends
/// the run (`status = waiting`) until [`answer`](super::answer) supplies this
/// call's result.
///
/// A hand-written `impl Tool` (not `#[rig_tool]`): the derive's proc-macro
/// crate resolution requires apps to name `rig-agent` in their own manifest,
/// which the library itself should not impose.
pub struct Ask;

/// The ask arguments, parsed at suspension so `pending_question` is a real
/// TEXT column. The worker parses the call's arguments into this type to
/// decide suspension vs. corrective failure.
#[derive(Debug, serde::Deserialize)]
pub struct AskArgs {
    /// The question to put to the human.
    pub question: String,
}

impl Tool for Ask {
    const NAME: &'static str = "ask";
    type Args = AskArgs;
    type Output = ToolOutput;
    type Error = std::convert::Infallible;

    fn description(&self) -> String {
        "Ask the human a question and wait for their answer".into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"question": {"type": "string"}},
            "required": ["question"],
        })
    }

    fn call(
        &self,
        _context: &mut ToolContext,
        _args: Self::Args,
    ) -> impl Future<Output = Result<Self::Output, Self::Error>> + Send {
        // NOT unreachable!(): a panic here would be caught by cja's worker
        // and retried into dead-letter on a permanent condition, orphaning
        // the run. If the interceptor were ever bypassed, answer inline and
        // let the run continue instead.
        std::future::ready(Ok(ToolOutput::text(
            "`ask` must be intercepted by the durable worker; it never runs inline",
        )))
    }
}
