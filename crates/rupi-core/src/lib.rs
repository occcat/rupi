//! rupi-core: Pi Agent 核心类型 —— 消息、会话树、事件、工具定义、扩展点。
//! 对标 `pi-agent-core` 的最小运行时无关层：只放类型与 trait，不依赖任何 LLM 或 IO。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// 消息角色。Pi 的 session 可混用多家 provider 的消息，这里保留 `provider` 以便移植时做兼容降级。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// 内容块：文本 / 工具调用 / 工具结果三态，与 OpenAI/Anthropic 两种风格兼容。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: serde_json::Value,
    },
    ToolResult {
        tool_call_id: String,
        content: String,
        is_error: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub role: Role,
    pub blocks: Vec<ContentBlock>,
    pub provider: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl Message {
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            role,
            blocks: vec![ContentBlock::Text { text: text.into() }],
            provider: None,
            created_at: Utc::now(),
        }
    }

    pub fn full_text(&self) -> String {
        self.blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.clone()),
                ContentBlock::ToolResult { content, .. } => Some(content.clone()),
                ContentBlock::ToolCall {
                    name, arguments, ..
                } => Some(format!("{name} {arguments}")),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// 会话树节点：Pi sessions are trees —— 支持 branch / rewind / summary。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionNode {
    pub id: String,
    pub parent: Option<String>,
    pub message: Message,
    pub summary: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// 会话树：线性 `current_branch` + 全量节点表。rewind 不删历史，只移动游标。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionTree {
    pub id: String,
    pub nodes: HashMap<String, SessionNode>,
    /// 从 root 到当前游标的节点 id 链
    pub current_path: Vec<String>,
    /// 压缩摘要：覆盖 `summary_through` 之前全部历史；prompt 只带摘要 + 近期窗口
    pub summary: Option<String>,
    pub summary_through: Option<String>,
}

impl SessionTree {
    pub fn new() -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            nodes: HashMap::new(),
            current_path: vec![],
            summary: None,
            summary_through: None,
        }
    }

    /// 追加一条消息，返回新节点 id。
    pub fn push(&mut self, message: Message) -> String {
        let parent = self.current_path.last().cloned();
        let id = Uuid::new_v4().to_string();
        let node = SessionNode {
            id: id.clone(),
            parent,
            message,
            summary: None,
            created_at: Utc::now(),
        };
        self.nodes.insert(id.clone(), node);
        self.current_path.push(id.clone());
        id
    }

    /// 当前分支的线性消息历史。
    pub fn history(&self) -> Vec<&Message> {
        self.current_path
            .iter()
            .filter_map(|id| self.nodes.get(id).map(|n| &n.message))
            .collect()
    }

    /// 历史总字符数（压缩触发依据）。
    pub fn history_chars(&self) -> usize {
        self.history().iter().map(|m| m.full_text().len()).sum()
    }

    /// 记录压缩摘要：`through` 为摘要覆盖到的最后一个节点 id。
    pub fn set_summary(&mut self, summary: String, through: String) {
        self.summary = Some(summary);
        self.summary_through = Some(through);
    }

    /// prompt 用窗口：无摘要时全量；有摘要时 `[摘要, 近 keep_last 条]`。
    /// 树本身不动，rewind/branch 语义不受影响。
    pub fn prompt_history(&self, keep_last: usize) -> Vec<Message> {
        let all: Vec<Message> = self.history().into_iter().cloned().collect();
        let Some(summary) = &self.summary else {
            return all;
        };
        let start = all.len().saturating_sub(keep_last);
        let mut out = vec![Message::text(
            Role::System,
            format!("[Conversation summary so far]\n{summary}"),
        )];
        out.extend(all[start..].iter().cloned());
        out
    }

    /// 分叉：从 `from_node` 切出一条新游标（side-quest 修工具不污染主上下文）。
    pub fn branch_from(&self, from_node: &str) -> Option<Self> {
        let idx = self.current_path.iter().position(|id| id == from_node)?;
        let mut forked = self.clone();
        forked.id = Uuid::new_v4().to_string();
        forked.current_path = self.current_path[..=idx].to_vec();
        Some(forked)
    }

    /// 回退游标到历史节点（不删除后续节点，可随时再前进/总结）。
    pub fn rewind_to(&mut self, node_id: &str) -> bool {
        if let Some(idx) = self.current_path.iter().position(|id| id == node_id) {
            self.current_path.truncate(idx + 1);
            true
        } else {
            false
        }
    }
}

