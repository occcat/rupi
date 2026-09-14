//! 长连接 JSON-RPC 扩展宿主：复用 `rupi_mcp::StdioRpc` 帧。
//! 扩展可 `initialize` 注册命令、订阅事件、在 `tools/call` 结果里带 UI 提示。

use crate::{block_on_async, effective_timeout_secs, ExtensionManifest};
use rupi_core::{agent_event_to_rpc_json, AgentEvent, Extension, ExtensionCommand, ToolDefinition};
use rupi_mcp::{Incoming, StdioRpc};
use rupi_tools::{Tool, ToolOutput, UiHint};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::oneshot;

pub struct RpcHost {
    pub manifest: ExtensionManifest,
    rpc: Arc<StdioRpc>,
    commands: Mutex<Vec<ExtensionCommand>>,
    subscribe: Mutex<HashSet<String>>,
    hints: Mutex<Vec<UiHint>>,
    ui_out: Mutex<Vec<AgentEvent>>,
    ui_wait: Mutex<HashMap<String, oneshot::Sender<serde_json::Value>>>,
}

impl RpcHost {
    pub fn start(manifest: ExtensionManifest) -> anyhow::Result<Arc<Self>> {
        let timeout = std::time::Duration::from_secs(effective_timeout_secs(manifest.timeout_secs));
        block_on_async(async { Self::start_async(manifest, timeout).await })
    }

    async fn start_async(
        manifest: ExtensionManifest,
        timeout: std::time::Duration,
    ) -> anyhow::Result<Arc<Self>> {
        let rpc =
            Arc::new(StdioRpc::spawn(&manifest.command, &manifest.args, &manifest.env).await?);
        let mut incoming = rpc.take_incoming().await.unwrap();
        let host = Arc::new(Self {
            commands: Mutex::new(manifest.commands.clone()),
            subscribe: Mutex::new(manifest.subscribe.iter().cloned().collect()),
            hints: Mutex::new(Vec::new()),
            ui_out: Mutex::new(Vec::new()),
            ui_wait: Mutex::new(HashMap::new()),
            manifest,
            rpc,
        });
        let peer = host.clone();
        tokio::spawn(async move {
            while let Some(msg) = incoming.recv().await {
                peer.handle_incoming(msg).await;
            }
        });
        match host
            .rpc
            .call_timeout(
                "initialize",
                serde_json::json!({
                    "protocolVersion": "1",
                    "clientInfo": {"name": "rupi", "version": "0.1.0"},
                }),
                timeout,
            )
            .await
        {
            Ok(caps) => host.apply_initialize(&caps),
            Err(e) => tracing::debug!(
                "ext {} initialize skipped ({e:#}); using manifest capabilities",
                host.manifest.name
            ),
        }
        let _ = host
            .rpc
            .notify(
                "notifications/session",
                serde_json::json!({"kind": "session_start", "name": host.manifest.name}),
            )
            .await;
        let _ = host
            .rpc
            .notify(
                "notifications/event",
                serde_json::json!({"tags": ["session_start"], "event": {"kind": "session_start"}}),
            )
            .await;
        Ok(host)
    }

    fn apply_initialize(&self, caps: &serde_json::Value) {
        if let Some(cmds) = caps
            .pointer("/capabilities/commands")
            .and_then(|c| c.as_array())
        {
            let mut out = Vec::new();
            for c in cmds {
                if let Some(name) = c.get("name").and_then(|n| n.as_str()) {
                    out.push(ExtensionCommand {
                        name: name.to_string(),
                        description: c
                            .get("description")
                            .and_then(|d| d.as_str())
                            .unwrap_or("")
                            .to_string(),
                    });
                }
            }
            if !out.is_empty() {
                *self.commands.lock().unwrap() = out;
            }
        }
        if let Some(evs) = caps
            .pointer("/capabilities/events")
            .or_else(|| caps.pointer("/capabilities/subscribe"))
            .and_then(|c| c.as_array())
        {
            let mut set = self.subscribe.lock().unwrap();
            for e in evs {
                if let Some(s) = e.as_str() {
                    set.insert(s.to_string());
                }
            }
        }
    }

