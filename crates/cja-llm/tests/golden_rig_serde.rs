//! Golden fixtures for rig 0.45.0's `Message` and suspended `AgentRun`
//! serialization: the shapes cja-llm's durable runs store in
//! `agent_messages.message` and `agent_runs.run_state`.
//!
//! A rig bump that changes either serialization fails here instead of
//! silently breaking stored transcripts. The fixtures are produced by
//! serializing real types (see `regenerate_fixtures`, `#[ignore]`d);
//! regenerate deliberately — `cargo test -p cja-llm --test golden_rig_serde
//! --features durable,testing -- --ignored` — and never edit them by hand.

use std::collections::BTreeSet;

use cja_llm::rig::rig_agent::run::{
    AgentRun, AgentRunStep, ModelTurn, PendingToolCall, TurnPolicy,
};
use cja_llm::rig::rig_core::completion::Usage;
use cja_llm::rig::rig_core::completion::message::{
    AssistantContent, AssistantMessage, CallId, Message, ToolCall, ToolFunction, ToolName,
};
use cja_llm::rig::rig_core::tool::{ToolOutput, ToolResult};
use serde_json::json;

fn add_function(args: serde_json::Value) -> ToolFunction {
    ToolFunction::new(ToolName::new("add".to_owned()).expect("tool name"), args)
}

fn golden_tool_call() -> AssistantContent {
    AssistantContent::ToolCall(ToolCall::from_wire(
        "call_golden",
        add_function(json!({"a": 1, "b": 2})),
    ))
}

/// The four message shapes a durable transcript row can hold.
fn expected_messages() -> Vec<(&'static str, Message)> {
    vec![
        ("user", Message::user("golden user prompt")),
        (
            "assistant",
            Message::Assistant(AssistantMessage::new(vec![AssistantContent::text(
                "golden reply",
            )])),
        ),
        (
            "assistant",
            Message::Assistant(AssistantMessage::rolled_back(
                None,
                vec![golden_tool_call()],
            )),
        ),
        (
            "user",
            Message::tool_result(
                CallId::from_wire("call_golden"),
                ToolName::new("add".to_owned()).expect("tool name"),
                "golden result",
            ),
        ),
    ]
}

fn messages_fixture_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rig-0.45.0-messages.json")
}

fn run_fixture_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/rig-0.45.0-suspended-run.json")
}

/// Build a run suspended mid-`ExecutingTools` with one pending `add` call,
/// mirroring upstream `run/tests.rs`'s suspended-state test: one model turn
/// carrying a tool call, never answered.
fn suspended_run() -> AgentRun {
    let mut run = AgentRun::new(Message::user("golden run prompt")).max_turns(4);
    // -> CallModel
    assert!(matches!(
        run.next_step().expect("first step"),
        AgentRunStep::CallModel { .. }
    ));
    let policy = TurnPolicy::new(BTreeSet::from(["add".to_owned()]), None, None).expect("policy");
    let turn = ModelTurn::new(
        AssistantMessage::default(),
        vec![golden_tool_call()],
        Usage::new()
            .input_tokens(3u64)
            .output_tokens(2u64)
            .total_tokens(5u64),
        policy,
        json!({"origin": "golden fixture"}),
    );
    let outcome = run.model_response(turn).expect("model_response");
    assert!(
        matches!(
            outcome,
            cja_llm::rig::rig_agent::run::ModelTurnOutcome::Continue { .. }
        ),
        "hand-built turns continue: {outcome:?}"
    );
    // Suspended here: the batch is pending, never answered.
    run
}

/// Serialize the real types to the fixture files. `#[ignore]`d: run only to
/// regenerate, after a deliberate rig-compatible change.
#[test]
#[ignore = "regeneration only: cargo test --test golden_rig_serde -- --ignored"]
fn regenerate_fixtures() {
    let messages: Vec<serde_json::Value> = expected_messages()
        .into_iter()
        .map(|(role, message)| {
            json!({
                "role": role,
                "json": serde_json::to_value(&message).expect("serialize message"),
            })
        })
        .collect();
    std::fs::write(
        messages_fixture_path(),
        serde_json::to_string_pretty(&messages).expect("pretty messages"),
    )
    .expect("write messages fixture");

    let run = serde_json::to_string_pretty(&suspended_run()).expect("serialize run");
    std::fs::write(run_fixture_path(), run).expect("write run fixture");
}

fn load_messages_fixture() -> serde_json::Value {
    let text = std::fs::read_to_string(messages_fixture_path()).expect("messages fixture");
    serde_json::from_str(&text).expect("fixture is JSON")
}

#[test]
fn stored_messages_deserialize_to_expected_values() {
    let fixture = load_messages_fixture();
    let entries = fixture.as_array().expect("fixture is an array");
    let expected = expected_messages();
    assert_eq!(entries.len(), expected.len(), "one entry per shape");

    for (entry, (role, want)) in entries.iter().zip(&expected) {
        assert_eq!(entry["role"].as_str(), Some(*role), "role matches");
        let got: Message = serde_json::from_value(entry["json"].clone())
            .expect("stored message shape still deserializes");
        assert_eq!(&got, want, "deserializes to the constructed value");
    }

    // Mutation sensitivity: a perturbed entry must NOT match its expected
    // value — this is what makes the equality above evidence rather than a
    // vacuous pass (a loader bug that never read the file would otherwise
    // deserialize nothing and compare nothing).
    let mut perturbed = fixture.clone();
    perturbed[0]["json"]["content"][0]["text"] = json!("PERTURBED");
    let got: Message =
        serde_json::from_value(perturbed[0]["json"].clone()).expect("perturbed still parses");
    assert_ne!(got, expected[0].1, "perturbation is detected");
}

#[test]
fn suspended_run_restores_and_re_emits_its_pending_call() {
    let text = std::fs::read_to_string(run_fixture_path()).expect("run fixture");
    let mut restored: AgentRun =
        serde_json::from_str(&text).expect("suspended run shape still deserializes");

    // Mid-ExecutingTools: the pending call re-emits.
    let calls = match restored.next_step().expect("restore step") {
        AgentRunStep::CallTools { calls } => calls,
        step => panic!("expected restored CallTools, got {step:?}"),
    };
    assert_eq!(calls.len(), 1);
    let PendingToolCall::Execute(exec) = calls.into_iter().next().expect("one call") else {
        panic!("expected an executable call");
    };
    assert_eq!(exec.id().wire(), "call_golden");
    assert_eq!(exec.name().as_str(), "add");

    // Answering it advances to the next model call: the run is usable.
    restored
        .answer(exec.answer(ToolResult::success(ToolOutput::text("golden result"))))
        .expect("answer accepted");
    assert!(matches!(
        restored.next_step().expect("post-answer step"),
        AgentRunStep::CallModel { .. }
    ));

    // Version-skew guard: the RUN_FORMAT envelope must be load-bearing — a
    // perturbed format value refuses to deserialize instead of corrupting.
    let mut skewed: serde_json::Value =
        serde_json::from_str(&text).expect("fixture parses as JSON");
    skewed["format"] = json!(9999);
    let serialized = serde_json::to_string(&skewed).expect("reserialize");
    assert!(
        serde_json::from_str::<AgentRun>(&serialized).is_err(),
        "a RUN_FORMAT mismatch must refuse loudly"
    );
}