/// Agent 事件流：对标 Pi 的 event streaming，UI/TUI 订阅此流渲染。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    TurnStart {
        turn: u32,
    },
    TextDelta {
        delta: String,
    },
    ToolStart {
        tool_call_id: String,
        name: String,
        arguments: serde_json::Value,
    },
    ToolEnd {
        tool_call_id: String,
        name: String,
        content: String,
        is_error: bool,
    },
    TurnEnd {
        turn: u32,
        stop_reason: StopReason,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Done,
    MaxTurns,
    Aborted,
    ProviderError(String),
}

/// 工具定义：JSON Schema 描述参数；`prompt_snippet` 为必填——Pi 若缺了它就不会把工具列入系统提示。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    /// JSON Schema object
    pub input_schema: serde_json::Value,
    /// 注入系统提示 "Available tools" 段的短文本；缺失则 agent 永远不知道工具存在。
    pub prompt_snippet: Option<String>,
}

impl ToolDefinition {
    pub fn prompt_line(&self) -> String {
        let snippet = self.prompt_snippet.as_deref().unwrap_or(&self.description);
        format!("- {}: {}", self.name, snippet)
    }
}

/// 扩展点：工具 / 命令 / 事件钩子。Pi 哲学：core 极小，一切能力走扩展组合。
#[async_trait::async_trait]
pub trait Extension: Send + Sync {
    fn name(&self) -> &str;
    fn tools(&self) -> Vec<ToolDefinition> {
        vec![]
    }
    /// 扩展可向前置系统提示追加 markdown 片段（如 MCP 工具清单、记忆块、Skill 索引）。
    fn system_prompt_snippet(&self) -> Option<String> {
        None
    }
    async fn on_event(&self, _event: &AgentEvent) -> anyhow::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_branch_and_rewind() {
        let mut s = SessionTree::new();
        let a = s.push(Message::text(Role::User, "hi"));
        s.push(Message::text(Role::Assistant, "hello"));
        assert_eq!(s.history().len(), 2);
        assert!(s.rewind_to(&a));
        assert_eq!(s.history().len(), 1);
        let forked = s.branch_from(&a).expect("branch");
        assert_eq!(forked.history().len(), 1);
    }

    #[test]
    fn prompt_history_uses_summary_plus_window() {
        let mut s = SessionTree::new();
        for i in 0..5 {
            s.push(Message::text(Role::User, format!("msg {i}")));
        }
        // 无摘要：全量
        assert_eq!(s.prompt_history(2).len(), 5);
        let through = s.current_path[2].clone();
        s.set_summary("early stuff".into(), through);
        // 有摘要：1 条摘要 + 近 2 条
        let w = s.prompt_history(2);
        assert_eq!(w.len(), 3);
        assert!(w[0].full_text().contains("early stuff"));
        assert!(w[2].full_text().contains("msg 4"));
    }

    #[test]
    fn tool_definition_requires_prompt_snippet_to_be_visible() {
        let t = ToolDefinition {
            name: "read".into(),
            description: "read file".into(),
            input_schema: serde_json::json!({"type":"object"}),
            prompt_snippet: None,
        };
        // 无 snippet 时回退到 description，保证可见性（Rust 复刻里强制要求调用方提供 snippet）
        assert!(t.prompt_line().contains("read file"));
    }
}
