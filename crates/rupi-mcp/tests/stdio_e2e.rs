//! End-to-end: spawn a real stdio MCP child and list/call tools.
use std::path::PathBuf;

use rupi_mcp::{McpClient, StdioTransport};

fn mock_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mock_mcp.py")
}

#[tokio::test]
async fn stdio_python_mock_list_and_call() {
    let python = which_python();
    let script = mock_script();
    assert!(script.exists(), "missing {script:?}");

    let transport = StdioTransport::spawn(
        &python,
        &[script.display().to_string()],
        &[],
    )
    .await
    .expect("spawn python mock MCP");
    let mut client = McpClient::new("mock-stdio", Box::new(transport));
    client
        .initialize(vec!["/tmp".into()])
        .await
        .expect("initialize");

    let tools = client.list_tools().await.expect("list");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");

    let result = client
        .call_tool("echo", serde_json::json!({"text": "hello-mcp"}))
        .await
        .expect("call");
    assert!(!result.is_error);
    assert_eq!(result.text(), "hello-mcp");

    let _ = client.close().await;
}

fn which_python() -> String {
    for cand in ["python3", "python"] {
        if std::process::Command::new(cand)
            .arg("-c")
            .arg("import sys; sys.exit(0)")
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            return cand.to_string();
        }
    }
    panic!("python3 not available for MCP stdio e2e");
}
