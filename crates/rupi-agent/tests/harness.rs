use rupi_agent::{
    builtin_tools, run_agent_loop, AgentEvent, AgentLoopConfig, SessionManager, ToolContext,
    ToolRegistry, ToolResult,
};
use rupi_ai::{FauxProvider, Message, Model, ProviderKind, ScriptedTurn};
use serde_json::json;
use std::sync::Arc;
use tempfile::tempdir;

#[tokio::test]
async fn agent_loop_executes_write_then_stops() {
    let dir = tempdir().unwrap();
    let provider = Arc::new(FauxProvider::new(vec![
        ScriptedTurn::tool(
            "write",
            json!({"path": "hello.txt", "content": "hi from rupi"}),
        ),
        ScriptedTurn::text("Wrote the file."),
    ]));
    let tools = ToolRegistry::new(builtin_tools(&[]));
    let model = Model::new("faux", ProviderKind::Faux);
    let config = AgentLoopConfig::new(
        model,
        provider,
        tools,
        "test".into(),
        dir.path().to_path_buf(),
    );
    let events = std::sync::Mutex::new(Vec::new());
    let msgs = run_agent_loop(
        vec![Message::user_text("write hello.txt")],
        vec![],
        &config,
        |e| events.lock().unwrap().push(format_event(&e)),
    )
    .await;
    let path = dir.path().join("hello.txt");
    let body = std::fs::read_to_string(&path).unwrap();
    assert_eq!(body, "hi from rupi");
    assert!(msgs.iter().any(|m| matches!(m, Message::Assistant(_))));
    let ev = events.lock().unwrap();
    assert!(ev.iter().any(|e| e == "agent_start"));
    assert!(ev.iter().any(|e| e.starts_with("tool_end:write")));
}

#[tokio::test]
async fn read_write_edit_roundtrip() {
    let dir = tempdir().unwrap();
    let ctx = ToolContext::new(dir.path());
    let write = rupi_agent::create_tool(rupi_agent::ToolName::Write);
    let edit = rupi_agent::create_tool(rupi_agent::ToolName::Edit);
    let read = rupi_agent::create_tool(rupi_agent::ToolName::Read);
    write
        .execute(
            json!({"path": "a.txt", "content": "alpha\nbeta\ngamma\n"}),
            &ctx,
        )
        .await;
    let edited = edit
        .execute(
            json!({
                "path": "a.txt",
                "edits": [{"oldText": "beta", "newText": "BETA"}]
            }),
            &ctx,
        )
        .await;
    assert!(!edited.is_error, "{}", edited.content);
    let shown = read
        .execute(json!({"path": "a.txt"}), &ctx)
        .await;
    assert!(shown.content.contains("BETA"));
}

#[tokio::test]
async fn bash_echo_and_grep() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("n.rs"), "fn main() { println!(\"x\"); }\n").unwrap();
    let ctx = ToolContext::new(dir.path());
    let bash = rupi_agent::create_tool(rupi_agent::ToolName::Bash);
    let out = bash
        .execute(json!({"command": "echo hello-rupi"}), &ctx)
        .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("hello-rupi"));
    let grep = rupi_agent::create_tool(rupi_agent::ToolName::Grep);
    let hits = grep
        .execute(json!({"pattern": "println", "path": "."}), &ctx)
        .await;
    assert!(hits.content.contains("n.rs"));
}

#[tokio::test]
async fn session_jsonl_roundtrip() {
    let dir = tempdir().unwrap();
    let mgr = SessionManager::new(dir.path());
    let mut session = mgr.create(dir.path(), Some("demo".into())).await.unwrap();
    mgr.append(&mut session, Message::user_text("hi"))
        .await
        .unwrap();
    let loaded = mgr.load(&session.path).await.unwrap();
    assert_eq!(loaded.header.id, session.header.id);
    assert_eq!(loaded.messages().len(), 1);
}

fn format_event(e: &AgentEvent) -> String {
    match e {
        AgentEvent::AgentStart => "agent_start".into(),
        AgentEvent::TurnStart => "turn_start".into(),
        AgentEvent::ToolExecutionEnd { name, .. } => format!("tool_end:{name}"),
        AgentEvent::AgentEnd { .. } => "agent_end".into(),
        _ => "other".into(),
    }
}

#[allow(dead_code)]
fn _use_tool_result(r: ToolResult) -> bool {
    !r.is_error
}
