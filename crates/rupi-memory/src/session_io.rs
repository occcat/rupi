//! Pi JSONL v3 会话导入/导出 + 简易 HTML。
//! 对标 `packages/coding-agent/docs/session-format.md`（header + message/compaction/session_info）。

use crate::{SessionRecord, SessionStore};
use rupi_core::{ContentBlock, Message, Role, SessionTree};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use uuid::Uuid;

const SESSION_FORMAT_VERSION: u32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionHeader {
    #[serde(rename = "type")]
    pub kind: String,
    pub version: u32,
    pub id: String,
    pub timestamp: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

impl SessionHeader {
    pub fn new(id: impl Into<String>, cwd: impl Into<String>) -> Self {
        Self {
            kind: "session".into(),
            version: SESSION_FORMAT_VERSION,
            id: id.into(),
            timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            cwd: cwd.into(),
            parent_session: None,
            metadata: None,
        }
    }
}

/// 从内存树导出 Pi JSONL（含当前路径上的全部节点；旁支一并写出以便 /clone 往返）。
pub fn export_tree_jsonl(
    tree: &SessionTree,
    cwd: &str,
    name: Option<&str>,
    parent_session: Option<&str>,
) -> String {
    let mut header = SessionHeader::new(&tree.id, cwd);
    header.parent_session = parent_session.map(|s| s.to_string());
    if let Some(n) = name {
        header.metadata = Some(serde_json::json!({"name": n}));
    }
    let mut lines = vec![serde_json::to_string(&header).unwrap_or_else(|_| "{}".into())];
    let mut ids: Vec<&String> = tree.nodes.keys().collect();
    ids.sort_by_key(|id| tree.nodes[*id].created_at);
    let mut prev_jsonl_id: Option<String> = None;
    for id in ids {
        let node = &tree.nodes[id];
        let parent = node.parent.clone().or_else(|| prev_jsonl_id.clone());
        for (entry_id, msg_json) in message_entries(id, parent.as_deref(), &node.message) {
            lines.push(msg_json);
            prev_jsonl_id = Some(entry_id);
        }
    }
    if let Some(summary) = &tree.summary {
        let through = tree.summary_through.as_deref();
        let first_kept = through.and_then(|t| {
            tree.current_path
                .iter()
                .position(|id| id == t)
                .and_then(|i| tree.current_path.get(i + 1))
                .cloned()
        });
        if let Some(fk) = first_kept {
            let parent = through.unwrap_or("").to_string();
            let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
            lines.push(
                serde_json::json!({
                    "type": "compaction",
                    "id": Uuid::new_v4().to_string(),
                    "parentId": parent,
                    "timestamp": ts,
                    "summary": summary,
                    "firstKeptEntryId": fk,
                    "tokensBefore": tree.history_tokens(),
                })
                .to_string(),
            );
        }
    }
    if let Some(n) = name {
        let parent = tree.current_path.last().cloned().unwrap_or_default();
        let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        lines.push(
            serde_json::json!({
                "type": "session_info",
                "id": Uuid::new_v4().to_string(),
                "parentId": parent,
                "timestamp": ts,
                "name": n,
            })
            .to_string(),
        );
    }
    lines.join("\n") + "\n"
}

fn message_entries(id: &str, parent: Option<&str>, msg: &Message) -> Vec<(String, String)> {
    let ts = msg
        .created_at
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let ms = msg.created_at.timestamp_millis();
    match msg.role {
        Role::Tool => {
            let mut out = Vec::new();
            let mut last_parent = parent.map(|s| s.to_string());
            for (i, b) in msg.blocks.iter().enumerate() {
                let ContentBlock::ToolResult {
                    tool_call_id,
                    content,
                    is_error,
                } = b
                else {
                    continue;
                };
                let eid = if i == 0 {
                    id.to_string()
                } else {
                    Uuid::new_v4().to_string()
                };
                let line = serde_json::json!({
                    "type": "message",
                    "id": eid,
                    "parentId": last_parent,
                    "timestamp": ts,
                    "message": {
                        "role": "toolResult",
                        "toolCallId": tool_call_id,
                        "toolName": "tool",
                        "content": [{"type": "text", "text": content}],
                        "isError": is_error,
                        "timestamp": ms,
                    }
                })
                .to_string();
                last_parent = Some(eid.clone());
                out.push((eid, line));
            }
            if out.is_empty() {
                out.push((
                    id.to_string(),
                    linear_message_line(id, parent, "user", &msg.full_text(), ts, ms),
                ));
            }
            out
        }
        Role::User => vec![(
            id.to_string(),
            linear_message_line(id, parent, "user", &msg.full_text(), ts, ms),
        )],
        Role::System => vec![(
            id.to_string(),
            serde_json::json!({
                "type": "custom_message",
                "id": id,
                "parentId": parent,
                "timestamp": ts,
                "customType": "rupi.system",
                "content": msg.full_text(),
                "display": true,
            })
            .to_string(),
        )],
        Role::Assistant => {
            let content: Vec<Value> = msg
                .blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => {
                        Some(serde_json::json!({"type": "text", "text": text}))
                    }
                    ContentBlock::Thinking { text, signature } => {
                        let mut o = serde_json::json!({"type": "thinking", "thinking": text});
                        if let Some(sig) = signature {
                            o["thinkingSignature"] = Value::String(sig.clone());
                        }
                        Some(o)
                    }
                    ContentBlock::ToolCall {
                        id,
                        name,
                        arguments,
                    } => Some(serde_json::json!({
                        "type": "toolCall",
                        "id": id,
                        "name": name,
                        "arguments": arguments,
                    })),
                    ContentBlock::ToolResult { .. }
                    | ContentBlock::RedactedThinking { .. }
                    | ContentBlock::Image { .. } => None,
                })
                .collect();
            vec![(
                id.to_string(),
                serde_json::json!({
                    "type": "message",
                    "id": id,
                    "parentId": parent,
                    "timestamp": ts,
                    "message": {
                        "role": "assistant",
                        "content": content,
                        "api": "openai-completions",
                        "provider": msg.provider.as_deref().unwrap_or("rupi"),
                        "model": "",
                        "usage": zero_usage(),
                        "stopReason": "stop",
                        "timestamp": ms,
                    }
                })
                .to_string(),
            )]
        }
    }
}

