use reqwest::Client;
use serde_json::Value;

use crate::client::Transport;
use crate::protocol::{JsonRpcRequest, JsonRpcResponse, McpError, McpResult};

/// Streamable HTTP transport (MCP 2025-03-26). Posts JSON-RPC to the endpoint
/// and expects a JSON response. SSE fallback is accepted when Content-Type is
/// text/event-stream: the first `data:` JSON object is used.
pub struct HttpTransport {
    client: Client,
    url: String,
    headers: Vec<(String, String)>,
    session_id: Option<String>,
}

impl HttpTransport {
    pub fn new(url: impl Into<String>, headers: Vec<(String, String)>) -> Self {
        Self {
            client: Client::new(),
            url: url.into(),
            headers,
            session_id: None,
        }
    }
}

#[async_trait::async_trait]
impl Transport for HttpTransport {
    async fn request(&mut self, req: JsonRpcRequest) -> McpResult<JsonRpcResponse> {
        let mut builder = self.client.post(&self.url).json(&req);
        for (k, v) in &self.headers {
            builder = builder.header(k, v);
        }
        if let Some(sid) = &self.session_id {
            builder = builder.header("mcp-session-id", sid);
        }
        let resp = builder
            .send()
            .await
            .map_err(|e| McpError::Transport(e.to_string()))?;
        if let Some(sid) = resp.headers().get("mcp-session-id") {
            if let Ok(s) = sid.to_str() {
                self.session_id = Some(s.to_string());
            }
        }
        let status = resp.status();
        let ctype = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let text = resp
            .text()
            .await
            .map_err(|e| McpError::Transport(e.to_string()))?;
        if !status.is_success() {
            return Err(McpError::Transport(format!("HTTP {status}: {text}")));
        }
        if ctype.contains("text/event-stream") {
            parse_sse_jsonrpc(&text)
        } else {
            serde_json::from_str(&text).map_err(|e| McpError::Protocol(e.to_string()))
        }
    }

    async fn notify(&mut self, req: JsonRpcRequest) -> McpResult<()> {
        let _ = self.request(req).await?;
        Ok(())
    }

    async fn close(&mut self) -> McpResult<()> {
        Ok(())
    }
}

fn parse_sse_jsonrpc(text: &str) -> McpResult<JsonRpcResponse> {
    for line in text.lines() {
        let line = line.trim();
        if let Some(data) = line.strip_prefix("data:") {
            let data = data.trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<Value>(data) {
                if v.get("jsonrpc").is_some() {
                    return serde_json::from_value(v).map_err(|e| McpError::Protocol(e.to_string()));
                }
            }
        }
    }
    Err(McpError::Protocol("no JSON-RPC payload in SSE stream".into()))
}
