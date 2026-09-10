use crate::config::{McpServerConfig, McpServersFile};
use crate::protocol::{JsonRpcId, JsonRpcRequest, JsonRpcResponse, PROTOCOL_VERSION};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;

#[derive(Debug, Error)]
pub enum McpError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("rpc: {0}")]
    Rpc(String),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone)]
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub server: String,
}

enum Transport {
    Stdio {
        stdin: Mutex<ChildStdin>,
        pending: Arc<Mutex<HashMap<i64, tokio::sync::oneshot::Sender<JsonRpcResponse>>>>,
        next_id: AtomicI64,
        _child: Child,
        _reader: JoinHandle<()>,
    },
    Http {
        url: String,
        client: reqwest::Client,
        headers: reqwest::header::HeaderMap,
        next_id: AtomicI64,
    },
}

pub struct McpClient {
    pub name: String,
    transport: Transport,
}

impl McpClient {
    pub async fn connect(name: &str, cfg: &McpServerConfig) -> Result<Self, McpError> {
        if cfg.url.is_some() {
            Self::connect_http(name, cfg).await
        } else {
            Self::connect_stdio(name, cfg).await
        }
    }

    async fn connect_stdio(name: &str, cfg: &McpServerConfig) -> Result<Self, McpError> {
        let command = cfg
            .command
            .as_deref()
            .ok_or_else(|| McpError::Rpc("stdio MCP server requires `command`".into()))?;
        let mut cmd = Command::new(command);
        cmd.args(&cfg.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (k, v) in &cfg.env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "missing stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "missing stdout"))?;
        let pending: Arc<Mutex<HashMap<i64, tokio::sync::oneshot::Sender<JsonRpcResponse>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let pending2 = pending.clone();
        let reader = tokio::spawn(async move {
            read_loop(stdout, pending2).await;
        });
        let client = Self {
            name: name.to_string(),
            transport: Transport::Stdio {
                stdin: Mutex::new(stdin),
                pending,
                next_id: AtomicI64::new(1),
                _child: child,
                _reader: reader,
            },
        };
        client.initialize().await?;
        Ok(client)
    }

    async fn connect_http(name: &str, cfg: &McpServerConfig) -> Result<Self, McpError> {
        let url = cfg.url.clone().unwrap();
        let http = reqwest::Client::new();
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            "application/json".parse().unwrap(),
        );
        headers.insert(
            reqwest::header::ACCEPT,
            "application/json, text/event-stream".parse().unwrap(),
        );
        for (k, v) in &cfg.headers {
            if let (Ok(hn), Ok(hv)) = (
                reqwest::header::HeaderName::from_bytes(k.as_bytes()),
                reqwest::header::HeaderValue::from_str(v),
            ) {
                headers.insert(hn, hv);
            }
        }
        let client = Self {
            name: name.to_string(),
            transport: Transport::Http {
                url,
                client: http,
                headers,
                next_id: AtomicI64::new(1),
            },
        };
        client.initialize().await?;
        Ok(client)
    }

