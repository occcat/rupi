use std::sync::Arc;

use async_trait::async_trait;
use rupi_agent_core::{AgentTool, AgentToolResult};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::client::McpClient;

/// Bridges one MCP tool into a Pi `AgentTool`. Names are prefixed `mcp_<server>_<tool>`
/// matching pi-mcp-extension's default accessibility prefix.
pub struct McpToolBridge {
    pub server: String,
    pub tool_name: String,
    pub description: String,
    pub parameters: Value,
    client: Arc<Mutex<McpClient>>,
}

impl McpToolBridge {
    pub fn new(
        server: impl Into<String>,
        tool_name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        client: Arc<Mutex<McpClient>>,
    ) -> Self {
        Self {
            server: server.into(),
            tool_name: tool_name.into(),
            description: description.into(),
            parameters,
            client,
        }
    }

    pub fn prefixed_name(&self) -> String {
        format!("mcp_{}_{}", sanitize(&self.server), sanitize(&self.tool_name))
    }
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

#[async_trait]
impl AgentTool for McpToolBridge {
    fn name(&self) -> &str {
        // AgentTool requires &str; store prefixed name would need ownership.
        // Use tool_name for the MCP call; the ToolSet key is registered separately.
        &self.tool_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        self.parameters.clone()
    }

    fn label(&self) -> &str {
        &self.tool_name
    }

    async fn execute(&self, _tool_call_id: &str, args: Value) -> AgentToolResult {
        let mut client = self.client.lock().await;
        match client.call_tool(&self.tool_name, args).await {
            Ok(result) => {
                if result.is_error {
                    AgentToolResult::err(result.text())
                } else {
                    AgentToolResult::ok(result.text())
                }
            }
            Err(e) => AgentToolResult::err(e.to_string()),
        }
    }
}

/// Wrapper that reports a prefixed name to the LLM.
pub struct PrefixedTool {
    pub prefix_name: String,
    inner: McpToolBridge,
}

impl PrefixedTool {
    pub fn new(bridge: McpToolBridge) -> Self {
        Self {
            prefix_name: bridge.prefixed_name(),
            inner: bridge,
        }
    }
}

#[async_trait]
impl AgentTool for PrefixedTool {
    fn name(&self) -> &str {
        &self.prefix_name
    }
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn parameters(&self) -> Value {
        self.inner.parameters()
    }
    async fn execute(&self, tool_call_id: &str, args: Value) -> AgentToolResult {
        self.inner.execute(tool_call_id, args).await
    }
}
