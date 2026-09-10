use std::sync::Arc;

use rupi_agent_core::{
    agent_loop, compact_messages, estimate_context_tokens, find_cut_point, format_skills_for_system_prompt,
    should_compact, Agent, AgentContext, AgentLoopConfig, CompactionSettings, PermissionDecision,
    PermissionGate, SessionStore, SkillPromptEntry, ToolSet,
};
use rupi_ai::{FauxProvider, FauxScript, Message, ModelCatalog};

use async_trait::async_trait;
use rupi_agent_core::{AgentTool, AgentToolResult};
use serde_json::Value;

struct EchoTool;

#[async_trait]
impl AgentTool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "echo args"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]})
    }
    async fn execute(&self, _id: &str, args: Value) -> AgentToolResult {
        AgentToolResult::ok(args["text"].as_str().unwrap_or(""))
    }
}

fn faux_model() -> rupi_ai::Model {
    ModelCatalog::builtin().resolve("faux/faux").unwrap()
}

#[tokio::test]
async fn loop_text_then_stop() {
    let provider = Arc::new(FauxProvider::new([FauxScript::text("done")]));
    let mut agent = Agent::new("sys", faux_model()).with_faux(provider);
    let msgs = agent.prompt("hello").await.unwrap();
    assert!(matches!(&msgs[0], Message::User { .. }));
    assert!(matches!(&msgs[1], Message::Assistant { .. }));
    let kinds = agent.events().iter().map(|e| e.kind()).collect::<Vec<_>>();
    assert_eq!(kinds.first().copied(), Some("agent_start"));
    assert_eq!(kinds.last().copied(), Some("agent_end"));
    assert!(kinds.contains(&"turn_start"));
    assert!(kinds.contains(&"turn_end"));
}

#[tokio::test]
async fn loop_tool_then_final() {
    let provider = Arc::new(FauxProvider::new([
        FauxScript::tool("echo", serde_json::json!({"text": "pong"})),
        FauxScript::text("finished"),
    ]));
    let mut tools = ToolSet::new();
    tools.register(Arc::new(EchoTool));
    let mut agent = Agent::new("sys", faux_model()).with_faux(provider);
    agent.set_tools(tools);
    let msgs = agent.prompt("ping").await.unwrap();
    assert!(msgs.iter().any(|m| matches!(m, Message::ToolResult { .. })));
    let last = msgs.last().unwrap();
    match last {
        Message::Assistant { content, .. } => {
            assert!(rupi_ai::content_text(content).contains("finished"));
        }
        _ => panic!("expected assistant"),
    }
}

#[tokio::test]
async fn continue_rejects_assistant_last() {
    let provider = Arc::new(FauxProvider::new([FauxScript::text("x")]));
    let mut agent = Agent::new("sys", faux_model()).with_faux(provider);
    agent.prompt("hi").await.unwrap();
    let err = agent.continue_run().await.unwrap_err();
    assert!(err.to_string().contains("assistant"));
}

#[tokio::test]
async fn follow_up_extends_loop() {
    let provider = Arc::new(FauxProvider::new([
        FauxScript::text("first"),
        FauxScript::text("second"),
    ]));
    let mut agent = Agent::new("sys", faux_model()).with_faux(provider);
    agent.follow_up(Message::user("more"));
    let msgs = agent.prompt("start").await.unwrap();
    let assistants: Vec<_> = msgs
        .iter()
        .filter(|m| m.is_assistant())
        .collect();
    assert_eq!(assistants.len(), 2);
}

#[test]
fn compaction_cut_and_should() {
    let mut msgs = Vec::new();
    for i in 0..50 {
        msgs.push(Message::user("word ".repeat(200) + &i.to_string()));
        msgs.push(Message::assistant_text("ok ".repeat(200)));
    }
    let settings = CompactionSettings {
        context_window: 1000,
        reserve_tokens: 100,
        keep_recent_tokens: 200,
    };
    assert!(should_compact(&msgs, &settings));
    let cut = find_cut_point(&msgs, 200);
    assert!(cut > 0);
    let compacted = compact_messages(&msgs, &settings);
    assert!(compacted.len() < msgs.len());
    assert!(estimate_context_tokens(&compacted) < estimate_context_tokens(&msgs));
}

#[test]
fn session_leaf_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = SessionStore::create(dir.path(), "/tmp", "faux/faux").unwrap();
    store.append_message(Message::user("a")).unwrap();
    store.append_message(Message::assistant_text("b")).unwrap();
    let path = store.path.clone();
    let leaf = store.leaf_id.clone();
    drop(store);
    let reopened = SessionStore::open(&path).unwrap();
    assert_eq!(reopened.leaf_id, leaf);
    assert_eq!(reopened.active_messages().len(), 2);
}

#[test]
fn skills_xml_matches_pi_shape() {
    let xml = format_skills_for_system_prompt(&[SkillPromptEntry {
        name: "foo".into(),
        description: "bar <baz>".into(),
        location: "/tmp/SKILL.md".into(),
        disable_model_invocation: false,
    }]);
    assert!(xml.contains("<available_skills>"));
    assert!(xml.contains("<name>foo</name>"));
    assert!(xml.contains("&lt;baz&gt;"));
}

#[tokio::test]
async fn permission_block_emits_error_result() {
    let provider = Arc::new(FauxProvider::new([
        FauxScript::tool("echo", serde_json::json!({"text": "x"})),
        FauxScript::text("ok"),
    ]));
    let mut tools = ToolSet::new();
    tools.register(Arc::new(EchoTool));
    let mut cfg = AgentLoopConfig::faux(faux_model(), provider, tools);
    cfg.before_tool_call = Some(Box::new(|_| rupi_agent_core::BeforeToolCallResult {
        block: true,
        reason: Some("nope".into()),
        terminate: false,
    }));
    let mut ctx = AgentContext::new("sys");
    let sink = rupi_agent_core::Agent::silent_sink();
    let msgs = agent_loop(vec![Message::user("go")], &mut ctx, &cfg, sink)
        .await
        .unwrap();
    assert!(msgs.iter().any(|m| matches!(
        m,
        Message::ToolResult { is_error: true, .. }
    )));
}

#[test]
fn sandbox_gate_asks_destructive_bash() {
    let g = PermissionGate::sandboxed("/tmp");
    assert_eq!(g.decide("bash", "rm -rf /"), PermissionDecision::Ask);
    assert_eq!(g.decide("bash", "ls -la"), PermissionDecision::Allow);
    assert_eq!(g.decide("read", "README.md"), PermissionDecision::Allow);
    assert_eq!(g.decide("session_search", "q"), PermissionDecision::Allow);
}