    async fn handle_incoming(&self, msg: Incoming) {
        match msg {
            Incoming::Notification { method, params } => {
                if method == "ui/hint" || method == "notifications/ui" {
                    self.push_hint(&params);
                } else if is_ui_dialog_method(&method) {
                    let _ = self.push_dialog(&method, &params, false);
                }
            }
            Incoming::Request { id, method, params } => match method.as_str() {
                "registerProvider" => match register_provider_params(&params) {
                    Ok(p) => {
                        let _ = self
                            .rpc
                            .respond(id, serde_json::json!({"ok": true, "name": p.name}))
                            .await;
                    }
                    Err(e) => {
                        let _ = self.rpc.respond_error(id, -32602, e.to_string()).await;
                    }
                },
                "registerKeybinding" => {
                    if let (Some(action), Some(key)) = (
                        params.get("action").and_then(|a| a.as_str()),
                        params
                            .get("key")
                            .or_else(|| params.get("keys"))
                            .and_then(|k| k.as_str()),
                    ) {
                        push_keybinding(action, key);
                    }
                    let _ = self.rpc.respond(id, serde_json::json!({"ok": true})).await;
                }
                "registerCommand" => {
                    if let Some(name) = params.get("name").and_then(|n| n.as_str()) {
                        self.commands.lock().unwrap().push(ExtensionCommand {
                            name: name.to_string(),
                            description: params
                                .get("description")
                                .and_then(|d| d.as_str())
                                .unwrap_or("")
                                .to_string(),
                        });
                    }
                    let _ = self.rpc.respond(id, serde_json::json!({"ok": true})).await;
                }
                "subscribe" => {
                    if let Some(arr) = params.get("events").and_then(|e| e.as_array()) {
                        let mut set = self.subscribe.lock().unwrap();
                        for e in arr {
                            if let Some(s) = e.as_str() {
                                set.insert(s.to_string());
                            }
                        }
                    }
                    let _ = self.rpc.respond(id, serde_json::json!({"ok": true})).await;
                }
                "ui/hint" => {
                    self.push_hint(&params);
                    let _ = self.rpc.respond(id, serde_json::json!({"ok": true})).await;
                }
                other if is_ui_dialog_method(other) => {
                    let wait = self.push_dialog(other, &params, true);
                    let reply = match wait {
                        Some(rx) => {
                            let ms = params
                                .get("timeout")
                                .and_then(|t| t.as_u64())
                                .unwrap_or(30_000)
                                .clamp(1, 300_000);
                            match tokio::time::timeout(std::time::Duration::from_millis(ms), rx)
                                .await
                            {
                                Ok(Ok(v)) => v,
                                _ => dialog_timeout_reply(&ui_method_name(other, &params)),
                            }
                        }
                        None => serde_json::json!({"ok": true}),
                    };
                    let _ = self.rpc.respond(id, reply).await;
                }
                _ => {
                    let _ = self
                        .rpc
                        .respond_error(id, -32601, format!("method not found: {method}"))
                        .await;
                }
            },
        }
    }

    fn push_hint(&self, params: &serde_json::Value) {
        let message = params
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        if message.is_empty() {
            return;
        }
        self.hints.lock().unwrap().push(UiHint {
            kind: params
                .get("kind")
                .and_then(|k| k.as_str())
                .unwrap_or("note")
                .to_string(),
            message,
        });
    }

    pub fn drain_hints(&self) -> Vec<UiHint> {
        self.hints.lock().unwrap().drain(..).collect()
    }

    /// 排队一条扩展 UI dialog，并可选地等宿主 `extension_ui_response`。
    fn push_dialog(
        &self,
        method: &str,
        params: &serde_json::Value,
        wait: bool,
    ) -> Option<oneshot::Receiver<serde_json::Value>> {
        let ev = dialog_event(&self.manifest.name, method, params);
        let id = match &ev {
            AgentEvent::ExtensionUiRequest { id, .. } => id.clone(),
            _ => return None,
        };
        if ev_is_notify(&ev) {
            if let AgentEvent::ExtensionUiRequest {
                message: Some(msg),
                notify_type,
                ..
            } = &ev
            {
                self.hints.lock().unwrap().push(UiHint {
                    kind: notify_type.clone().unwrap_or_else(|| "notify".into()),
                    message: msg.clone(),
                });
            }
            self.ui_out.lock().unwrap().push(ev);
            return None;
        }
        let rx = if wait {
            let (tx, rx) = oneshot::channel();
            self.ui_wait.lock().unwrap().insert(id, tx);
            Some(rx)
        } else {
            None
        };
        self.ui_out.lock().unwrap().push(ev);
        rx
    }

