//! Agent ↔ 外部扩展联调：剧本让模型调用热加载的工具，主循环执行并回填结果。

use rupi_agent::AgentLoop;
use rupi_core::{CancelFlag, ContentBlock, Message, Role, SessionTree, StopReason};
use rupi_llm::{ChatResponse, MockProvider};
use rupi_memory::{FrozenMemory, MemoryManager, MemoryStore};
use rupi_skills::SkillRegistry;
use rupi_tools::ToolRegistry;

fn tool_call_response() -> ChatResponse {
    ChatResponse {
        message: Message {
            id: "assistant-1".into(),
            role: Role::Assistant,
            blocks: vec![ContentBlock::ToolCall {
                id: "c1".into(),
                name: "upper".into(),
                arguments: serde_json::json!({"text": "hello ext"}),
            }],
            provider: Some("mock".into()),
            created_at: chrono::Utc::now(),
        },
        stop_reason: "tool_calls".into(),
    }
}

#[tokio::test]
async fn agent_executes_hot_loaded_extension_tool() {
    let dir = std::env::temp_dir().join(format!("rupi-ext-agent-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("upper.json"),
        r#"{"name":"upper","description":"up","input_schema":{"type":"object"},
            "command":"sh","args":["-c","tr a-z A-Z"]}"#,
    )
    .unwrap();

    let mut set = rupi_ext::ExtensionSet::new(dir.clone());
    let mut tools = ToolRegistry::with_builtins();
    rupi_ext::register_all(&mut tools, set.load_all());
    assert!(tools.definitions().iter().any(|d| d.name == "upper"));

    // 剧本：先调 upper，再纯文本收尾
    let provider = MockProvider::new(vec![
        tool_call_response(),
        MockProvider::text_response("done"),
    ]);
    let agent = AgentLoop::new(5);
    let mut session = SessionTree::new();
    let mem = MemoryManager::new(MemoryStore::new(std::env::temp_dir().join("rupi-ext-mem")));
    let reason = agent
        .run(
            &provider,
            &mut session,
            "uppercase it",
            &tools,
            &mem,
            &FrozenMemory::default(),
            &SkillRegistry::default(),
            &[],
            &|_| {},
            &CancelFlag::new(),
        )
        .await
        .unwrap();
    assert!(matches!(reason, StopReason::Done));
    // tool 结果（转大写后的 arguments JSON）已回填进会话
    let all: String = session
        .history()
        .iter()
        .map(|m| m.full_text())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(all.contains("HELLO EXT"));

    // 热重载：删掉扩展后工具消失
    std::fs::remove_file(dir.join("upper.json")).unwrap();
    let (changed, removed) = set.refresh();
    assert!(changed.is_empty());
    assert_eq!(removed, vec!["upper".to_string()]);
    for name in removed {
        tools.unregister(&name);
    }
    assert!(!tools.definitions().iter().any(|d| d.name == "upper"));
    let _ = std::fs::remove_dir_all(&dir);
}
