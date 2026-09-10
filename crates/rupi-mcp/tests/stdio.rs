use rupi_mcp::{mcp_tools_from_manager, McpManager, McpServerConfig, McpServersFile};
use rupi_agent::ToolContext;
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

fn echo_server() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/echo_server.py")
}

#[tokio::test]
async fn stdio_mcp_initialize_list_and_call() {
    let mut servers = HashMap::new();
    servers.insert(
        "echo".into(),
        McpServerConfig {
            command: Some("python3".into()),
            args: vec![echo_server().display().to_string()],
            env: HashMap::new(),
            url: None,
            headers: HashMap::new(),
            direct_tools: Some(true),
            disabled: false,
        },
    );
    let file = McpServersFile {
        mcp_servers: servers,
    };
    let mgr = Arc::new(McpManager::connect_all(&file).await);
    let status = mgr.status_text().await;
    assert!(status.contains("echo"), "{status}");
    let tools = mgr.all_tools().await;
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");

    let (client, _) = mgr.find_tool("echo", "echo").await.expect("tool");
    let result = client
        .call_tool("echo", json!({"text": "hello-mcp"}))
        .await
        .unwrap();
    let text = result["content"][0]["text"].as_str().unwrap();
    assert_eq!(text, "hello-mcp");

    let native = mcp_tools_from_manager(mgr, true).await;
    let names: Vec<_> = native.iter().map(|t| t.name().to_string()).collect();
    assert!(names.contains(&"mcp".to_string()));
    assert!(names.iter().any(|n| n.starts_with("mcp_echo_")));
    let direct = native
        .iter()
        .find(|t| t.name().starts_with("mcp_echo_"))
        .unwrap();
    let ctx = ToolContext::new(".");
    let out = direct
        .execute(json!({"text": "via-direct"}), &ctx)
        .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("via-direct"));
}