    pub fn command_list(&self) -> Vec<ExtensionCommand> {
        self.commands.lock().unwrap().clone()
    }

    pub fn expand_command(&self, name: &str, args: &str) -> Option<String> {
        let known = self.commands.lock().unwrap().iter().any(|c| c.name == name);
        if !known {
            return None;
        }
        let timeout =
            std::time::Duration::from_secs(effective_timeout_secs(self.manifest.timeout_secs));
        match block_on_async(self.rpc.call_timeout(
            "commands/execute",
            serde_json::json!({"name": name, "args": args}),
            timeout,
        )) {
            Ok(v) => v
                .get("prompt")
                .and_then(|p| p.as_str())
                .map(|s| s.to_string())
                .or_else(|| Some(format!("[ext /{name}] {args}"))),
            Err(e) => {
                tracing::warn!("ext {} command /{name} failed: {e:#}", self.manifest.name);
                Some(format!("[ext /{name}] {args}"))
            }
        }
    }

    pub fn call_tool(&self, arguments: serde_json::Value) -> ToolOutput {
        let timeout =
            std::time::Duration::from_secs(effective_timeout_secs(self.manifest.timeout_secs));
        match block_on_async(self.rpc.call_timeout(
            "tools/call",
            serde_json::json!({"name": self.manifest.name, "arguments": arguments}),
            timeout,
        )) {
            Ok(v) => parse_tool_result(&self.manifest.name, v),
            Err(e) => ToolOutput::err(format!("ext {}: {e:#}", self.manifest.name)),
        }
    }
}

fn event_tags(event: &AgentEvent) -> Vec<&'static str> {
    match event {
        AgentEvent::ToolStart { .. } => vec!["tool_call", "tool_start"],
        AgentEvent::ToolEnd { .. } => vec!["tool_end", "tool_result"],
        AgentEvent::TurnEnd { .. } => vec!["turn_end"],
        AgentEvent::TurnStart { turn } => {
            if *turn == 1 {
                vec!["session_start", "turn_start"]
            } else {
                vec!["turn_start"]
            }
        }
        AgentEvent::RunEnd { .. } => vec!["run_end", "session_end"],
        AgentEvent::UiHint { .. } => vec!["ui_hint"],
        AgentEvent::ExtensionUiRequest { .. } => vec!["extension_ui", "ui_dialog"],
        AgentEvent::AutoRetryStart { .. } | AgentEvent::AutoRetryEnd { .. } => {
            vec!["auto_retry"]
        }
        AgentEvent::ModelChange { .. } => vec!["model_change"],
        _ => vec![],
    }
}

fn is_ui_dialog_method(method: &str) -> bool {
    matches!(
        method,
        "ui/dialog"
            | "ui/select"
            | "ui/confirm"
            | "ui/input"
            | "ui/editor"
            | "ui/notify"
            | "extension_ui_request"
            | "select"
            | "confirm"
            | "input"
            | "editor"
            | "notify"
    )
}

fn ui_method_name(method: &str, params: &serde_json::Value) -> String {
    if let Some(m) = params.get("method").and_then(|m| m.as_str()) {
        if !m.is_empty() {
            return m.to_string();
        }
    }
    method.rsplit('/').next().unwrap_or(method).to_string()
}

fn ev_is_notify(ev: &AgentEvent) -> bool {
    matches!(
        ev,
        AgentEvent::ExtensionUiRequest { method, .. }
            if matches!(method.as_str(), "notify" | "setStatus" | "setWidget" | "setTitle" | "set_editor_text")
    )
}

fn dialog_timeout_reply(method: &str) -> serde_json::Value {
    if method == "confirm" {
        serde_json::json!({"confirmed": false, "cancelled": true})
    } else {
        serde_json::json!({"cancelled": true})
    }
}

