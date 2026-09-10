use rupi_mcp::{
    InMemoryTransport, McpClient, ToolAnnotation, ToolSpec, PROTOCOL_VERSION,
};

#[tokio::test]
async fn initialize_and_list_and_call() {
    let spec = ToolSpec {
        name: "sum".into(),
        description: "add".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {"a": {"type": "number"}}
        }),
        annotations: Some(ToolAnnotation {
            read_only_hint: Some(true),
            ..Default::default()
        }),
    };
    assert!(spec.annotated_description().contains("readOnly"));
    let transport = InMemoryTransport::echo_tools(vec![spec]);
    let mut client = McpClient::new("echo", Box::new(transport));
    let init = client.initialize(vec!["/tmp".into()]).await.unwrap();
    assert_eq!(init["protocolVersion"], PROTOCOL_VERSION);
    let tools = client.list_tools().await.unwrap();
    assert_eq!(tools.len(), 1);
    let result = client
        .call_tool("sum", serde_json::json!({"a": 1}))
        .await
        .unwrap();
    assert!(!result.is_error);
    assert!(result.text().contains("sum"));
}

#[test]
fn layered_mcp_config_project_overrides() {
    let global = r#"{"mcpServers":{"a":{"command":"npx","args":["-y","a"]},"b":{"url":"http://localhost/sse","type":"sse"}}}"#;
    let project = r#"{"mcpServers":{"a":{"command":"uvx","args":["a"]}}}"#;
    let cfgs = rupi_mcp::load_mcp_configs(Some(global), Some(project));
    let a = cfgs.iter().find(|c| c.name == "a").unwrap();
    assert_eq!(a.command.as_deref(), Some("uvx"));
    assert!(cfgs.iter().any(|c| c.name == "b"));
}
