//! 真实子进程联调：spawn fake MCP server → tools/list → register → tools/call。

use rupi_mcp::{McpManager, McpServerConfig};
use rupi_tools::{Tool, ToolRegistry};
use std::sync::Arc;

fn fake_config() -> McpServerConfig {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake_mcp_server.py");
    McpServerConfig::new("fake", "python3", vec![script.to_string()])
}

#[tokio::test]
async fn bridge_lists_and_calls_tools() {
    let bridge = rupi_mcp::McpBridge::spawn(fake_config())
        .await
        .expect("spawn");
    let tools = bridge.list_tools().await.expect("list");
    assert_eq!(tools.len(), 2);
    let echo = tools.iter().find(|t| t.name == "echo").unwrap();

    // string "true" 应被 sanitize 为 boolean true → shout 生效
    let r = bridge
        .call_tool(
            "echo",
            serde_json::json!({"text": "hi", "shout": "true"}),
            &echo.input_schema,
        )
        .await
        .expect("call");
    assert!(!r.is_error);
    assert_eq!(r.text, "HI");

    // tool 执行失败走 isError 结果，而非协议 error
    let fail = tools.iter().find(|t| t.name == "fail").unwrap();
    let r = bridge
        .call_tool("fail", serde_json::json!({}), &fail.input_schema)
        .await
        .expect("call");
    assert!(r.is_error);
    assert_eq!(r.text, "boom");
}

#[tokio::test]
async fn manager_registers_remote_tools_as_native() {
    let configs = vec![fake_config()];
    let manager = McpManager::spawn_all(&configs).await.expect("spawn all");
    let mut registry = ToolRegistry::with_builtins();
    let names = manager.register_all(&mut registry, &configs).await;
    assert!(names.contains(&"fake_echo".to_string()));
    assert!(names.contains(&"fake_fail".to_string()));

    // 注册后的原生工具可直接执行，且 prompt_snippet 必填
    let defs = registry.definitions();
    let echo_def = defs.iter().find(|d| d.name == "fake_echo").unwrap();
    assert!(echo_def.prompt_snippet.is_some());

    let tool: Arc<dyn Tool> = Arc::new(rupi_mcp::McpToolExecutor::new(
        "fake",
        manager.bridges[0].clone(),
        &rupi_mcp::McpTool {
            name: "echo".into(),
            description: "echo".into(),
            input_schema: serde_json::json!({"type":"object","properties":{"text":{"type":"string"}}}),
        },
    ));
    let out = tool
        .execute(serde_json::json!({"text": "yo"}))
        .await
        .expect("exec");
    assert!(!out.is_error);
    assert_eq!(out.content, "yo");
}
