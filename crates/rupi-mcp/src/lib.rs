//! rupi-mcp: MCP-Direct 桥（Pi 官方立场：core 无 MCP，能力走扩展）。
//! 实现 `spawn server → initialize → tools/list → registerTool` 全链路，
//! 外加 `resources/list → resources/read`（每 server 一个 `{server}_read_resource` 原生工具）：
//! stdio 上跑换行分隔的 JSON-RPC 2.0，30s 超时，`sanitize_params` 把 LLM 传回的
//! string 宽容转回 boolean/number，`prompt_snippet` 必填否则 agent 看不见工具。

use anyhow::Context;
use rupi_core::ToolDefinition;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot, Mutex};

/// MCP server 配置：一条命令 + 参数 + 可选环境变量。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
}

impl McpServerConfig {
    pub fn new(name: &str, command: &str, args: Vec<String>) -> Self {
        Self {
            name: name.into(),
            command: command.into(),
            args,
            env: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// 暴露给 server 的根目录（MCP roots）：默认当前工作目录，文件类 server 据此定作用域。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpRoot {
    pub uri: String,
    pub name: String,
}

impl McpRoot {
    pub fn cwd() -> Self {
        let dir = std::env::current_dir().unwrap_or_else(|_| Path::new("/tmp").to_path_buf());
        Self {
            uri: format!("file://{}", dir.display()),
            name: dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "root".into()),
        }
    }
}

/// server→client 请求应答（纯函数，可单测）：roots/list 返回本机根目录，
/// ping 回空结果，未知方法按 JSON-RPC 回 MethodNotFound；notification（无 id）返回 None。
pub fn server_request_response(
    method: &str,
    id: Option<i64>,
    roots: &[McpRoot],
) -> Option<serde_json::Value> {
    let id = id?;
    let payload = match method {
        "roots/list" => serde_json::json!({"roots": roots}),
        "ping" => serde_json::json!({}),
        _ => {
            return Some(serde_json::json!({
                "jsonrpc": "2.0", "id": id,
                "error": {"code": -32601, "message": format!("method not found: {method}")}
            }));
        }
    };
    Some(serde_json::json!({"jsonrpc": "2.0", "id": id, "result": payload}))
}

/// 把 LLM 经常传成 string 的参数按 JSON Schema 纠正回 boolean / number / integer。
/// 对应 pi-directx 的 `sanitizeParams`。
pub fn sanitize_params(
    params: &serde_json::Value,
    schema: &serde_json::Value,
) -> serde_json::Value {
    let Some(obj) = params.as_object() else {
        return params.clone();
    };
    let Some(props) = schema.get("properties").and_then(|p| p.as_object()) else {
        return params.clone();
    };
    let mut out = obj.clone();
    for (key, prop) in props {
        let Some(v) = obj.get(key) else { continue };
        let Some(t) = prop.get("type").and_then(|t| t.as_str()) else {
            continue;
        };
        if let serde_json::Value::String(s) = v {
            let coerced = match t {
                "boolean" => match s.to_lowercase().as_str() {
                    "true" | "1" | "yes" => Some(serde_json::json!(true)),
                    "false" | "0" | "no" => Some(serde_json::json!(false)),
                    _ => None,
                },
                "number" => s.parse::<f64>().ok().map(|n| serde_json::json!(n)),
                "integer" => s.parse::<i64>().ok().map(|n| serde_json::json!(n)),
                _ => None,
            };
            if let Some(c) = coerced {
                out.insert(key.clone(), c);
            }
        }
    }
    serde_json::Value::Object(out)
}

type PendingMap = Arc<Mutex<HashMap<i64, oneshot::Sender<serde_json::Value>>>>;

/// MCP stdio 桥：拥有子进程 + JSON-RPC 路由（requestId 自增、30s 超时、退出清理）。
pub struct McpBridge {
    pub config: McpServerConfig,
    pub roots: Vec<McpRoot>,
    next_id: AtomicI64,
    pending: PendingMap,
    stdin: Arc<Mutex<ChildStdin>>,
    _child: Child,
    _reader_task: tokio::task::JoinHandle<()>,
    _stderr_task: tokio::task::JoinHandle<()>,
}

impl McpBridge {
    /// 启动 server 进程并握手 `initialize` + `notifications/initialized`。
    pub async fn spawn(config: McpServerConfig) -> anyhow::Result<Self> {
        let mut cmd = Command::new(&config.command);
        cmd.args(&config.args)
            .envs(&config.env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = cmd
            .spawn()
            .context(format!("spawn MCP server {}", config.command))?;
        let stdin: ChildStdin = child.stdin.take().context("no stdin")?;
        let stdout: ChildStdout = child.stdout.take().context("no stdout")?;
        let stderr = child.stderr.take();

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (tx_lines, mut rx_lines) = mpsc::unbounded_channel::<String>();

        // stdout 读取任务：逐行解析，server→client 只可能是 Response 或 Notification。
        let pending_clone = pending.clone();
        let reader_task = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                let _ = tx_lines.send(line);
            }
        });
        let pending_route = pending.clone();
        let stdin_route = Arc::new(Mutex::new(stdin));
        let roots_route = vec![McpRoot::cwd()];
        let stdin_write = stdin_route.clone();
        let roots_in_task = roots_route.clone();
        tokio::spawn(async move {
            while let Some(line) = rx_lines.recv().await {
                let v: serde_json::Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let id = v.get("id").and_then(|i| i.as_i64());
                // server→client 请求（含 roots/list、ping）：必须应答，否则 server 侧超时
                if let Some(method) = v.get("method").and_then(|m| m.as_str()) {
                    if let Some(resp) = server_request_response(method, id, &roots_in_task) {
                        let mut stdin = stdin_write.lock().await;
                        let _ = stdin.write_all(format!("{resp}\n").as_bytes()).await;
                        let _ = stdin.flush().await;
                    }
                    continue;
                }
                // 无 method 即 Response：按 id 路由给挂起的 call
                if let Some(id) = id {
                    if let Some(tx) = pending_route.lock().await.remove(&id) {
                        let _ = tx.send(v);
                    }
                }
                // notifications 直接丢弃（可在此转 event）
            }
            drop(pending_clone);
        });