fn dialog_event(source: &str, method: &str, params: &serde_json::Value) -> AgentEvent {
    let method = ui_method_name(method, params);
    let id = params
        .get("id")
        .and_then(|i| i.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("ui-{}-{source}", next_ui_seq()));
    let options = params
        .get("options")
        .and_then(|o| o.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let message = params
        .get("message")
        .and_then(|m| m.as_str())
        .map(str::to_string);
    let _ = source;
    AgentEvent::ExtensionUiRequest {
        id,
        method,
        title: params
            .get("title")
            .and_then(|t| t.as_str())
            .map(str::to_string),
        message,
        options,
        timeout: params.get("timeout").and_then(|t| t.as_u64()),
        placeholder: params
            .get("placeholder")
            .and_then(|p| p.as_str())
            .map(str::to_string),
        prefill: params
            .get("prefill")
            .and_then(|p| p.as_str())
            .map(str::to_string),
        notify_type: params
            .get("notifyType")
            .or_else(|| params.get("notify_type"))
            .and_then(|n| n.as_str())
            .map(str::to_string),
    }
}

fn next_ui_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(1);
    N.fetch_add(1, Ordering::Relaxed)
}

fn register_provider_params(params: &serde_json::Value) -> anyhow::Result<rupi_llm::ExtraProvider> {
    let name = params
        .get("name")
        .or_else(|| params.get("id"))
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .to_string();
    let protocol = params
        .get("protocol")
        .or_else(|| params.get("compat"))
        .and_then(|p| p.as_str())
        .unwrap_or("openai")
        .to_string();
    let base_url = params
        .get("base_url")
        .or_else(|| params.get("baseUrl"))
        .and_then(|b| b.as_str())
        .unwrap_or("")
        .to_string();
    let api_key_env = params
        .get("api_key_env")
        .or_else(|| params.get("apiKeyEnv"))
        .and_then(|s| s.as_str())
        .map(|s| s.to_string());
    let models = params
        .get("models")
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    let id = m.get("id").and_then(|i| i.as_str())?.to_string();
                    Some(rupi_llm::ModelEntry {
                        id: id.clone(),
                        provider: m
                            .get("provider")
                            .and_then(|p| p.as_str())
                            .unwrap_or(&name)
                            .to_string(),
                        name: m
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or(&id)
                            .to_string(),
                        context: m
                            .get("context")
                            .or_else(|| m.get("contextWindow"))
                            .and_then(|c| c.as_u64())
                            .unwrap_or(0),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    rupi_llm::register_extra_provider(rupi_llm::ExtraProvider {
        name,
        protocol,
        base_url,
        api_key_env,
        models,
    })
}

#[derive(Debug, Clone)]
pub struct RegisteredKeybinding {
    pub action: String,
    pub key: String,
}

fn keybind_lock() -> &'static Mutex<Vec<RegisteredKeybinding>> {
    static LOCK: OnceLock<Mutex<Vec<RegisteredKeybinding>>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(Vec::new()))
}

fn push_keybinding(action: &str, key: &str) {
    let mut g = keybind_lock().lock().unwrap();
    if let Some(slot) = g.iter_mut().find(|k| k.action == action) {
        slot.key = key.to_string();
    } else {
        g.push(RegisteredKeybinding {
            action: action.to_string(),
            key: key.to_string(),
        });
    }
}

pub fn registered_keybindings() -> Vec<RegisteredKeybinding> {
    keybind_lock().lock().unwrap().clone()
}

fn wants(sub: &HashSet<String>, tags: &[&str]) -> bool {
    if sub.is_empty() {
        return false;
    }
    if sub.contains("*") {
        return true;
    }
    tags.iter().any(|t| sub.contains(*t))
}

#[async_trait::async_trait]
impl Extension for RpcHost {
    fn name(&self) -> &str {
        &self.manifest.name
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![self.manifest.definition()]
    }

    fn commands(&self) -> Vec<ExtensionCommand> {
        self.command_list()
    }

