use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: &str = "2025-03-26";
pub const PROTOCOL_FALLBACK: &str = "2024-11-05";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(untagged)]
pub enum JsonRpcId {
    Number(i64),
    String(String),
    #[default]
    Null,
}

fn jsonrpc_id_is_null(id: &JsonRpcId) -> bool {
    matches!(id, JsonRpcId::Null)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    /// Notifications omit `id` (JSON-RPC 2.0 / MCP).
    #[serde(default, skip_serializing_if = "jsonrpc_id_is_null")]
    pub id: JsonRpcId,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl JsonRpcRequest {
    pub fn new(id: i64, method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: JsonRpcId::Number(id),
            method: method.into(),
            params,
        }
    }

    pub fn notification(method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: JsonRpcId::Null,
            method: method.into(),
            params,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: JsonRpcId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ToolAnnotation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only_hint: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destructive_hint: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotent_hint: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_world_hint: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolSpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub input_schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<ToolAnnotation>,
}

impl ToolSpec {
    pub fn annotated_description(&self) -> String {
        let mut desc = self.description.clone();
        if let Some(a) = &self.annotations {
            let mut hints = Vec::new();
            if a.read_only_hint == Some(true) {
                hints.push("readOnly");
            }
            if a.destructive_hint == Some(true) {
                hints.push("destructive");
            }
            if a.idempotent_hint == Some(true) {
                hints.push("idempotent");
            }
            if a.open_world_hint == Some(true) {
                hints.push("openWorld");
            }
            if !hints.is_empty() {
                desc = format!("{desc} [{}]", hints.join(", "));
            }
        }
        desc
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallToolResult {
    #[serde(default)]
    pub content: Vec<Value>,
    #[serde(default)]
    pub is_error: bool,
}

impl CallToolResult {
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|c| {
                if c["type"].as_str() == Some("text") {
                    c["text"].as_str().map(|s| s.to_string())
                } else {
                    Some(c.to_string())
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("transport: {0}")]
    Transport(String),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("rpc {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type McpResult<T> = Result<T, McpError>;
