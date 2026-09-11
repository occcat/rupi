//! rupi-core: Pi Agent 核心类型 —— 消息、会话树、事件、工具定义、扩展点。
//! 对标 `pi-agent-core` 的最小运行时无关层：只放类型与 trait，不依赖任何 LLM 或 IO。

pub mod commands;
pub mod trust;

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

/// 内容块：文本 / 工具调用 / 工具结果 / 思考块 / 图片，与 OpenAI/Anthropic/Gemini 兼容。
/// Thinking 系 Anthropic extended-thinking 专有：`signature` 是回放凭证，
/// 多轮工具流必须原样带回，否则 API 400；`RedactedThinking` 是服务端加密块，
/// 无明文、必须按 `data` 原样回放。OpenAI/Gemini 请求映射跳过它们（服务端各自管理推理态）。
/// Image：`media_type` 为 MIME（如 `image/png`），`data` 为标准 Base64（无 data: 前缀）。
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
    Thinking {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    RedactedThinking {
        data: String,
    },
    Image {
        media_type: String,
        data: String,
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

    /// 由内容块组装消息（`@file` 图文混排、工具结果附图片）。
    pub fn from_blocks(role: Role, blocks: Vec<ContentBlock>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            role,
            blocks,
            provider: None,
            created_at: Utc::now(),
        }
    }

    pub fn has_images(&self) -> bool {
        self.blocks
            .iter()
            .any(|b| matches!(b, ContentBlock::Image { .. }))
    }

    /// 估算本条消息送模型时的 token 数（文本 + 工具块）。
    pub fn estimate_tokens(&self) -> u64 {
        estimate_tokens(&self.full_text())
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
                // 思考过程计入 transcript（压缩摘要可见）；加密块无明文，跳过
                ContentBlock::Thinking { text, .. } if !text.is_empty() => {
                    Some(format!("[thinking] {text}"))
                }
                ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. } => None,
                ContentBlock::Image { media_type, .. } => Some(format!("[image {media_type}]")),
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
        self.push_with_id(Uuid::new_v4().to_string(), message)
    }

    /// 以指定 id 追加（resume 时沿用 sessions.db 行 id，跨进程短 id 稳定）。
    pub fn push_with_id(&mut self, id: String, message: Message) -> String {
        let parent = self.current_path.last().cloned();
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

    /// 历史总字符数（展示/兼容；压实触发改走 [`Self::history_tokens`]）。
    pub fn history_chars(&self) -> usize {
        self.history().iter().map(|m| m.full_text().len()).sum()
    }

    /// 当前分支历史的估算 token 数（对标 Pi contextTokens）。
    pub fn history_tokens(&self) -> u64 {
        self.history().iter().map(|m| m.estimate_tokens()).sum()
    }

    /// 记录压缩摘要：`through` 为摘要覆盖到的最后一个节点 id。
    pub fn set_summary(&mut self, summary: String, through: String) {
        self.summary = Some(summary);
        self.summary_through = Some(through);
    }

    /// prompt 用窗口：无摘要时全量；有摘要时 `[摘要, summary_through 之后的消息]`。
    /// `keep_last` 仅在缺少 `summary_through` 时作尾部条数兜底（兼容旧调用）。
    /// 树本身不动，rewind/branch 语义不受影响。
    pub fn prompt_history(&self, keep_last: usize) -> Vec<Message> {
        let all: Vec<Message> = self.history().into_iter().cloned().collect();
        let Some(summary) = &self.summary else {
            return all;
        };
        let start = self
            .summary_through
            .as_ref()
            .and_then(|t| self.current_path.iter().position(|id| id == t))
            .map(|i| i + 1)
            .unwrap_or_else(|| all.len().saturating_sub(keep_last));
        let mut out = vec![Message::text(
            Role::System,
            format!("[Conversation summary so far]\n{summary}"),
        )];
        if start < all.len() {
            out.extend(all[start..].iter().cloned());
        }
        out
    }

    /// 从末尾累加 token，保留至少 `keep_recent_tokens`；返回应被摘要的前缀长度。
    /// 整段都不够切（或空）时返回 `None`。
    pub fn compaction_cut(&self, keep_recent_tokens: usize) -> Option<usize> {
        let hist = self.history();
        if hist.is_empty() {
            return None;
        }
        let mut acc = 0u64;
        let mut kept = 0usize;
        for m in hist.iter().rev() {
            acc = acc.saturating_add(m.estimate_tokens());
            kept += 1;
            if acc >= keep_recent_tokens as u64 {
                break;
            }
        }
        if kept >= hist.len() {
            return None;
        }
        Some(hist.len() - kept)
    }

    /// 分叉：从 `from_node` 切出一条新游标（side-quest 修工具不污染主上下文）。
    /// 压缩边界随动：摘要覆盖点落在分叉路径内才继承，否则清零——否则分叉会带着
    /// 描述“别人家历史”的摘要进 prompt（上游 #8990 同类），缺失时宁可重压。
    pub fn branch_from(&self, from_node: &str) -> Option<Self> {
        let idx = self.current_path.iter().position(|id| id == from_node)?;
        let mut forked = self.clone();
        forked.id = Uuid::new_v4().to_string();
        forked.current_path = self.current_path[..=idx].to_vec();
        if forked
            .summary_through
            .as_ref()
            .is_some_and(|t| !forked.current_path.contains(t))
        {
            forked.summary = None;
            forked.summary_through = None;
        }
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

    /// 跳转到任意节点（含废弃分支）：当前路径内则截断，否则沿 parent 链重建路径。
    /// Pi `/tree` 时间旅行的底层语义；跳转不删节点，原分支仍在树上。
    pub fn goto_node(&mut self, node_id: &str) -> bool {
        if !self.nodes.contains_key(node_id) {
            return false;
        }
        if self.rewind_to(node_id) {
            return true;
        }
        let mut path = vec![];
        let mut cur = Some(node_id.to_string());
        while let Some(id) = cur {
            let Some(n) = self.nodes.get(&id) else {
                return false;
            };
            path.push(id.clone());
            cur = n.parent.clone();
        }
        path.reverse();
        self.current_path = path;
        true
    }

    /// 短 id 前缀解析（≥4 字符，前缀唯一才成功，供 `/goto` 用）。
    pub fn resolve_short_id(&self, prefix: &str) -> Option<String> {
        if prefix.len() < 4 {
            return None;
        }
        let mut hit = None;
        for id in self.nodes.keys() {
            if id.starts_with(prefix) {
                if hit.is_some() {
                    return None;
                }
                hit = Some(id.clone());
            }
        }
        hit
    }

    /// 树视图（Pi `/tree` 对齐）：从 roots DFS，全分支可见；
    /// `*` 为当前游标路径，`+` 为废弃分支节点。每行：标记 短id role: 预览。
    pub fn tree_view(&self) -> String {
        use std::collections::HashMap;
        let on_path: std::collections::HashSet<&str> =
            self.current_path.iter().map(|s| s.as_str()).collect();
        let mut children: HashMap<Option<String>, Vec<String>> = HashMap::new();
        let mut roots: Vec<String> = vec![];
        // 确定性输出：按创建时间排序
        let mut ids: Vec<&String> = self.nodes.keys().collect();
        ids.sort_by_key(|id| self.nodes[*id].created_at);
        for id in ids {
            let parent = self.nodes[id].parent.clone();
            if parent.as_ref().is_some_and(|p| self.nodes.contains_key(p)) {
                children.entry(parent).or_default().push(id.clone());
            } else {
                roots.push(id.clone());
            }
        }
        let mut out = String::new();
        fn dfs(
            tree: &SessionTree,
            children: &HashMap<Option<String>, Vec<String>>,
            on_path: &std::collections::HashSet<&str>,
            id: &str,
            depth: usize,
            out: &mut String,
        ) {
            if let Some(n) = tree.nodes.get(id) {
                let mark = if on_path.contains(id) { '*' } else { '+' };
                let preview: String = n
                    .message
                    .full_text()
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .chars()
                    .take(60)
                    .collect();
                out.push_str(&format!(
                    "{mark} {} {:?}: {preview}\n",
                    &id[..8.min(id.len())],
                    n.message.role,
                ));
                if let Some(kids) = children.get(&Some(id.to_string())) {
                    for k in kids {
                        out.push_str(&"  ".repeat(depth + 1));
                        dfs(tree, children, on_path, k, depth + 1, out);
                    }
                }
            }
        }
        for r in &roots {
            dfs(self, &children, &on_path, r, 0, &mut out);
        }
        // 空树不回空串：调用方（REPL/TUI）直接打印，空串等于零输出
        if out.is_empty() {
            out.push_str("(empty session — send a message first)");
        }
        out
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
    /// 整轮结束（对标上游 `agent_end`）：终止屏障，扩展订阅者被 await，可做落盘/通知等收尾。
    /// TurnEnd 是每回合的；RunEnd 整轮只发一次（Done / MaxTurns 两出口）。
    RunEnd {
        stop_reason: StopReason,
    },
    /// 审批问询开始（对标上游 `ui_prompt_start`）：主循环即将阻塞等人工裁决。
    /// 宿主集成借此区分“agent 干活”与“等用户拍板”（如停转圈、记等待耗时）。
    UiPromptStart {
        tool: String,
        reason: String,
    },
    /// 审批问询结束（对标上游 `ui_prompt_end`）：人工已裁决，循环继续。
    UiPromptEnd {
        tool: String,
        approved: bool,
    },
    /// 回想指示（对标 Hermes `describe_recall`）：本轮 prefetch 实际注入了外部记忆，
    /// 即使模型沉默用户也看得到记忆被用了；无注入时不发射。
    MemoryRecall {
        detail: String,
    },
    /// 压实开始（对标上游 compaction operation 的 `compaction_start`）：真正要调模型
    /// 做摘要前发射（阈值未命中等跳过路径不发射），前端可借此显示状态而非静默卡住。
    CompactionStart,
    /// 压实结束（对标上游 `compaction_end`）：`summarized` 为被摘要的消息数，
    /// `kept` 为保留的尾部条数。
    CompactionEnd {
        summarized: usize,
        kept: usize,
    },
    /// 本回合模型用量（provider 在流末尾给出；未给则不发射）。对标 pi-ai usage。
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
    Error {
        message: String,
    },
    /// 扩展返回的 UI 提示（对标 Pi extension UI hints）：宿主可画 toast/状态行。
    /// `kind` 为扩展自报（`note`/`status`/`toast`），未知值当普通系统行。
    UiHint {
        source: String,
        kind: String,
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

/// 协作式取消旗标（对标上游 effect-gate 的取消源）：TUI `Esc` / CLI `Ctrl-C` 置位，
/// 主循环在 turn 边界、流式补全 `select!`、串行工具间隙检查，中止后发
/// `TurnEnd{Aborted}`（轮中）+ `RunEnd{Aborted}` 收尾。
/// `clone` 共享同一状态（一次置位处处可见）；语义是协作式的——in-flight 的工具调用
/// 跑完当前项，并行批跑完当前批，审批问询（同步阻塞）不受影响；`cancel` 经
/// `execute_with_cancel` 透给工具后，子 agent 内层循环就地停、bash/外部进程整组被杀、
/// MCP 远端调用不再等。
#[derive(Debug, Clone, Default)]
pub struct CancelFlag {
    inner: std::sync::Arc<CancelInner>,
}

#[derive(Debug, Default)]
struct CancelInner {
    cancelled: std::sync::atomic::AtomicBool,
    notify: tokio::sync::Notify,
}

impl CancelFlag {
    pub fn new() -> Self {
        Self::default()
    }

    /// 置位并唤醒所有 `cancelled()` 等待者；重复调用无害。
    pub fn cancel(&self) {
        self.inner
            .cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.inner.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner
            .cancelled
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// 置位即返回。`notify_waiters` 无等待者时留一个 permit，先检查后等待的写法无竞态。
    pub async fn cancelled(&self) {
        loop {
            if self.is_cancelled() {
                return;
            }
            self.inner.notify.notified().await;
        }
    }
}

/// 混合估算：ASCII 约 4 字符/token，非 ASCII（含 CJK）约 1 字/token。
/// 无 tiktoken 依赖；provider 回报的 `usage` 可在上层校准。
pub fn estimate_tokens(text: &str) -> u64 {
    let mut tokens = 0u64;
    let mut ascii_run = 0u64;
    for ch in text.chars() {
        if ch.is_ascii() {
            ascii_run += 1;
        } else {
            tokens += ascii_run.div_ceil(4);
            ascii_run = 0;
            tokens += 1;
        }
    }
    tokens + ascii_run.div_ceil(4)
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

/// 扩展注册的斜杠命令（JSON-RPC 扩展 `registerCommand` / initialize.capabilities）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionCommand {
    pub name: String,
    pub description: String,
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
    /// 扩展注册的斜杠命令（默认无）。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![]
    }
    async fn on_event(&self, _event: &AgentEvent) -> anyhow::Result<()> {
        Ok(())
    }
}

/// 按扩展名猜图片 MIME。未知扩展返回 None（不当图片读）。
pub fn image_media_type(path: &std::path::Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => Some("image/png"),
        Some("jpg") | Some("jpeg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        Some("bmp") => Some("image/bmp"),
        Some("svg") => Some("image/svg+xml"),
        _ => None,
    }
}

/// 标准 Base64（无换行）。图片块与 data URL 共用，避免再引 crate。
pub fn encode_base64(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    let mut i = 0;
    while i < data.len() {
        let b0 = data[i];
        let b1 = data.get(i + 1).copied();
        let b2 = data.get(i + 2).copied();
        out.push(T[(b0 >> 2) as usize] as char);
        out.push(T[(((b0 & 0x03) << 4) | (b1.unwrap_or(0) >> 4)) as usize] as char);
        match (b1, b2) {
            (Some(b1), Some(b2)) => {
                out.push(T[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
                out.push(T[(b2 & 0x3f) as usize] as char);
            }
            (Some(b1), None) => {
                out.push(T[((b1 & 0x0f) << 2) as usize] as char);
                out.push('=');
            }
            (None, _) => {
                out.push('=');
                out.push('=');
            }
        }
        i += 3;
    }
    out
}

/// `data:{media_type};base64,{data}`（OpenAI `image_url`）。
pub fn image_data_url(media_type: &str, data: &str) -> String {
    format!("data:{media_type};base64,{data}")
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
    fn tree_view_and_goto_across_abandoned_branch() {
        let mut s = SessionTree::new();
        s.push(Message::text(Role::User, "root question"));
        let fork_point = s.push(Message::text(Role::Assistant, "answer A"));
        s.push(Message::text(Role::User, "follow A"));
        // 回退并走新分支：旧 follow A 成为废弃分支
        assert!(s.rewind_to(&fork_point));
        let b = s.push(Message::text(Role::User, "follow B"));
        // 树视图同时看见两分支
        let view = s.tree_view();
        assert!(view.contains("follow A"));
        assert!(view.contains("follow B"));
        assert!(view.contains('*'));
        assert!(view.contains('+'));
        // 跨分支跳转：沿 parent 链重建路径
        let abandoned: String = s
            .nodes
            .iter()
            .find(|(_, n)| n.message.full_text().contains("follow A"))
            .map(|(id, _)| id.clone())
            .unwrap();
        assert!(s.goto_node(&abandoned));
        assert!(s
            .history()
            .iter()
            .any(|m| m.full_text().contains("follow A")));
        assert!(!s
            .history()
            .iter()
            .any(|m| m.full_text().contains("follow B")));
        // 短 id 解析往返
        let short = &b[..8];
        assert_eq!(s.resolve_short_id(short), Some(b.clone()));
        assert!(s.resolve_short_id("ab").is_none());
        assert!(s.goto_node("no-such-node") == false);
    }

    #[test]
    fn prompt_history_uses_summary_plus_after_through() {
        let mut s = SessionTree::new();
        for i in 0..5 {
            s.push(Message::text(Role::User, format!("msg {i}")));
        }
        // 无摘要：全量
        assert_eq!(s.prompt_history(2).len(), 5);
        let through = s.current_path[2].clone();
        s.set_summary("early stuff".into(), through);
        // 有摘要：1 条摘要 + through 之后（msg 3/4），keep 参数不再切片
        let w = s.prompt_history(99);
        assert_eq!(w.len(), 3);
        assert!(w[0].full_text().contains("early stuff"));
        assert!(w[2].full_text().contains("msg 4"));
    }

    #[test]
    fn estimate_tokens_ascii_and_cjk() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcdefgh"), 2);
        assert_eq!(estimate_tokens("你好"), 2);
        assert!(estimate_tokens("hello 世界") >= 3);
        let mut s = SessionTree::new();
        s.push(Message::text(Role::User, "abcd"));
        assert_eq!(s.history_tokens(), 1);
        assert_eq!(s.compaction_cut(1), None);
        s.push(Message::text(Role::User, "efghijkl"));
        assert_eq!(s.compaction_cut(1), Some(1));
    }

    #[test]
    fn branch_from_drops_out_of_path_summary() {
        let mut s = SessionTree::new();
        for i in 0..5 {
            s.push(Message::text(Role::User, format!("msg {i}")));
        }
        let through = s.current_path[2].clone();
        s.set_summary("early stuff".into(), through);
        // 从摘要覆盖点之前分叉：摘要描述的是“别人家历史”，必须清零。
        let early = s.branch_from(&s.current_path[0].clone()).unwrap();
        assert!(early.summary.is_none());
        assert!(early.summary_through.is_none());
        // 从覆盖点之后分叉：路径仍包含覆盖点，摘要保留。
        let late = s.branch_from(&s.current_path[4].clone()).unwrap();
        assert!(late.summary.as_deref() == Some("early stuff"));
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

    #[test]
    fn image_block_full_text_and_base64() {
        let m = Message::from_blocks(
            Role::User,
            vec![
                ContentBlock::Text {
                    text: "see".into(),
                },
                ContentBlock::Image {
                    media_type: "image/png".into(),
                    data: encode_base64(&[0, 1, 2]),
                },
            ],
        );
        assert!(m.has_images());
        assert!(m.full_text().contains("see"));
        assert!(m.full_text().contains("[image image/png]"));
        assert_eq!(image_media_type(std::path::Path::new("a.PNG")), Some("image/png"));
        assert_eq!(image_media_type(std::path::Path::new("x.txt")), None);
        // RFC 4648：`Man` → `TWFu`；空输入空串
        assert_eq!(encode_base64(b"Man"), "TWFu");
        assert_eq!(encode_base64(b"Ma"), "TWE=");
        assert_eq!(encode_base64(b"M"), "TQ==");
        assert_eq!(encode_base64(b""), "");
        assert_eq!(
            image_data_url("image/png", "abc"),
            "data:image/png;base64,abc"
        );
    }
}
