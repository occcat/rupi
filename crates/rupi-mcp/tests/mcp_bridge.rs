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
    assert_eq!(tools.len(), 3);
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
    let names = manager.register_all(&mut registry).await;
    assert!(names.contains(&"fake_echo".to_string()));
    assert!(names.contains(&"fake_fail".to_string()));

    // 注册后的原生工具可直接执行，且 prompt_snippet 必填
    let defs = registry.definitions();
    let echo_def = defs.iter().find(|d| d.name == "fake_echo").unwrap();
    assert!(echo_def.prompt_snippet.is_some());

    let tool: Arc<dyn Tool> = Arc::new(rupi_mcp::McpToolExecutor::new(
        "fake",
        manager.entries[0].bridge.clone(),
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

#[tokio::test]
async fn bridge_answers_roots_list_requests() {
    let bridge = rupi_mcp::McpBridge::spawn(fake_config())
        .await
        .expect("spawn");
    assert!(!bridge.roots.is_empty());
    assert!(bridge.roots[0].uri.starts_with("file://"));
    // fake server 在 initialized 后反向请求 roots/list，轮询 probe 直到桥应答到达
    let probe = bridge
        .list_tools()
        .await
        .expect("list")
        .into_iter()
        .find(|t| t.name == "roots_probe")
        .expect("roots_probe tool");
    let mut seen = String::new();
    for _ in 0..50 {
        let r = bridge
            .call_tool("roots_probe", serde_json::json!({}), &probe.input_schema)
            .await
            .expect("call");
        if r.text != "null" {
            seen = r.text;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(seen.contains("file://"), "server saw roots: {seen}");
}

#[tokio::test]
async fn manager_skips_dead_servers_without_misalignment() {
    let bad = McpServerConfig::new("dead", "definitely-not-a-real-binary-xyz", vec![]);
    let configs = vec![bad, fake_config()];
    let manager = McpManager::spawn_all(&configs).await.expect("spawn all");
    // 死 server 只跳过自己：存活项仍与自己的 config 配对（zip 错位回归）
    assert_eq!(manager.entries.len(), 1);
    assert_eq!(manager.entries[0].config.name, "fake");
    let mut registry = ToolRegistry::with_builtins();
    let names = manager.register_all(&mut registry).await;
    assert!(names.iter().all(|n| n.starts_with("fake_")));
    assert!(names.contains(&"fake_echo".to_string()));
}

#[test]
fn server_request_response_covers_roots_ping_unknown() {
    let roots = vec![rupi_mcp::McpRoot {
        uri: "file:///proj".into(),
        name: "proj".into(),
    }];
    let r = rupi_mcp::server_request_response("roots/list", Some(7), &roots).unwrap();
    assert_eq!(r["id"], serde_json::json!(7));
    assert_eq!(r["result"]["roots"][0]["uri"], serde_json::json!("file:///proj"));
    let p = rupi_mcp::server_request_response("ping", Some(8), &roots).unwrap();
    assert_eq!(p["result"], serde_json::json!({}));
    let e = rupi_mcp::server_request_response("sampling/createMessage", Some(9), &roots).unwrap();
    assert_eq!(e["error"]["code"], serde_json::json!(-32601));
    assert!(rupi_mcp::server_request_response("ping", None, &roots).is_none());
}
