use crate::client::McpManager;
use async_trait::async_trait;
use rupi_agent::{Tool, ToolContext, ToolResult};
use serde_json::{json, Value};
use std::sync::Arc;

/// Proxy tool: search/describe/call MCP servers without exploding the tool list.
pub struct McpProxyTool {
    manager: Arc<McpManager>,
}

impl McpProxyTool {
    pub fn new(manager: Arc<McpManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for McpProxyTool {
    fn name(&self) -> &str {
        "mcp"
    }

    fn description(&self) -> &str {
        "Discover and call MCP tools. Actions: status, search, describe, call. For call, pass server, tool, and arguments."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["status", "search", "describe", "call"],
                    "description": "status | search | describe | call"
                },
                "query": {"type": "string", "description": "Search query for action=search"},
                "server": {"type": "string"},
                "tool": {"type": "string"},
                "arguments": {"type": "object"}
            },
            "required": ["action"]
        })
    }

    fn prompt_snippet(&self) -> &str {
        "Discover and call MCP server tools"
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> ToolResult {
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
        match action {
            "status" => ToolResult::ok(self.manager.status_text().await),
            "search" => {
                let q = args
                    .get("query")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let tools = self.manager.all_tools().await;
                let hits: Vec<String> = tools
                    .into_iter()
                    .filter(|t| {
                        q.is_empty()
                            || t.name.to_ascii_lowercase().contains(&q)
                            || t.description.to_ascii_lowercase().contains(&q)
                            || t.server.to_ascii_lowercase().contains(&q)
                    })
                    .map(|t| format!("mcp_{}_{} — {}", sanitize(&t.server), t.name, t.description))
                    .collect();
                if hits.is_empty() {
                    ToolResult::ok("No MCP tools matched.")
                } else {
                    ToolResult::ok(hits.join("\n"))
                }
            }
            "describe" => {
                let server = args.get("server").and_then(|v| v.as_str()).unwrap_or("");
                let tool = args.get("tool").and_then(|v| v.as_str()).unwrap_or("");
                match self.manager.find_tool(server, tool).await {
                    Some((_, info)) => ToolResult::ok(format!(
                        "server: {}\ntool: {}\ndescription: {}\ninputSchema: {}",
                        info.server,
                        info.name,
                        info.description,
                        serde_json::to_string_pretty(&info.input_schema).unwrap_or_default()
                    )),
                    None => ToolResult::err(format!("unknown MCP tool {server}/{tool}")),
                }
            }
            "call" => {
                let server = args.get("server").and_then(|v| v.as_str()).unwrap_or("");
                let tool = args.get("tool").and_then(|v| v.as_str()).unwrap_or("");
                let arguments = args.get("arguments").cloned().unwrap_or(json!({}));
                match self.manager.find_tool(server, tool).await {
                    Some((client, _)) => match client.call_tool(tool, arguments).await {
                        Ok(v) => ToolResult::ok(stringify_mcp_result(&v)),
                        Err(e) => ToolResult::err(e.to_string()),
                    },
                    None => ToolResult::err(format!("unknown MCP tool {server}/{tool}")),
                }
            }
            _ => ToolResult::err("action must be status, search, describe, or call"),
        }
    }
}

pub struct McpDirectTool {
    manager: Arc<McpManager>,
    server: String,
    tool: String,
    description: String,
    parameters: Value,
    qualified: String,
}

impl McpDirectTool {
    pub fn new(
        manager: Arc<McpManager>,
        server: String,
        tool: String,
        description: String,
        parameters: Value,
    ) -> Self {
        let qualified = format!("mcp_{}_{}", sanitize(&server), tool);
        Self {
            manager,
            server,
            tool,
            description,
            parameters,
            qualified,
        }
    }
}

#[async_trait]
impl Tool for McpDirectTool {
    fn name(&self) -> &str {
        &self.qualified
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        self.parameters.clone()
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> ToolResult {
        match self.manager.find_tool(&self.server, &self.tool).await {
            Some((client, _)) => match client.call_tool(&self.tool, args).await {
                Ok(v) => ToolResult::ok(stringify_mcp_result(&v)),
                Err(e) => ToolResult::err(e.to_string()),
            },
            None => ToolResult::err(format!("MCP server {} is not connected", self.server)),
        }
    }
}

pub async fn mcp_tools_from_manager(
    manager: Arc<McpManager>,
    direct: bool,
) -> Vec<Arc<dyn Tool>> {
    let mut tools: Vec<Arc<dyn Tool>> = vec![Arc::new(McpProxyTool::new(manager.clone()))];
    if direct {
        for info in manager.all_tools().await {
            tools.push(Arc::new(McpDirectTool::new(
                manager.clone(),
                info.server,
                info.name,
                info.description,
                info.input_schema,
            )));
        }
    }
    tools
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn stringify_mcp_result(v: &Value) -> String {
    if let Some(content) = v.get("content").and_then(|c| c.as_array()) {
        let mut parts = Vec::new();
        for item in content {
            if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                parts.push(t.to_string());
            } else {
                parts.push(item.to_string());
            }
        }
        if v.get("isError").and_then(|e| e.as_bool()).unwrap_or(false) {
            return format!("MCP error:\n{}", parts.join("\n"));
        }
        return parts.join("\n");
    }
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}
