use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::protocol::{
    CallToolResult, JsonRpcId, JsonRpcRequest, JsonRpcResponse, McpError, McpResult, ToolSpec,
    PROTOCOL_VERSION,
};

#[async_trait::async_trait]
pub trait Transport: Send + Sync {
    async fn request(&mut self, req: JsonRpcRequest) -> McpResult<JsonRpcResponse>;
    async fn notify(&mut self, req: JsonRpcRequest) -> McpResult<()>;
    async fn close(&mut self) -> McpResult<()>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportKind {
    Stdio,
    Http,
    Sse,
    Memory,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub name: String,
    pub transport: TransportKind,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub lazy: bool,
}

pub struct McpClient {
    pub name: String,
    transport: Box<dyn Transport>,
    next_id: AtomicI64,
    pub tools: Vec<ToolSpec>,
    pub protocol_version: String,
}

impl McpClient {
    pub fn new(name: impl Into<String>, transport: Box<dyn Transport>) -> Self {
        Self {
            name: name.into(),
            transport,
            next_id: AtomicI64::new(1),
            tools: Vec::new(),
            protocol_version: PROTOCOL_VERSION.into(),
        }
    }

    fn next_id(&self) -> i64 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }

    pub async fn initialize(&mut self, roots: Vec<String>) -> McpResult<Value> {
        let req = JsonRpcRequest::new(
            self.next_id(),
            "initialize",
            Some(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {
                    "roots": { "listChanged": false },
                    "tools": {}
                },
                "clientInfo": { "name": "rupi", "version": env!("CARGO_PKG_VERSION") },
                "roots": roots.iter().map(|r| json!({"uri": format!("file://{r}"), "name": r})).collect::<Vec<_>>(),
            })),
        );
        let resp = self.transport.request(req).await?;
        let result = take_result(resp)?;
        if let Some(v) = result["protocolVersion"].as_str() {
            self.protocol_version = v.to_string();
        }
        let _ = self
            .transport
            .notify(JsonRpcRequest::notification("notifications/initialized", None))
            .await;
        Ok(result)
    }

    pub async fn list_tools(&mut self) -> McpResult<Vec<ToolSpec>> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..100 {
            let mut params = json!({});
            if let Some(c) = &cursor {
                params["cursor"] = json!(c);
            }
            let req = JsonRpcRequest::new(self.next_id(), "tools/list", Some(params));
            let resp = self.transport.request(req).await?;
            let result = take_result(resp)?;
            if let Some(arr) = result["tools"].as_array() {
                for t in arr {
                    if let Ok(spec) = serde_json::from_value::<ToolSpec>(t.clone()) {
                        tools.push(spec);
                    }
                }
            }
            cursor = result["nextCursor"].as_str().map(|s| s.to_string());
            if cursor.is_none() {
                break;
            }
        }
        self.tools = tools.clone();
        Ok(tools)
    }

    pub async fn call_tool(&mut self, name: &str, arguments: Value) -> McpResult<CallToolResult> {
        let req = JsonRpcRequest::new(
            self.next_id(),
            "tools/call",
            Some(json!({ "name": name, "arguments": arguments })),
        );
        let resp = self.transport.request(req).await?;
        let result = take_result(resp)?;
        serde_json::from_value(result).map_err(|e| McpError::Protocol(e.to_string()))
    }
}

fn take_result(resp: JsonRpcResponse) -> McpResult<Value> {
    if let Some(err) = resp.error {
        Err(McpError::Rpc {
            code: err.code,
            message: err.message,
        })
    } else {
        Ok(resp.result.unwrap_or(Value::Null))
    }
}

#[derive(Default)]
pub struct McpManager {
    pub clients: HashMap<String, McpClient>,
}

impl McpManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, client: McpClient) {
        self.clients.insert(client.name.clone(), client);
    }

    pub fn all_tools(&self) -> Vec<(String, ToolSpec)> {
        let mut out = Vec::new();
        for (server, client) in &self.clients {
            for tool in &client.tools {
                out.push((server.clone(), tool.clone()));
            }
        }
        out
    }
}

