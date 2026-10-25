//! Named gateway model constants, in Bifrost's `provider/model` form.
//!
//! The gateway splits a model string on the first `/` only when the prefix is
//! a known provider (`openai`, `anthropic`, `sakana`); a bare model name falls
//! back to Bifrost's model-catalog resolver, which can fail. The constants
//! here are therefore always qualified, and free-form strings passed by
//! callers are their own responsibility.
//!
//! Any gateway model string is accepted by every API in this crate; these
//! constants are sugar, not a limit.

/// `Anthropic`'s Claude Haiku 5.5 — the house default model.
pub const ANTHROPIC_CLAUDE_HAIKU_5_5: &str = "anthropic/claude-haiku-5-5";
/// `OpenAI`'s `GPT-5` mini.
pub const OPENAI_GPT_5_MINI: &str = "openai/gpt-5-mini";
/// Sakana's Fugu Ultra. The gateway's Sakana subscription may lapse; live
/// `sakana/*` calls can `429 usage_limit_reached`, which is upstream behavior,
/// not a cja-llm bug.
pub const SAKANA_FUGU_ULTRA: &str = "sakana/fugu-ultra";
/// The model [`crate::Llm::agent`] callers get when they name no other.
pub const DEFAULT: &str = ANTHROPIC_CLAUDE_HAIKU_5_5;