fn linear_message_line(
    id: &str,
    parent: Option<&str>,
    role: &str,
    text: &str,
    ts: String,
    ms: i64,
) -> String {
    serde_json::json!({
        "type": "message",
        "id": id,
        "parentId": parent,
        "timestamp": ts,
        "message": {"role": role, "content": text, "timestamp": ms}
    })
    .to_string()
}

fn zero_usage() -> Value {
    serde_json::json!({
        "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0,
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}
    })
}

/// 解析 JSONL → 树 + 可选名/父会话。坏行跳过。
pub fn import_jsonl(jsonl: &str) -> anyhow::Result<ImportedSession> {
    let mut header: Option<SessionHeader> = None;
    let mut tree = SessionTree::new();
    tree.nodes.clear();
    tree.current_path.clear();
    let mut name = None;
    let mut last_msg_id = None;
    for (i, raw) in jsonl.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value =
            serde_json::from_str(line).map_err(|e| anyhow::anyhow!("jsonl line {}: {e}", i + 1))?;
        let typ = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match typ {
            "session" => {
                header = serde_json::from_value(v).ok();
                if let Some(h) = &header {
                    tree.id = h.id.clone();
                    if let Some(n) = h
                        .metadata
                        .as_ref()
                        .and_then(|m| m.get("name"))
                        .and_then(|x| x.as_str())
                    {
                        name = Some(n.to_string());
                    }
                }
            }
            "message" => {
                let id = v
                    .get("id")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                if id.is_empty() {
                    continue;
                }
                let parent = v
                    .get("parentId")
                    .and_then(|x| x.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string());
                let Some(msg_v) = v.get("message") else {
                    continue;
                };
                let message = pi_message_to_rupi(msg_v);
                tree.nodes.insert(
                    id.clone(),
                    rupi_core::SessionNode {
                        id: id.clone(),
                        parent,
                        message,
                        summary: None,
                        created_at: chrono::Utc::now(),
                    },
                );
                last_msg_id = Some(id);
            }
            "custom_message" => {
                let id = v
                    .get("id")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                if id.is_empty() {
                    continue;
                }
                let parent = v
                    .get("parentId")
                    .and_then(|x| x.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string());
                let text = match v.get("content") {
                    Some(Value::String(s)) => s.clone(),
                    Some(Value::Array(a)) => a
                        .iter()
                        .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => continue,
                };
                tree.nodes.insert(
                    id.clone(),
                    rupi_core::SessionNode {
                        id: id.clone(),
                        parent,
                        message: Message::text(Role::System, text),
                        summary: None,
                        created_at: chrono::Utc::now(),
                    },
                );
                last_msg_id = Some(id);
            }
            "compaction" => {
                if let Some(s) = v.get("summary").and_then(|x| x.as_str()) {
                    if let Some(fk) = v.get("firstKeptEntryId").and_then(|x| x.as_str()) {
                        let through = tree
                            .nodes
                            .get(fk)
                            .and_then(|n| n.parent.clone())
                            .unwrap_or_else(|| fk.to_string());
                        tree.summary = Some(s.to_string());
                        tree.summary_through = Some(through);
                    }
                }
            }
            "session_info" => {
                if let Some(n) = v.get("name").and_then(|x| x.as_str()) {
                    name = Some(n.to_string());
                }
            }
            _ => {}
        }
    }
    if tree.nodes.is_empty() {
        anyhow::bail!("jsonl contained no messages");
    }
    let leaf = last_msg_id.or_else(|| tree.nodes.keys().next().cloned());
    if let Some(leaf) = leaf {
        tree.goto_node(&leaf);
    }
    Ok(ImportedSession { header, tree, name })
}