    async fn on_event(&self, event: &AgentEvent) -> anyhow::Result<()> {
        let tags = event_tags(event);
        let sub = self.subscribe.lock().unwrap().clone();
        if !wants(&sub, &tags) {
            return Ok(());
        }
        let payload = agent_event_to_rpc_json(event);
        let _ = self
            .rpc
            .notify(
                "notifications/event",
                serde_json::json!({"tags": tags, "event": payload}),
            )
            .await;
        Ok(())
    }

    fn poll_ui_requests(&self) -> Vec<AgentEvent> {
        self.ui_out.lock().unwrap().drain(..).collect()
    }

    fn complete_ui_request(&self, id: &str, reply: serde_json::Value) -> bool {
        if let Some(tx) = self.ui_wait.lock().unwrap().remove(id) {
            let _ = tx.send(reply);
            true
        } else {
            false
        }
    }
}

pub struct ExternalRpcTool {
    host: Arc<RpcHost>,
}

impl ExternalRpcTool {
    pub fn new(host: Arc<RpcHost>) -> Self {
        Self { host }
    }
}

#[async_trait::async_trait]
impl Tool for ExternalRpcTool {
    fn definition(&self) -> ToolDefinition {
        self.host.manifest.definition()
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
    ) -> anyhow::Result<rupi_tools::ToolOutput> {
        Ok(self.host.call_tool(arguments))
    }
}

fn parse_tool_result(source: &str, v: serde_json::Value) -> ToolOutput {
    if let Some(s) = v.as_str() {
        return ToolOutput::ok(s);
    }
    let content = v
        .get("content")
        .and_then(|c| {
            if let Some(s) = c.as_str() {
                return Some(s.to_string());
            }
            c.as_array().map(|arr| {
                arr.iter()
                    .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("")
            })
        })
        .unwrap_or_else(|| v.to_string());
    let is_error = v
        .get("isError")
        .or_else(|| v.get("is_error"))
        .and_then(|b| b.as_bool())
        .unwrap_or(false);
    let mut out = if is_error {
        ToolOutput::err(content)
    } else {
        ToolOutput::ok(content)
    };
    if let Some(ui) = v.get("ui") {
        if let Some(message) = ui.get("message").and_then(|m| m.as_str()) {
            out = out.with_ui_hint(UiHint {
                kind: ui
                    .get("kind")
                    .and_then(|k| k.as_str())
                    .unwrap_or("note")
                    .to_string(),
                message: message.to_string(),
            });
            let _ = source;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_provider_requires_name_and_url() {
        let err = register_provider_params(&serde_json::json!({"name": "x"})).unwrap_err();
        assert!(err.to_string().contains("base_url"), "{err:#}");
    }

    #[test]
    fn register_keybinding_overrides_same_action() {
        push_keybinding("send", "ctrl+s");
        push_keybinding("send", "ctrl+enter");
        let got = registered_keybindings();
        let send = got.iter().find(|k| k.action == "send").unwrap();
        assert_eq!(send.key, "ctrl+enter");
    }

    #[test]
    fn event_tags_include_session_start_on_first_turn() {
        let first = event_tags(&AgentEvent::TurnStart { turn: 1 });
        assert!(first.contains(&"session_start"));
        assert!(first.contains(&"turn_start"));
        let later = event_tags(&AgentEvent::TurnStart { turn: 2 });
        assert!(!later.contains(&"session_start"));
        let model = event_tags(&AgentEvent::ModelChange {
            provider: "openai".into(),
            model: "gpt-4o-mini".into(),
        });
        assert_eq!(model, vec!["model_change"]);
    }

    #[test]
    fn dialog_event_matches_pi_extension_ui_request() {
        let ev = dialog_event(
            "demo",
            "ui/select",
            &serde_json::json!({
                "id": "uuid-1",
                "title": "Allow dangerous command?",
                "options": ["Allow", "Block"],
                "timeout": 10000
            }),
        );
        let v = agent_event_to_rpc_json(&ev);
        assert_eq!(v["type"], "extension_ui_request");
        assert_eq!(v["id"], "uuid-1");
        assert_eq!(v["method"], "select");
        assert_eq!(v["options"][1], "Block");
        assert_eq!(v["timeout"], 10000);
        assert_eq!(event_tags(&ev), vec!["extension_ui", "ui_dialog"]);
    }
}