/// In-memory loopback used by tests.
pub struct InMemoryTransport {
    pub handler: Box<dyn Fn(JsonRpcRequest) -> JsonRpcResponse + Send + Sync>,
}

impl InMemoryTransport {
    pub fn echo_tools(tools: Vec<ToolSpec>) -> Self {
        let tools_for_list = tools.clone();
        Self {
            handler: Box::new(move |req: JsonRpcRequest| match req.method.as_str() {
                "initialize" => JsonRpcResponse {
                    jsonrpc: "2.0".into(),
                    id: req.id,
                    result: Some(json!({
                        "protocolVersion": PROTOCOL_VERSION,
                        "capabilities": { "tools": { "listChanged": true } },
                        "serverInfo": { "name": "echo", "version": "0.1.0" }
                    })),
                    error: None,
                },
                "notifications/initialized" => JsonRpcResponse {
                    jsonrpc: "2.0".into(),
                    id: JsonRpcId::Null,
                    result: Some(Value::Null),
                    error: None,
                },
                "tools/list" => JsonRpcResponse {
                    jsonrpc: "2.0".into(),
                    id: req.id,
                    result: Some(json!({ "tools": tools_for_list })),
                    error: None,
                },
                "tools/call" => {
                    let name = req
                        .params
                        .as_ref()
                        .and_then(|p| p["name"].as_str())
                        .unwrap_or("unknown");
                    let args = req
                        .params
                        .as_ref()
                        .map(|p| p["arguments"].clone())
                        .unwrap_or(Value::Null);
                    JsonRpcResponse {
                        jsonrpc: "2.0".into(),
                        id: req.id,
                        result: Some(json!({
                            "content": [{"type": "text", "text": format!("{name} => {args}")}],
                            "isError": false
                        })),
                        error: None,
                    }
                }
                other => JsonRpcResponse {
                    jsonrpc: "2.0".into(),
                    id: req.id,
                    result: None,
                    error: Some(crate::JsonRpcError {
                        code: -32601,
                        message: format!("method not found: {other}"),
                        data: None,
                    }),
                },
            }),
        }
    }
}

#[async_trait::async_trait]
impl Transport for InMemoryTransport {
    async fn request(&mut self, req: JsonRpcRequest) -> McpResult<JsonRpcResponse> {
        Ok((self.handler)(req))
    }
    async fn notify(&mut self, _req: JsonRpcRequest) -> McpResult<()> {
        Ok(())
    }
    async fn close(&mut self) -> McpResult<()> {
        Ok(())
    }
}

pub fn load_mcp_configs(global: Option<&str>, project: Option<&str>) -> Vec<ServerConfig> {
    let mut by_name: HashMap<String, ServerConfig> = HashMap::new();
    for raw in [global, project].into_iter().flatten() {
        if let Ok(v) = serde_json::from_str::<Value>(raw) {
            if let Some(servers) = v["mcpServers"].as_object().or(v.as_object()) {
                for (name, spec) in servers {
                    if name == "mcpServers" {
                        continue;
                    }
                    if let Some(cfg) = parse_server(name, spec) {
                        by_name.insert(name.clone(), cfg);
                    }
                }
            }
        }
    }
    by_name.into_values().collect()
}

fn parse_server(name: &str, spec: &Value) -> Option<ServerConfig> {
    if let Some(cmd) = spec["command"].as_str() {
        let args = spec["args"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        return Some(ServerConfig {
            name: name.into(),
            transport: TransportKind::Stdio,
            command: Some(cmd.into()),
            args,
            env: Default::default(),
            url: None,
            headers: Default::default(),
            lazy: spec["lazy"].as_bool().unwrap_or(false),
        });
    }
    if let Some(url) = spec["url"].as_str() {
        let kind = if spec["type"].as_str() == Some("sse") {
            TransportKind::Sse
        } else {
            TransportKind::Http
        };
        return Some(ServerConfig {
            name: name.into(),
            transport: kind,
            command: None,
            args: vec![],
            env: Default::default(),
            url: Some(url.into()),
            headers: Default::default(),
            lazy: spec["lazy"].as_bool().unwrap_or(false),
        });
    }
    None
}