#[derive(Debug)]
pub struct ImportedSession {
    pub header: Option<SessionHeader>,
    pub tree: SessionTree,
    pub name: Option<String>,
}

fn pi_message_to_rupi(v: &Value) -> Message {
    let role = v.get("role").and_then(|r| r.as_str()).unwrap_or("user");
    match role {
        "assistant" => {
            let blocks = pi_content_blocks(v.get("content"));
            Message {
                id: Uuid::new_v4().to_string(),
                role: Role::Assistant,
                blocks: if blocks.is_empty() {
                    vec![ContentBlock::Text {
                        text: String::new(),
                    }]
                } else {
                    blocks
                },
                provider: v
                    .get("provider")
                    .and_then(|p| p.as_str())
                    .map(|s| s.to_string()),
                created_at: chrono::Utc::now(),
            }
        }
        "toolResult" => {
            let id = v
                .get("toolCallId")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let text = content_text(v.get("content"));
            let is_error = v.get("isError").and_then(|x| x.as_bool()).unwrap_or(false);
            Message {
                id: Uuid::new_v4().to_string(),
                role: Role::Tool,
                blocks: vec![ContentBlock::ToolResult {
                    tool_call_id: id,
                    content: text,
                    is_error,
                }],
                provider: None,
                created_at: chrono::Utc::now(),
            }
        }
        _ => Message::text(Role::User, content_text(v.get("content"))),
    }
}

fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|b| {
                b.get("text")
                    .and_then(|t| t.as_str())
                    .or_else(|| b.get("thinking").and_then(|t| t.as_str()))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn pi_content_blocks(content: Option<&Value>) -> Vec<ContentBlock> {
    let Some(Value::Array(a)) = content else {
        if let Some(Value::String(s)) = content {
            return vec![ContentBlock::Text { text: s.clone() }];
        }
        return vec![];
    };
    a.iter()
        .filter_map(|b| {
            let t = b.get("type").and_then(|x| x.as_str()).unwrap_or("");
            match t {
                "text" => Some(ContentBlock::Text {
                    text: b.get("text").and_then(|x| x.as_str()).unwrap_or("").into(),
                }),
                "thinking" => Some(ContentBlock::Thinking {
                    text: b
                        .get("thinking")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .into(),
                    signature: b
                        .get("thinkingSignature")
                        .and_then(|x| x.as_str())
                        .map(|s| s.to_string()),
                }),
                "toolCall" => Some(ContentBlock::ToolCall {
                    id: b.get("id").and_then(|x| x.as_str()).unwrap_or("").into(),
                    name: b.get("name").and_then(|x| x.as_str()).unwrap_or("").into(),
                    arguments: b.get("arguments").cloned().unwrap_or(Value::Null),
                }),
                _ => None,
            }
        })
        .collect()
}

pub fn export_tree_html(tree: &SessionTree, title: &str) -> String {
    let mut body = String::new();
    for id in &tree.current_path {
        let Some(n) = tree.nodes.get(id) else {
            continue;
        };
        let role = match n.message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
            Role::System => "system",
        };
        body.push_str(&format!(
            "<div class=\"{role}\"><div class=\"role\">{role}</div><pre>{}</pre></div>\n",
            esc(&n.message.full_text())
        ));
    }
    format!(
        "<!DOCTYPE html><html lang=\"zh\"><head><meta charset=\"utf-8\"><title>{}</title>\
<style>body{{font-family:sans-serif;max-width:52rem;margin:2rem auto;padding:0 1rem}}\
.user{{background:#e8f4ff;padding:.6rem 1rem;margin:.6rem 0;border-radius:6px}}\
.assistant{{background:#f4f4f4;padding:.6rem 1rem;margin:.6rem 0;border-radius:6px}}\
.tool,.system{{background:#fff8e0;padding:.6rem 1rem;margin:.6rem 0;border-radius:6px}}\
.role{{font-size:.75rem;color:#666;text-transform:uppercase}}\
pre{{white-space:pre-wrap;margin:.3rem 0 0}}</style></head><body><h1>{}</h1>\n{body}</body></html>\n",
        esc(title),
        esc(title)
    )
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// 把树落进已存在的 session 行（新节点 id，避免全局 messages.id PK 冲突）。
pub fn persist_tree(
    store: &SessionStore,
    session_id: &str,
    tree: &SessionTree,
) -> anyhow::Result<usize> {
    let mut order: Vec<&String> = tree.nodes.keys().collect();
    order.sort_by_key(|id| tree.nodes[*id].created_at);
    let mut rows = Vec::new();
    for id in order {
        let Some(node) = tree.nodes.get(id) else {
            continue;
        };
        let role = match node.message.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        rows.push(crate::SessionMessageRow {
            id: id.clone(),
            role: role.into(),
            content: node.message.full_text(),
            blocks: serde_json::to_string(&node.message).ok(),
        });
    }
    let n = rows.len();
    store.persist_turn(session_id, &rows, tree.summary.as_deref())?;
    Ok(n)
}

/// 从库行导出（resume 形状）。
pub fn export_store_jsonl(
    store: &SessionStore,
    session_id: &str,
    cwd: &str,
) -> anyhow::Result<String> {
    let recs = store.session_records(session_id, 10_000)?;
    if recs.is_empty() && !store.has_session(session_id)? {
        anyhow::bail!("unknown session: {session_id}");
    }
    let mut tree = SessionTree::new();
    tree.id = session_id.to_string();
    for rec in recs {
        tree.push_with_id(rec.id.clone(), rec.to_message());
    }
    let stored = store.get_summary(session_id).unwrap_or_default();
    if !stored.is_empty() {
        if let Some(first) = tree.current_path.first().cloned() {
            tree.summary = Some(stored);
            tree.summary_through = Some(first);
        }
    }
    let name = store.get_name(session_id).ok().flatten();
    let parent = store.get_parent_session(session_id).ok().flatten();
    Ok(export_tree_jsonl(
        &tree,
        cwd,
        name.as_deref(),
        parent.as_deref(),
    ))
}

pub fn export_store_html(store: &SessionStore, session_id: &str) -> anyhow::Result<String> {
    let recs = store.session_records(session_id, 10_000)?;
    let mut tree = SessionTree::new();
    tree.id = session_id.to_string();
    for rec in recs {
        tree.push_with_id(rec.id.clone(), rec.to_message());
    }
    let title = store
        .get_name(session_id)
        .ok()
        .flatten()
        .unwrap_or_else(|| format!("session {session_id}"));
    Ok(export_tree_html(&tree, &title))
}

/// 导入 JSONL 为新会话（新 id，避免与库内冲突）。
pub fn import_into_store(
    store: &SessionStore,
    jsonl: &str,
    cwd: &str,
) -> anyhow::Result<(String, SessionTree)> {
    let imported = import_jsonl(jsonl)?;
    let sid = store.create_session_ex(
        "imported",
        imported.name.as_deref(),
        Some(cwd),
        imported
            .header
            .as_ref()
            .and_then(|h| h.parent_session.as_deref()),
    )?;
    let mut tree = remap_tree(&imported.tree, false);
    persist_tree(store, &sid, &tree)?;
    if let Some(n) = &imported.name {
        store.set_name(&sid, n)?;
    }
    tree.id = sid.clone();
    Ok((sid, tree))
}

/// 复制树并重映射节点 id（`path_only` = /fork 只带当前路径）。
pub fn remap_tree(src: &SessionTree, path_only: bool) -> SessionTree {
    let ids: Vec<String> = if path_only {
        src.current_path.clone()
    } else {
        let mut v: Vec<String> = src.nodes.keys().cloned().collect();
        v.sort();
        v
    };
    let map: HashMap<String, String> = ids
        .iter()
        .map(|id| (id.clone(), Uuid::new_v4().to_string()))
        .collect();
    let mut out = SessionTree::new();
    out.nodes.clear();
    out.current_path.clear();
    for id in &ids {
        let Some(n) = src.nodes.get(id) else {
            continue;
        };
        let new_id = map[id].clone();
        let parent = n
            .parent
            .as_ref()
            .and_then(|p| map.get(p).cloned())
            .filter(|_| !path_only || n.parent.as_ref().is_some_and(|p| map.contains_key(p)));
        let mut msg = n.message.clone();
        msg.id = new_id.clone();
        out.nodes.insert(
            new_id.clone(),
            rupi_core::SessionNode {
                id: new_id,
                parent,
                message: msg,
                summary: n.summary.clone(),
                created_at: n.created_at,
            },
        );
    }
    out.current_path = src
        .current_path
        .iter()
        .filter_map(|id| map.get(id).cloned())
        .collect();
    if out.current_path.is_empty() {
        if let Some(leaf) = out.nodes.keys().next().cloned() {
            out.goto_node(&leaf);
        }
    }
    if let Some(t) = &src.summary_through {
        if let Some(nt) = map.get(t) {
            out.summary = src.summary.clone();
            out.summary_through = Some(nt.clone());
        }
    }
    out
}

/// 从库记录重建树（与 CLI restore 同语义）。
pub fn tree_from_records(recs: Vec<SessionRecord>, summary: &str) -> SessionTree {
    let mut s = SessionTree::new();
    for rec in recs {
        s.push_with_id(rec.id.clone(), rec.to_message());
    }
    if !summary.is_empty() {
        if let Some(first) = s.current_path.first().cloned() {
            s.summary = Some(summary.to_string());
            s.summary_through = Some(first);
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonl_roundtrip_user_assistant() {
        let mut tree = SessionTree::new();
        tree.push(Message::text(Role::User, "hello"));
        tree.push(Message::text(Role::Assistant, "hi there"));
        let jsonl = export_tree_jsonl(&tree, "/tmp/proj", Some("demo"), None);
        assert!(jsonl.starts_with("{\"type\":\"session\""), "{jsonl}");
        assert!(jsonl.contains("\"version\":3"));
        assert!(jsonl.contains("\"role\":\"user\""));
        assert!(jsonl.contains("\"role\":\"assistant\""));
        let imported = import_jsonl(&jsonl).unwrap();
        assert_eq!(imported.name.as_deref(), Some("demo"));
        assert_eq!(imported.tree.history().len(), 2);
        assert!(imported.tree.history()[0].full_text().contains("hello"));
        assert!(imported.tree.history()[1].full_text().contains("hi there"));
    }

    #[test]
    fn html_escapes_and_lists_roles() {
        let mut tree = SessionTree::new();
        tree.push(Message::text(Role::User, "<script>"));
        let html = export_tree_html(&tree, "t");
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("class=\"user\""));
    }

    #[test]
    fn import_into_store_roundtrip() {
        let home = std::env::temp_dir().join(format!("rupi-sessio-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let store = SessionStore::open(&home).unwrap();
        let mut tree = SessionTree::new();
        tree.push(Message::text(Role::User, "hello import"));
        tree.push(Message::text(Role::Assistant, "ok"));
        let jsonl = export_tree_jsonl(&tree, "/tmp/p", Some("imported-name"), None);
        let (id, got) = import_into_store(&store, &jsonl, "/tmp/p").unwrap();
        assert_eq!(got.history().len(), 2);
        assert_eq!(
            store.get_name(&id).unwrap().as_deref(),
            Some("imported-name")
        );
        assert_eq!(
            store.latest_session(Some("/tmp/p")).unwrap().as_deref(),
            Some(id.as_str())
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn remap_fork_keeps_path_only() {
        let mut tree = SessionTree::new();
        let a = tree.push(Message::text(Role::User, "a"));
        tree.push(Message::text(Role::Assistant, "b"));
        tree.rewind_to(&a);
        tree.push(Message::text(Role::User, "c"));
        assert_eq!(tree.nodes.len(), 3);
        let forked = remap_tree(&tree, true);
        assert_eq!(forked.nodes.len(), 2);
        assert_eq!(forked.history().len(), 2);
        let cloned = remap_tree(&tree, false);
        assert_eq!(cloned.nodes.len(), 3);
    }
}