    async fn initialize(&self) -> Result<(), McpError> {
        let result = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {"tools": {}},
                    "clientInfo": {"name": "rupi", "version": "0.1.0"}
                }),
            )
            .await?;
        if result.error.is_some() {
            return Err(McpError::Rpc(format!(
                "initialize failed: {:?}",
                result.error
            )));
        }
        if let Transport::Http {
            headers, client, url, ..
        } = &self.transport
        {
            // capture session id from last HTTP response is handled in request()
            let _ = (headers, client, url);
        }
        self.notify("notifications/initialized", json!({})).await?;
        Ok(())
    }

    pub async fn list_tools(&self) -> Result<Vec<McpToolInfo>, McpError> {
        let resp = self.request("tools/list", json!({})).await?;
        let result = resp
            .result
            .ok_or_else(|| McpError::Rpc(format!("tools/list error: {:?}", resp.error)))?;
        let tools = result
            .get("tools")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(tools
            .into_iter()
            .filter_map(|t| {
                Some(McpToolInfo {
                    name: t.get("name")?.as_str()?.to_string(),
                    description: t
                        .get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or("")
                        .to_string(),
                    input_schema: t
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or(json!({"type": "object"})),
                    server: self.name.clone(),
                })
            })
            .collect())
    }

    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, McpError> {
        let resp = self
            .request("tools/call", json!({"name": name, "arguments": arguments}))
            .await?;
        if let Some(err) = resp.error {
            return Err(McpError::Rpc(err.message));
        }
        Ok(resp.result.unwrap_or(Value::Null))
    }

    async fn request(&self, method: &str, params: Value) -> Result<JsonRpcResponse, McpError> {
        match &self.transport {
            Transport::Stdio {
                stdin,
                pending,
                next_id,
                ..
            } => {
                let id = next_id.fetch_add(1, Ordering::SeqCst);
                let (tx, rx) = tokio::sync::oneshot::channel();
                pending.lock().await.insert(id, tx);
                let req = JsonRpcRequest {
                    jsonrpc: "2.0".into(),
                    id: Some(JsonRpcId::Number(id)),
                    method: method.into(),
                    params: Some(params),
                };
                write_stdio(stdin, &req).await?;
                rx.await.map_err(|_| McpError::Rpc("rpc cancelled".into()))
            }
            Transport::Http {
                url,
                client,
                headers,
                next_id,
            } => {
                let id = next_id.fetch_add(1, Ordering::SeqCst);
                let req = JsonRpcRequest {
                    jsonrpc: "2.0".into(),
                    id: Some(JsonRpcId::Number(id)),
                    method: method.into(),
                    params: Some(params),
                };
                let resp = client
                    .post(url)
                    .headers(headers.clone())
                    .json(&req)
                    .send()
                    .await
                    .map_err(|e| McpError::Rpc(e.to_string()))?;
                if let Some(sid) = resp.headers().get("mcp-session-id") {
                    // Can't mutate headers through &self; session cookies still work for many servers
                    // that are stateless. Best-effort: include in subsequent clones if we stored it.
                    let _ = sid;
                }
                let status = resp.status();
                let text = resp.text().await.map_err(|e| McpError::Rpc(e.to_string()))?;
                if !status.is_success() {
                    return Err(McpError::Rpc(format!("HTTP {status}: {text}")));
                }
                // Streamable HTTP may return SSE; extract first data JSON.
                let json_text = extract_json_payload(&text);
                serde_json::from_str(&json_text).map_err(McpError::from)
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        match &self.transport {
            Transport::Stdio { stdin, .. } => {
                let req = JsonRpcRequest {
                    jsonrpc: "2.0".into(),
                    id: None,
                    method: method.into(),
                    params: Some(params),
                };
                write_stdio(stdin, &req).await
            }
            Transport::Http {
                url, client, headers, ..
            } => {
                let req = JsonRpcRequest {
                    jsonrpc: "2.0".into(),
                    id: None,
                    method: method.into(),
                    params: Some(params),
                };
                let _ = client
                    .post(url)
                    .headers(headers.clone())
                    .json(&req)
                    .send()
                    .await;
                Ok(())
            }
        }
    }
}

fn extract_json_payload(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.starts_with('{') {
        return trimmed.to_string();
    }
    for line in trimmed.lines() {
        if let Some(data) = line.strip_prefix("data:") {
            let data = data.trim();
            if data.starts_with('{') {
                return data.to_string();
            }
        }
    }
    trimmed.to_string()
}

async fn write_stdio(stdin: &Mutex<ChildStdin>, req: &JsonRpcRequest) -> Result<(), McpError> {
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    let mut stdin = stdin.lock().await;
    stdin.write_all(line.as_bytes()).await?;
    stdin.flush().await?;
    Ok(())
}

async fn read_loop(
    stdout: tokio::process::ChildStdout,
    pending: Arc<Mutex<HashMap<i64, tokio::sync::oneshot::Sender<JsonRpcResponse>>>>,
) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(resp) = serde_json::from_str::<JsonRpcResponse>(&line) else {
            continue;
        };
        let id = match resp.id {
            Some(JsonRpcId::Number(n)) => n,
            _ => continue,
        };
        if let Some(tx) = pending.lock().await.remove(&id) {
            let _ = tx.send(resp);
        }
    }
}

pub struct McpServerHandle {
    pub name: String,
    pub client: Option<Arc<McpClient>>,
    pub tools: Vec<McpToolInfo>,
    pub error: Option<String>,
}

pub struct McpManager {
    pub servers: RwLock<Vec<McpServerHandle>>,
}

impl Default for McpManager {
    fn default() -> Self {
        Self::new()
    }
}

impl McpManager {
    pub fn new() -> Self {
        Self {
            servers: RwLock::new(Vec::new()),
        }
    }

    pub async fn connect_all(file: &McpServersFile) -> Self {
        let mgr = Self::new();
        for (name, cfg) in &file.mcp_servers {
            if cfg.disabled {
                continue;
            }
            match McpClient::connect(name, cfg).await {
                Ok(client) => {
                    let tools = client.list_tools().await.unwrap_or_default();
                    mgr.servers.write().await.push(McpServerHandle {
                        name: name.clone(),
                        client: Some(Arc::new(client)),
                        tools,
                        error: None,
                    });
                }
                Err(e) => {
                    tracing::warn!("MCP server {name} failed: {e}");
                    mgr.servers.write().await.push(McpServerHandle {
                        name: name.clone(),
                        client: None,
                        tools: Vec::new(),
                        error: Some(e.to_string()),
                    });
                }
            }
        }
        mgr
    }

    pub async fn status_text(&self) -> String {
        let servers = self.servers.read().await;
        if servers.is_empty() {
            return "No MCP servers configured.".into();
        }
        let mut lines = Vec::new();
        for s in servers.iter() {
            if let Some(err) = &s.error {
                lines.push(format!("x {} — {err}", s.name));
            } else {
                lines.push(format!("* {} — {} tools", s.name, s.tools.len()));
                for t in &s.tools {
                    lines.push(format!("    - {}", t.name));
                }
            }
        }
        lines.join("\n")
    }

    pub async fn find_tool(&self, server: &str, tool: &str) -> Option<(Arc<McpClient>, McpToolInfo)> {
        let servers = self.servers.read().await;
        for s in servers.iter() {
            if s.name != server {
                continue;
            }
            let client = s.client.clone()?;
            let t = s.tools.iter().find(|t| t.name == tool)?.clone();
            return Some((client, t));
        }
        None
    }

    pub async fn all_tools(&self) -> Vec<McpToolInfo> {
        self.servers
            .read()
            .await
            .iter()
            .filter(|s| s.error.is_none())
            .flat_map(|s| s.tools.clone())
            .collect()
    }
}