        let stderr_task = tokio::spawn(async move {
            if let Some(stderr) = stderr {
                let mut reader = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    tracing::debug!(target: "rupi-mcp", "mcp stderr: {line}");
                }
            }
        });

        let bridge = Self {
            config,
            roots: roots_route.clone(),
            next_id: AtomicI64::new(1),
            pending,
            stdin: stdin_route,
            _child: child,
            _reader_task: reader_task,
            _stderr_task: stderr_task,
        };
        bridge
            .call(
                "initialize",
                serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {"roots": {"listChanged": false}},
                    "clientInfo": {"name": "rupi", "version": "0.1.0"}
                }),
            )
            .await?;
        bridge
            .notify("notifications/initialized", serde_json::json!({}))
            .await?;
        Ok(bridge)
    }

    pub async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = serde_json::json!({"jsonrpc":"2.0","id": id, "method": method, "params": params});
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        {
            let mut stdin = self.stdin.lock().await;
            stdin.write_all(format!("{}\n", req).as_bytes()).await?;
            stdin.flush().await?;
        }
        let resp = match tokio::time::timeout(std::time::Duration::from_secs(30), rx).await {
            Ok(r) => r?,
            Err(_) => {
                // 超时即摘掉挂起项：迟到响应无人认领，不留泄漏
                self.pending.lock().await.remove(&id);
                anyhow::bail!("MCP {method} timed out after 30s");
            }
        };
        if let Some(err) = resp.get("error") {
            anyhow::bail!("MCP error for {method}: {err}");
        }
        Ok(resp
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null))
    }

    pub async fn notify(&self, method: &str, params: serde_json::Value) -> anyhow::Result<()> {
        let req = serde_json::json!({"jsonrpc":"2.0","method": method, "params": params});
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(format!("{}\n", req).as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    /// `tools/list`（支持 cursor 分页）→ 全部工具。
    pub async fn list_tools(&self) -> anyhow::Result<Vec<McpTool>> {
        let mut tools = vec![];
        let mut cursor: Option<String> = None;
        loop {
            let mut params = serde_json::json!({});
            if let Some(c) = cursor {
                params["cursor"] = serde_json::json!(c);
            }
            let result = self.call("tools/list", params).await?;
            if let Some(arr) = result.get("tools").and_then(|t| t.as_array()) {
                for t in arr {
                    tools.push(McpTool {
                        name: t
                            .get("name")
                            .and_then(|s| s.as_str())
                            .unwrap_or("")
                            .to_string(),
                        description: t
                            .get("description")
                            .and_then(|s| s.as_str())
                            .unwrap_or("")
                            .to_string(),
                        input_schema: t
                            .get("inputSchema")
                            .cloned()
                            .unwrap_or(serde_json::json!({"type":"object"})),
                    });
                }
            }
            cursor = result
                .get("nextCursor")
                .and_then(|s| s.as_str())
                .map(|s| s.to_string());
            if cursor.is_none() {
                break;
            }
        }
        Ok(tools)
    }

    /// `tools/call`：先 sanitize 再转发；tool 执行失败走 `isError` 结果而非协议 error。
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
        schema: &serde_json::Value,
    ) -> anyhow::Result<McpToolResult> {
        let args = sanitize_params(&arguments, schema);
        let result = self
            .call(
                "tools/call",
                serde_json::json!({"name": name, "arguments": args}),
            )
            .await?;
        let is_error = result
            .get("isError")
            .and_then(|b| b.as_bool())
            .unwrap_or(false);
        let text = result
            .get("content")
            .and_then(|c| c.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|b| {
                        b.get("text")
                            .and_then(|t| t.as_str())
                            .map(|s| s.to_string())
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_else(|| result.to_string());
        Ok(McpToolResult {
            text,
            is_error,
            structured: result.get("structuredContent").cloned(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct McpToolResult {
    pub text: String,
    pub is_error: bool,
    pub structured: Option<serde_json::Value>,
}

/// MCP 资源描述（`resources/list` 条目）：uri 唯一定位，name/mime 供展示。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpResource {
    pub uri: String,
    pub name: String,
    pub mime_type: Option<String>,
}

impl McpBridge {
    /// `resources/list`（支持 cursor 分页）→ 全部资源。
    pub async fn list_resources(&self) -> anyhow::Result<Vec<McpResource>> {
        let mut out = vec![];
        let mut cursor: Option<String> = None;
        loop {
            let mut params = serde_json::json!({});
            if let Some(c) = cursor {
                params["cursor"] = serde_json::json!(c);
            }
            let result = self.call("resources/list", params).await?;
            if let Some(arr) = result.get("resources").and_then(|r| r.as_array()) {
                for r in arr {
                    out.push(McpResource {
                        uri: r
                            .get("uri")
                            .and_then(|s| s.as_str())
                            .unwrap_or("")
                            .to_string(),
                        name: r
                            .get("name")
                            .and_then(|s| s.as_str())
                            .unwrap_or("")
                            .to_string(),
                        mime_type: r
                            .get("mimeType")
                            .and_then(|s| s.as_str())
                            .map(|s| s.to_string()),
                    });
                }
            }
            cursor = result
                .get("nextCursor")
                .and_then(|s| s.as_str())
                .map(|s| s.to_string());
            if cursor.is_none() {
                break;
            }
        }
        Ok(out)
    }

    /// `resources/read`：把 contents 里的 text 拼起来回给模型；未知 uri 走协议 error。
    pub async fn read_resource(&self, uri: &str) -> anyhow::Result<String> {
        let result = self
            .call("resources/read", serde_json::json!({"uri": uri}))
            .await?;
        let text = result
            .get("contents")
            .and_then(|c| c.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|b| {
                        b.get("text")
                            .and_then(|t| t.as_str())
                            .map(|s| s.to_string())
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_else(|| result.to_string());
        Ok(text)
    }
}

/// 把 MCP 工具转成 Pi 原生工具定义。`prompt_snippet` 必填——没有它 agent 看不见工具。
pub fn mcp_tool_to_definition(prefix: &str, tool: &McpTool) -> ToolDefinition {
    ToolDefinition {
        name: format!("{prefix}_{}", tool.name),
        description: tool.description.clone(),
        input_schema: tool.input_schema.clone(),
        prompt_snippet: Some(format!(
            "{} (MCP via {}): {}",
            tool.name,
            prefix,
            if tool.description.is_empty() {
                "no description"
            } else {
                &tool.description
            }
        )),
    }
}

/// MCP 资源读取器：每 server 注册一个 `{server}_read_resource` 原生工具。
/// description 内嵌注册时列出的可用 URI（模型不用猜）；执行期按 uri 直读远端。
/// 桥断了也不崩主循环：转成 tool error 回给模型（与 McpToolExecutor 同策略）。
pub struct McpResourceReader {
    definition: ToolDefinition,
    bridge: Arc<McpBridge>,
}

impl McpResourceReader {
    pub fn tool_name(server: &str) -> String {
        format!("{server}_read_resource")
    }

    pub fn new(server: &str, bridge: Arc<McpBridge>, resources: &[McpResource]) -> Self {
        let uris = resources
            .iter()
            .map(|r| r.uri.clone())
            .collect::<Vec<_>>()
            .join(", ");
        let description = if uris.is_empty() {
            format!("Read an MCP resource from server '{server}' by URI.")
        } else {
            format!("Read an MCP resource from server '{server}' by URI. Available: {uris}")
        };
        Self {
            definition: ToolDefinition {
                name: Self::tool_name(server),
                description,
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {"uri": {"type": "string"}},
                    "required": ["uri"],
                }),
                prompt_snippet: Some(format!(
                    "read_resource (MCP via {server}): read a server-provided resource by URI"
                )),
            },
            bridge,
        }
    }
}

#[async_trait::async_trait]
impl rupi_tools::Tool for McpResourceReader {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
    ) -> anyhow::Result<rupi_tools::ToolOutput> {
        let uri = arguments.get("uri").and_then(|u| u.as_str()).unwrap_or("");
        if uri.is_empty() {
            return Ok(rupi_tools::ToolOutput::err(
                "missing required argument: uri",
            ));
        }
        match self.bridge.read_resource(uri).await {
            Ok(text) => Ok(rupi_tools::ToolOutput::ok(text)),
            Err(e) => Ok(rupi_tools::ToolOutput::err(format!(
                "MCP resource read failed: {e:#}"
            ))),
        }
    }
}

// ---- registerToolsFromMCP：把远端 MCP 工具注册为本地原生工具 ----

/// 单个 MCP 工具的本地执行器：`Tool` trait 实现，`tools/call` 前做 sanitize。
pub struct McpToolExecutor {
    definition: ToolDefinition,
    bridge: Arc<McpBridge>,
    tool_name: String,
    input_schema: serde_json::Value,
}

impl McpToolExecutor {
    pub fn new(prefix: &str, bridge: Arc<McpBridge>, tool: &McpTool) -> Self {
        Self {
            definition: mcp_tool_to_definition(prefix, tool),
            bridge,
            tool_name: tool.name.clone(),
            input_schema: tool.input_schema.clone(),
        }
    }
}

#[async_trait::async_trait]
impl rupi_tools::Tool for McpToolExecutor {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
    ) -> anyhow::Result<rupi_tools::ToolOutput> {
        match self
            .bridge
            .call_tool(&self.tool_name, arguments, &self.input_schema)
            .await
        {
            Ok(r) => Ok(if r.is_error {
                rupi_tools::ToolOutput::err(r.text)
            } else {
                rupi_tools::ToolOutput::ok(r.text)
            }),
            // 桥断了也不崩主循环：转成 tool error 回给模型
            Err(e) => Ok(rupi_tools::ToolOutput::err(format!(
                "MCP call failed: {e:#}"
            ))),
        }
    }
}

/// 多 server 管理器：对标 `registerToolsFromMCP`，为每个 server spawn 一座桥，
/// 把全部远端工具注册进 `ToolRegistry`（命名 `{server}_{tool}`，首注册者胜）。
/// config 与 bridge 配对存放：某 server 启动失败只跳过自己，不错位后续配对。
pub struct McpManager {
    pub entries: Vec<McpServerEntry>,
}

pub struct McpServerEntry {
    pub config: McpServerConfig,
    pub bridge: Arc<McpBridge>,
}

impl McpManager {
    pub async fn spawn_all(configs: &[McpServerConfig]) -> anyhow::Result<Self> {
        let mut entries = vec![];
        for cfg in configs {
            match McpBridge::spawn(cfg.clone()).await {
                Ok(b) => entries.push(McpServerEntry {
                    config: cfg.clone(),
                    bridge: Arc::new(b),
                }),
                Err(e) => tracing::warn!("MCP server '{}' failed to start: {e:#}", cfg.name),
            }
        }
        Ok(Self { entries })
    }

    /// 发现全部远端工具并注册进 registry。返回成功注册的工具名。
    pub async fn register_all(&self, registry: &mut rupi_tools::ToolRegistry) -> Vec<String> {
        let mut registered = vec![];
        for entry in &self.entries {
            match entry.bridge.list_tools().await {
                Ok(tools) => {
                    for t in tools {
                        let name = format!("{}_{}", entry.config.name, t.name);
                        if registered.contains(&name) {
                            tracing::warn!("MCP tool name conflict: {name}; first wins");
                            continue;
                        }
                        registry.register(Arc::new(McpToolExecutor::new(
                            &entry.config.name,
                            entry.bridge.clone(),
                            &t,
                        )));
                        registered.push(name);
                    }
                }
                Err(e) => {
                    tracing::warn!("MCP tools/list failed for '{}': {e:#}", entry.config.name)
                }
            }
            // 资源读入口：每 server 一个 `{server}_read_resource`，description 自带可用 URI。
            // resources/list 失败只跳过自己（与工具侧同等的失败隔离），无资源也不注册空工具。
            match entry.bridge.list_resources().await {
                Ok(resources) if !resources.is_empty() => {
                    let name = McpResourceReader::tool_name(&entry.config.name);
                    if registered.contains(&name) {
                        tracing::warn!("MCP tool name conflict: {name}; first wins");
                    } else {
                        registry.register(Arc::new(McpResourceReader::new(
                            &entry.config.name,
                            entry.bridge.clone(),
                            &resources,
                        )));
                        registered.push(name);
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(
                        "MCP resources/list failed for '{}': {e:#}",
                        entry.config.name
                    )
                }
            }
        }
        registered
    }
}

/// 从 JSON 文件加载 server 配置：`[{"name":..,"command":..,"args":[..],"env":{..}}]`。
pub fn load_configs(path: &Path) -> anyhow::Result<Vec<McpServerConfig>> {
    let raw = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&raw)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_coerces_string_types() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "verbose": {"type": "boolean"},
                "count": {"type": "integer"},
                "ratio": {"type": "number"},
                "name": {"type": "string"}
            }
        });
        let params = serde_json::json!({"verbose":"true","count":"42","ratio":"1.5","name":"x"});
        let out = sanitize_params(&params, &schema);
        assert_eq!(out["verbose"], serde_json::json!(true));
        assert_eq!(out["count"], serde_json::json!(42));
        assert_eq!(out["name"], serde_json::json!("x"));
    }

    #[test]
    fn mcp_tool_definition_always_has_prompt_snippet() {
        let t = McpTool {
            name: "search".into(),
            description: "web search".into(),
            input_schema: serde_json::json!({"type":"object"}),
        };
        let d = mcp_tool_to_definition("exa", &t);
        assert_eq!(d.name, "exa_search");
        assert!(d.prompt_snippet.is_some());
    }
}
