//! JSON-RPC 2.0 over WebSocket（一帧一条文本消息）。

use crate::jsonrpc::{dispatch_rpc_value, Incoming, PendingMap};
use anyhow::Context;
use futures::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type WsStream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
type WsSink = futures::stream::SplitSink<WsStream, Message>;

/// 长连接 WebSocket JSON-RPC（对标 stdio 帧语义）。
pub struct WsRpc {
    pending: PendingMap,
    write: Arc<Mutex<WsSink>>,
    next_id: AtomicI64,
    incoming: Mutex<Option<mpsc::UnboundedReceiver<Incoming>>>,
    _reader_task: tokio::task::JoinHandle<()>,
    _route_task: tokio::task::JoinHandle<()>,
}

impl WsRpc {
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        let (ws, _) = connect_async(url)
            .await
            .with_context(|| format!("websocket connect {url}"))?;
        let (sink, mut stream) = ws.split();
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (tx_lines, mut rx_lines) = mpsc::unbounded_channel::<String>();
        let reader_task = tokio::spawn(async move {
            while let Some(msg) = stream.next().await {
                match msg {
                    Ok(Message::Text(t)) => {
                        let t = t.to_string();
                        if !t.trim().is_empty() {
                            let _ = tx_lines.send(t);
                        }
                    }
                    Ok(Message::Binary(b)) => {
                        if let Ok(t) = std::str::from_utf8(b.as_ref()) {
                            if !t.trim().is_empty() {
                                let _ = tx_lines.send(t.to_string());
                            }
                        }
                    }
                    Ok(Message::Close(_)) | Err(_) => break,
                    _ => {}
                }
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
                dispatch_rpc_value(v, &pending_route, &tx_in).await;
            }
        });

        Ok(Self {
            pending,
            write: Arc::new(Mutex::new(sink)),
            next_id: AtomicI64::new(1),
            incoming: Mutex::new(Some(rx_in)),
            _reader_task: reader_task,
            _route_task: route_task,
        })
    }

    pub async fn take_incoming(&self) -> Option<mpsc::UnboundedReceiver<Incoming>> {
        self.incoming.lock().await.take()
    }

    pub async fn write_json(&self, v: &serde_json::Value) -> anyhow::Result<()> {
        let mut guard = self.write.lock().await;
        guard
            .send(Message::Text(v.to_string().into()))
            .await
            .context("websocket send")?;
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
        self.write_json(&req).await?;
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
        self.write_json(&req).await
    }
}

impl Drop for WsRpc {
    fn drop(&mut self) {
        self._reader_task.abort();
        self._route_task.abort();
    }
}
