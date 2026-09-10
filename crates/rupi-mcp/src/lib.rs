//! rupi-mcp: MCP-Direct 桥（Pi 官方立场：core 无 MCP，能力走扩展）。
//! 实现 `spawn server → initialize → tools/list → registerTool` 全链路：
//! stdio 上跑换行分隔的 JSON-RPC 2.0，30s 超时，`sanitize_params` 把 LLM 传回的
//! string 宽容转回 boolean/number，`prompt_snippet` 必填否则 agent 看不见工具。

use anyhow::Context;
use rupi_core::ToolDefinition;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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
        tokio::spawn(async move {
            while let Some(line) = rx_lines.recv().await {
                let v: serde_json::Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if let Some(id) = v.get("id").and_then(|i| i.as_i64()) {
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
            next_id: AtomicI64::new(1),
            pending,
            stdin: Arc::new(Mutex::new(stdin)),
            _child: child,
            _reader_task: reader_task,
            _stderr_task: stderr_task,
        };
        bridge
            .call(
                "initialize",
                serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
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
        let resp = tokio::time::timeout(std::time::Duration::from_secs(30), rx)
            .await
            .map_err(|_| anyhow::anyhow!("MCP {method} timed out after 30s"))??;
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
