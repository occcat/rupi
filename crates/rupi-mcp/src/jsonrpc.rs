//! 换行分隔的 JSON-RPC 2.0 stdio 会话（MCP 与长连接扩展共用）。
//!
//! 契约：每行一个 JSON 对象；带 `id` 的响应对挂起的 `call`；带 `method` 的
//! 请求/通知推进 `Incoming` 通道，由调用方应答（`respond` / `write_line`）。

use anyhow::Context;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, Mutex};

pub type PendingMap = Arc<Mutex<HashMap<i64, oneshot::Sender<serde_json::Value>>>>;

/// 对端发来的请求或通知（非本端 `call` 的响应）。
#[derive(Debug, Clone)]
pub enum Incoming {
    Request {
        id: i64,
        method: String,
        params: serde_json::Value,
    },
    Notification {
        method: String,
        params: serde_json::Value,
    },
}

/// 长连接 JSON-RPC 子进程（stdin 写、stdout 按行读）。
pub struct StdioRpc {
    pending: PendingMap,
    stdin: Arc<Mutex<ChildStdin>>,
    next_id: AtomicI64,
    incoming: Mutex<Option<mpsc::UnboundedReceiver<Incoming>>>,
    _child: Child,
    _reader_task: tokio::task::JoinHandle<()>,
    _route_task: tokio::task::JoinHandle<()>,
    _stderr_task: tokio::task::JoinHandle<()>,
}

impl StdioRpc {
    pub async fn spawn(
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
    ) -> anyhow::Result<Self> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .envs(env)
            .kill_on_drop(true)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = cmd.spawn().context(format!("spawn {command}"))?;
        let stdin = child.stdin.take().context("no stdin")?;
        let stdout = child.stdout.take().context("no stdout")?;
        let stderr = child.stderr.take();

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (tx_lines, mut rx_lines) = mpsc::unbounded_channel::<String>();
        let reader_task = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                let _ = tx_lines.send(line);
            }
        });

        let (tx_in, rx_in) = mpsc::unbounded_channel::<Incoming>();
        let pending_route = pending.clone();
        let route_task = tokio::spawn(async move {
            while let Some(line) = rx_lines.recv().await {
                let v: serde_json::Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let id = v.get("id").and_then(|i| i.as_i64());
                if let Some(method) = v.get("method").and_then(|m| m.as_str()) {
                    let params = v.get("params").cloned().unwrap_or(serde_json::json!({}));
                    let msg = if let Some(id) = id {
                        Incoming::Request {
                            id,
                            method: method.to_string(),
                            params,
                        }
                    } else {
                        Incoming::Notification {
                            method: method.to_string(),
                            params,
                        }
                    };
                    let _ = tx_in.send(msg);
                    continue;
                }
                if let Some(id) = id {
                    if let Some(tx) = pending_route.lock().await.remove(&id) {
                        let _ = tx.send(v);
                    }
                }
            }
        });

        let stderr_task = tokio::spawn(async move {
            if let Some(stderr) = stderr {
                let mut reader = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    tracing::debug!(target: "rupi-jsonrpc", "stderr: {line}");
                }
            }
        });

        Ok(Self {
            pending,
            stdin: Arc::new(Mutex::new(stdin)),
            next_id: AtomicI64::new(1),
            incoming: Mutex::new(Some(rx_in)),
            _child: child,
            _reader_task: reader_task,
            _route_task: route_task,
            _stderr_task: stderr_task,
        })
    }

    /// 取出对端请求通道（只取一次）。
    pub async fn take_incoming(&self) -> Option<mpsc::UnboundedReceiver<Incoming>> {
        self.incoming.lock().await.take()
    }

    pub async fn write_line(&self, v: &serde_json::Value) -> anyhow::Result<()> {
        let mut guard = self.stdin.lock().await;
        guard.write_all(format!("{v}\n").as_bytes()).await?;
        guard.flush().await?;
        Ok(())
    }

    pub async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        self.call_timeout(method, params, std::time::Duration::from_secs(30))
            .await
    }

    pub async fn call_timeout(
        &self,
        method: &str,
        params: serde_json::Value,
        timeout: std::time::Duration,
    ) -> anyhow::Result<serde_json::Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = serde_json::json!({"jsonrpc":"2.0","id": id, "method": method, "params": params});
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        self.write_line(&req).await?;
        let resp = match tokio::time::timeout(timeout, rx).await {
            Ok(r) => r?,
            Err(_) => {
                self.pending.lock().await.remove(&id);
                anyhow::bail!("JSON-RPC {method} timed out after {}s", timeout.as_secs());
            }
        };
        if let Some(err) = resp.get("error") {
            anyhow::bail!("JSON-RPC error for {method}: {err}");
        }
        Ok(resp
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null))
    }

    pub async fn notify(&self, method: &str, params: serde_json::Value) -> anyhow::Result<()> {
        let req = serde_json::json!({"jsonrpc":"2.0","method": method, "params": params});
        self.write_line(&req).await
    }

    pub async fn respond(&self, id: i64, result: serde_json::Value) -> anyhow::Result<()> {
        self.write_line(&serde_json::json!({"jsonrpc":"2.0","id": id, "result": result}))
            .await
    }

    pub async fn respond_error(
        &self,
        id: i64,
        code: i64,
        message: impl Into<String>,
    ) -> anyhow::Result<()> {
        self.write_line(&serde_json::json!({
            "jsonrpc":"2.0","id": id,
            "error": {"code": code, "message": message.into()}
        }))
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stdio_rpc_call_and_incoming_request() {
        // 小 Python：initialize 回 result；再发一条 ping 请求；tools/call 回显。
        let script = r#"
import json, sys
for line in sys.stdin:
    line=line.strip()
    if not line: continue
    msg=json.loads(line)
    mid=msg.get("id")
    method=msg.get("method")
    if method=="initialize":
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{"ok":True}}), flush=True)
        print(json.dumps({"jsonrpc":"2.0","id":99,"method":"ping","params":{}}), flush=True)
    elif method=="tools/call":
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{"content":"hi"}}), flush=True)
"#;
        let rpc = StdioRpc::spawn("python3", &["-u".into(), "-c".into(), script.into()], &HashMap::new())
            .await
            .unwrap();
        let mut incoming = rpc.take_incoming().await.unwrap();
        let init = rpc
            .call("initialize", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(init["ok"], true);
        let ping = tokio::time::timeout(std::time::Duration::from_secs(2), incoming.recv())
            .await
            .unwrap()
            .expect("incoming ping");
        match ping {
            Incoming::Request { id, method, .. } => {
                assert_eq!(id, 99);
                assert_eq!(method, "ping");
                rpc.respond(id, serde_json::json!({})).await.unwrap();
            }
            _ => panic!("expected request"),
        }
        let call = rpc
            .call("tools/call", serde_json::json!({"name":"x"}))
            .await
            .unwrap();
        assert_eq!(call["content"], "hi");
    }
}
