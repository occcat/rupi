//! rupi-memory: Hermes 风格分层记忆。
//! - 内建层：`MEMORY.md` / `USER.md`，session 启动时冻结快照注入系统提示（保 prefix cache），
//!   会话内写盘即时生效但快照不变，下个 session 才可见。
//! - 外部层：`MemoryProvider` trait（7 方法生命周期），`MemoryManager` 编排且只允许一个外部 provider。
//! - 会话层：SQLite 会话/消息库 + FTS5 + session_search；后台 review 钩子沉淀记忆与 Skill。

use async_trait::async_trait;
use rupi_core::ToolDefinition;
use rusqlite::OptionalExtension;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const MEMORY_FILE: &str = "MEMORY.md";
pub const USER_FILE: &str = "USER.md";
pub const FAILURES_FILE: &str = "failures.md";

/// 疑似密钥落盘即拒绝（Hermes secret scanning 对齐）：API key / token / 私钥块等。
/// 误伤可接受——记忆本就不该存这些；调用方收到 error 可改写后再存。
pub fn contains_secret(text: &str) -> bool {
    let lower = text.to_lowercase();
    for marker in [
        "sk-",
        "ghp_",
        "gho_",
        "xoxb-",
        "xoxp-",
        "xoxa-",
        "xoxs-",
        "akia",
        "-----begin",
        "private key",
    ] {
        if lower.contains(marker) {
            return true;
        }
    }
    // key= / token: / password= 后跟较长无空格值
    let mut last_word = String::new();
    let mut chars = lower.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_alphanumeric() || c == '_' || c == '-' {
            last_word.push(c);
        } else {
            if (c == ':' || c == '=')
                && matches!(
                    last_word.as_str(),
                    "api_key" | "apikey" | "api-key" | "token" | "password" | "passwd" | "secret"
                )
            {
                let rest: String = chars.clone().take(64).collect();
                let val = rest.trim_start_matches([' ', '"', '\'']);
                if val.chars().take_while(|c| !c.is_whitespace()).count() >= 8 {
                    return true;
                }
            }
            last_word.clear();
        }
    }
    false
}

/// 无语义信号的输入（对标 Hermes `is_trivial_prompt` + `TRIVIAL_PROMPT_RE`）：
/// 空输入、斜杠命令、纯招呼/确认（`hi!`/`thanks :)`/`done???`），命中则跳过本轮外部
/// recall——省一次后端往返，且陈旧上下文不会带偏单字回复。`k8s`/`yolo`/`note` 这类
/// 词内命中不算（词后只许跟标点），大小写不敏感；实现与原正则逐项等价（先 trim，
/// 再“词表词 + 纯标点尾”全匹配），不另引 regex 依赖。
pub fn is_trivial_prompt(text: &str) -> bool {
    let stripped = text.trim();
    if stripped.is_empty() || stripped.starts_with('/') {
        return true;
    }
    let lower = stripped.to_lowercase();
    let word = lower.trim_end_matches([
        ' ', '\t', '\n', '\r', '!', '?', '.', ':', ';', ',', '"', '\'', '~', '\u{2018}',
        '\u{2019}', '\u{201c}', '\u{201d}', '\u{2014}', '\u{2013}', '\u{2026}', '(', ')', '[', ']',
        '{', '}', '<', '>', '*', '&', '^', '%', '$', '#', '@', '+', '=', '`', '\u{a0}',
    ]);
    matches!(
        word,
        "yes"
            | "no"
            | "ok"
            | "okay"
            | "sure"
            | "thanks"
            | "thank you"
            | "y"
            | "n"
            | "yep"
            | "nope"
            | "yeah"
            | "nah"
            | "hi"
            | "hey"
            | "hello"
            | "yo"
            | "sup"
            | "continue"
            | "go ahead"
            | "do it"
            | "proceed"
            | "got it"
            | "cool"
            | "nice"
            | "great"
            | "done"
            | "next"
            | "lgtm"
            | "k"
    )
}

/// `[core]` 分层（借鉴 Hermes core/extended 两层，自 `rupi-pi-agent-9492` 分支的思路）：
/// 任一行带 `[core]` 标记时，只把标记行注入冻结快照（标记剥掉、保留 `- ` 项目符），其余行
/// 作为 extended 层留给 `memory_search` 按需召回，并在末尾注明数量；无标记则全量注入
/// （向后兼容）。返回 (注入文本, extended 行数)。
pub fn core_tier(text: &str) -> (String, usize) {
    fn split_core(line: &str) -> Option<String> {
        let body = line.trim_start();
        let body = body
            .strip_prefix("- ")
            .or_else(|| body.strip_prefix("* "))
            .unwrap_or(body);
        let prefix = &line[..line.len() - body.len()];
        let tag = body.get(..6)?;
        if !tag.eq_ignore_ascii_case("[core]") {
            return None;
        }
        Some(format!("{prefix}{}", body[6..].trim_start()))
    }
    let lines: Vec<&str> = text.lines().collect();
    if !lines.iter().any(|l| split_core(l).is_some()) {
        return (text.to_string(), 0);
    }
    let mut out: Vec<String> = vec![];
    let mut extended = 0usize;
    for l in &lines {
        match split_core(l) {
            Some(core) => out.push(core),
            None if l.trim().is_empty() => {}
            None => extended += 1,
        }
    }
    let mut s = out.join("\n");
    if extended > 0 {
        s.push_str(&format!(
            "\n(core tier: {extended} extended entr{} not shown — recall with memory_search)",
            if extended == 1 { "y" } else { "ies" }
        ));
    }
    (s, extended)
}
/// 内建记忆文件：受 char limit 保护（默认 ~800 tokens / ~500 tokens），超限截断保尾部。
#[derive(Clone)]
pub struct MemoryStore {
    pub home: PathBuf,
    pub memory_enabled: bool,
    pub user_profile_enabled: bool,
    pub memory_char_limit: usize,
    pub user_char_limit: usize,
    /// 项目根（`discover_project` 从 cwd 上溯 `.git` 得到）；项目记忆存 `<root>/.rupi/MEMORY.md`。
    pub project_root: Option<PathBuf>,
    /// 复用 `sessions.db` 连接：`mirror_memory` / `memory_search` / `session_search`
    /// 不再每次 `SessionStore::open`。
    sessions: std::sync::Arc<std::sync::Mutex<Option<SessionStore>>>,
}

impl std::fmt::Debug for MemoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryStore")
            .field("home", &self.home)
            .field("memory_enabled", &self.memory_enabled)
            .field("user_profile_enabled", &self.user_profile_enabled)
            .field("memory_char_limit", &self.memory_char_limit)
            .field("user_char_limit", &self.user_char_limit)
            .field("project_root", &self.project_root)
            .finish()
    }
}

impl MemoryStore {
    pub fn new(home: PathBuf) -> Self {
        Self {
            home,
            memory_enabled: true,
            user_profile_enabled: true,
            memory_char_limit: 5000,
            user_char_limit: 5000,
            project_root: None,
            sessions: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }

    fn with_sessions<T>(
        &self,
        f: impl FnOnce(&SessionStore) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let mut g = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_none() {
            *g = Some(SessionStore::open(&self.home)?);
        }
        f(g.as_ref().expect("session store just opened"))
    }

    pub fn with_project(mut self, root: PathBuf) -> Self {
        self.project_root = Some(root);
        self
    }

    /// 从 `start` 上溯找 `.git`（或已有 `.rupi/MEMORY.md` 的目录）定项目根；找不到返回 None。
    pub fn discover_project(start: &Path) -> Option<PathBuf> {
        let mut dir = if start.is_file() {
            start.parent()?.to_path_buf()
        } else {
            start.to_path_buf()
        };
        loop {
            if dir.join(".git").exists() || dir.join(".rupi").join("MEMORY.md").exists() {
                return Some(dir);
            }
            if !dir.pop() {
                return None;
            }
        }
    }

    /// 项目名（目录名），注入 prompt 做分区标注。
    pub fn project_name(&self) -> Option<String> {
        self.project_root
            .as_ref()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
    }

    fn project_memory_path(&self) -> Option<PathBuf> {
        self.project_root
            .as_ref()
            .map(|r| r.join(".rupi").join(MEMORY_FILE))
    }

    fn memories_dir(&self) -> PathBuf {
        self.home.join("memories")
    }

    fn read_limited(&self, path: &Path, limit: usize) -> String {
        let content = std::fs::read_to_string(path).unwrap_or_default();
        Self::fit_to_limit(&content, limit)
    }

    /// 容量压实（Hermes auto-consolidation 的确定性底层）：超限时按行丢最旧、保最新；
    /// 单行超长才按字符截尾。读路径与写路径共用，不变量：输出永远 ≤ limit（+ 一行标记）。
    fn fit_to_limit(content: &str, limit: usize) -> String {
        if content.len() <= limit {
            return content.to_string();
        }
        let mut lines: Vec<&str> = content.lines().collect();
        let mut kept: Vec<&str> = vec![];
        let mut bytes = 0;
        while let Some(line) = lines.pop() {
            let cost = line.len() + 1;
            if !kept.is_empty() && bytes + cost > limit {
                break;
            }
            bytes += cost;
            kept.push(line);
        }
        kept.reverse();
        let mut out = String::from("...[compacted: oldest entries dropped]\n");
        out.push_str(&kept.join("\n"));
        if content.ends_with('\n') {
            out.push('\n');
        }
        // 极端：单行即超限（按字符截尾；字节切片会切断 UTF-8 导致 panic）
        if out.len() > limit + 128 {
            let tail: String = out.chars().rev().take(limit).collect();
            out = format!("...[truncated]\n{}", tail.chars().rev().collect::<String>());
        }
        out
    }

    pub fn memory_text(&self) -> String {
        if !self.memory_enabled {
            return String::new();
        }
        let (mut out, _) = core_tier(&self.read_limited(
            &self.memories_dir().join(MEMORY_FILE),
            self.memory_char_limit,
        ));
        // 项目层独立限额、分区标注（Hermes two-tier 对齐：全局 + 项目都搜得到）。
        if let (Some(path), Some(name)) = (self.project_memory_path(), self.project_name()) {
            let (proj, _) = core_tier(&self.read_limited(&path, self.memory_char_limit));
            if !proj.trim().is_empty() {
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str(&format!("\n[project:{name}]\n{proj}"));
            }
        }
        out
    }

    pub fn user_text(&self) -> String {
        if !self.user_profile_enabled {
            return String::new();
        }
        self.read_limited(&self.memories_dir().join(USER_FILE), self.user_char_limit)
    }

    /// 会话启动时冻结快照：此后本 session 的系统提示不再变化。
    pub fn frozen_snapshot(&self) -> FrozenMemory {
        FrozenMemory {
            memory: self.memory_text(),
            user: self.user_text(),
            failures: self.failures_text(),
        }
    }

    /// 失败记忆（Hermes failures.md 对齐）：存“什么没成 + 为什么”，带时间戳。
    /// 同样过密钥扫描；失败记录只追加，由 review 纠正检测或 agent 显式写入。
    pub fn record_failure(&self, entry: &str) -> anyhow::Result<()> {
        if contains_secret(entry) {
            anyhow::bail!(
                "refused: failure entry looks like a secret; describe it without the credential"
            );
        }
        std::fs::create_dir_all(self.memories_dir())?;
        let path = self.memories_dir().join(FAILURES_FILE);
        let mut content = std::fs::read_to_string(&path).unwrap_or_default();
        // 幂等：同纠正反复出现只记一条（行首 `- [日期] ` 前缀剥掉再比，日期不同也算重复）
        let want = entry.trim();
        let dup = content.lines().any(|l| {
            let body = l
                .trim()
                .strip_prefix("- [")
                .and_then(|r| r.split_once("] "));
            match body {
                Some((_, rest)) => rest.trim() == want,
                None => l.trim() == want,
            }
        });
        if dup {
            tracing::debug!("failure entry skipped (already recorded)");
            return Ok(());
        }
        content.push_str(&format!(
            "- [{}] {}\n",
            chrono::Utc::now().format("%Y-%m-%d"),
            entry.trim()
        ));
        std::fs::write(&path, &content)?;
        self.mirror_memory("failure", entry);
        self.compact_file(&path);
        Ok(())
    }

    pub fn failures_text(&self) -> String {
        self.read_limited(
            &self.memories_dir().join(FAILURES_FILE),
            self.memory_char_limit,
        )
    }

    /// agent 经 `memory` 工具写入：即时落盘，返回实时状态（但不改变已冻结快照）。
    /// 含疑似密钥一律拒绝（不落盘，调用方可改写脱敏后再存）。
    /// scope: "global"（默认）| "project"（无项目根则报错提示）。
    pub fn apply_write(&self, op: &str, entry: &str) -> anyhow::Result<String> {
        self.apply_write_scoped("global", op, entry)
    }

    pub fn apply_write_scoped(&self, scope: &str, op: &str, entry: &str) -> anyhow::Result<String> {
        if contains_secret(entry) {
            anyhow::bail!("refused: entry looks like a secret (api key/token/private key); store a reference instead");
        }
        let (dir, mirror_target) = match scope {
            "project" => {
                let path = self.project_memory_path().ok_or_else(|| {
                    anyhow::anyhow!(
                        "no project detected (no .git above cwd); write scope=global instead"
                    )
                })?;
                let dir = path.parent().unwrap().to_path_buf();
                std::fs::create_dir_all(&dir)?;
                (dir, "project")
            }
            _ => {
                std::fs::create_dir_all(self.memories_dir())?;
                (self.memories_dir(), "memory")
            }
        };
        let path = dir.join(MEMORY_FILE);
        let mut content = std::fs::read_to_string(&path).unwrap_or_default();
        match op {
            "add" => {
                // 幂等：逐行 strip 后精确命中即 no-op（逐轮复盘会把同一事实反复建议，
                // 无脑追加会让 MEMORY.md 长出 N 行 identical；子串不算，不同表述照存）。
                let want = entry.trim();
                if content.lines().any(|l| l.trim() == want) {
                    tracing::debug!("memory add skipped (already present)");
                    return Ok(content);
                }
                content.push_str(entry);
                if !entry.ends_with('\n') {
                    content.push('\n');
                }
            }
            "replace" => {
                // entry 格式: OLD ||| NEW（简化版 replace）
                if let Some((old, new)) = entry.split_once("|||") {
                    content = content.replacen(old.trim(), new.trim(), 1);
                } else {
                    content = entry.to_string();
                }
            }
            "remove" => {
                content = content.replace(entry, "");
            }
            _ => anyhow::bail!("unknown memory op: {op}"),
        }
        std::fs::write(&path, &content)?;
        self.mirror_memory(mirror_target, entry);
        self.compact_file(&path);
        Ok(std::fs::read_to_string(&path).unwrap_or(content))
    }

    /// 写后压实：文件超限即按行丢最旧，保证磁盘文件永远有界（读路径不再是唯一防线）。
    fn compact_file(&self, path: &Path) {
        let content = std::fs::read_to_string(path).unwrap_or_default();
        let fitted = Self::fit_to_limit(&content, self.memory_char_limit);
        if fitted.len() < content.len() {
            tracing::info!(
                "memory compacted: {} -> {} bytes ({})",
                content.len(),
                fitted.len(),
                path.display()
            );
            let _ = std::fs::write(path, &fitted);
        }
    }

    pub fn memory_tool_definition(&self) -> Option<ToolDefinition> {
        if !self.memory_enabled && !self.user_profile_enabled {
            return None;
        }
        Some(ToolDefinition {
            name: "memory".into(),
            description: "Manage long-term memory (MEMORY.md): add/replace/remove entries; scope=project writes to the project's .rupi/MEMORY.md".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "op": {"type": "string", "enum": ["add", "replace", "remove"]},
                    "entry": {"type": "string"},
                    "scope": {"type": "string", "enum": ["global", "project"], "default": "global"}
                },
                "required": ["op", "entry"]
            }),
            prompt_snippet: Some("memory(op, entry, scope=global): persist durable facts across sessions".into()),
        })
    }

    pub fn memory_search_tool_definition() -> ToolDefinition {
        ToolDefinition {
            name: "memory_search".into(),
            description: "Search learned long-term memories (MEMORY.md / failures mirror): query past facts, lessons, failures".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "limit": {"type": "integer", "default": 5}
                },
                "required": ["query"]
            }),
            prompt_snippet: Some(
                "memory_search(query): recall learned facts/lessons/failures on demand".into(),
            ),
        }
    }

    /// 会话搜索工具定义：跨会话全文检索（sessions.db FTS），回忆“聊过的”上下文；
    /// 与 `memory_search`（“学到的”）互补。`--no-memory` 全关时随 memory 一并撤下。
    pub fn session_search_tool_definition() -> ToolDefinition {
        ToolDefinition {
            name: "session_search".into(),
            description: "Search past conversation sessions (full-text): recall what was discussed/done in earlier sessions".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "limit": {"type": "integer", "default": 5}
                },
                "required": ["query"]
            }),
            prompt_snippet: Some(
                "session_search(query): recall past conversations on demand".into(),
            ),
        }
    }

    /// 记忆引导语（对标 Hermes “给模型何存何取的指导”）：静态文本，前缀缓存安全；
    /// 空库时更要出现（否则模型永远不调 memory 工具，存→冻→忆的环转不起来）。
    /// 双开关全关时返回空（Hermes `memory_enabled=false`：工具与指导块一并撤下，
    /// 模型收不到用不了的工具）。
    pub fn guidance_block(&self) -> String {
        if !self.memory_enabled && !self.user_profile_enabled {
            return String::new();
        }
        "\n<MemoryGuidance>\nLong-term memory persists across sessions: MEMORY.md keeps durable facts (project conventions, environment, lessons learned), USER.md keeps user preferences. Save via the `memory` tool (op=add) when you learn something reusable — a preference, a correction, a gotcha; scope=project for repo-specific facts. Prefix an entry with `[core]` to pin it into every session's prompt; once any `[core]` entry exists, untagged entries form an extended tier that is only reachable via `memory_search`. Do NOT store ephemeral task state or secrets. Writes take effect in the prompt from the next session; the tool response shows live state. Recall with `memory_search` before asking the user twice; use `session_search` to recall what was discussed in earlier conversations; past failures arrive as <FailureMemory> — do not repeat them.\n</MemoryGuidance>\n".to_string()
    }

    /// 文件层子串检索（`memory_search` 的兜底）：手工编辑的行、`[core]` 分层下的 extended
    /// 行不一定进过 FTS 镜像，直接扫 MEMORY.md（全局/项目）与 failures.md。
    /// 返回 (target, 行文本)，大小写不敏感，最多 `limit` 条。
    pub fn grep_memory_files(&self, query: &str, limit: usize) -> Vec<(String, String)> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return vec![];
        }
        let mut sources: Vec<(&str, PathBuf)> = vec![
            ("memory", self.memories_dir().join(MEMORY_FILE)),
            ("failure", self.memories_dir().join(FAILURES_FILE)),
        ];
        if let Some(p) = self.project_memory_path() {
            sources.push(("project", p));
        }
        let mut out = vec![];
        for (target, path) in sources {
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            for line in content.lines() {
                if out.len() >= limit {
                    return out;
                }
                let t = line.trim();
                if !t.is_empty() && t.to_lowercase().contains(&q) {
                    out.push((target.to_string(), t.to_string()));
                }
            }
        }
        out
    }

    /// SQLite 镜像（best-effort）：成功写入的记忆同步一行到 sessions.db，
    /// 失败只 warning，绝不影响 markdown 主写入。复用本 store 的连接。
    fn mirror_memory(&self, target: &str, content: &str) {
        if let Err(e) = self.with_sessions(|db| db.mirror_memory_entry(target, content)) {
            tracing::warn!("memory mirror failed: {e:#}");
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct FrozenMemory {
    pub memory: String,
    pub user: String,
    pub failures: String,
}

impl FrozenMemory {
    pub fn system_block(&self) -> String {
        let mut s = String::new();
        if !self.memory.is_empty() {
            s.push_str(&format!(
                "\n<LongTermMemory>\n{}\n</LongTermMemory>\n",
                self.memory
            ));
        }
        if !self.user.is_empty() {
            s.push_str(&format!("\n<UserProfile>\n{}\n</UserProfile>\n", self.user));
        }
        if !self.failures.is_empty() {
            s.push_str(&format!(
                "\n<FailureMemory>\nPast failures — do not repeat these mistakes:\n{}\n</FailureMemory>\n",
                self.failures
            ));
        }
        s
    }
}

/// 最近一次 prefetch 注入了什么：确定性 recall 指示器的输入
/// （Hermes `RecallStatus` 对齐；`count == 0` 表有内容无离散计数，渲染为通用文案）。
#[derive(Debug, Clone)]
pub struct RecallStatus {
    pub provider_label: String,
    pub count: usize,
    pub glyph: String,
}

impl RecallStatus {
    pub fn new(provider_label: impl Into<String>, count: usize) -> Self {
        Self {
            provider_label: provider_label.into(),
            count,
            glyph: "🧠".into(),
        }
    }
}

/// 外部记忆 provider 契约：8 方法生命周期（Hermes `MemoryProvider` 对齐）。
#[async_trait]
pub trait MemoryProvider: Send + Sync {
    async fn initialize(&mut self, home: &Path) -> anyhow::Result<()>;
    /// 注入系统提示的记忆块。
    fn system_prompt_block(&self) -> String {
        String::new()
    }
    /// 每轮 API 调用前触发，必须立即返回（后台预热缓存，慢 backend 不阻塞首字）。
    async fn prefetch(&self) -> String {
        String::new()
    }
    /// 最近一次 `prefetch` 实际注入了什么（`None` = 无指示器）。
    /// 只反映 LAST prefetch，由 manager 在每次 `prefetch_all` 后快照，
    /// 寒暄门短路时快照清空，永不展示陈旧计数（Hermes `recall_status` 同约）。
    fn recall_status(&self) -> Option<RecallStatus> {
        None
    }
    /// 每轮结束后异步持久化。
    async fn sync_turn(&self, _user: &str, _assistant: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn tool_schemas(&self) -> Vec<ToolDefinition> {
        vec![]
    }
    async fn handle_tool_call(
        &self,
        _name: &str,
        _args: serde_json::Value,
    ) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
    async fn shutdown(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    /// 内建记忆写入时通知外部 provider（memory bridge）。
    async fn on_memory_write(&self, _op: &str, _entry: &str) -> anyhow::Result<()> {
        Ok(())
    }
    /// 压缩前钩子。
    async fn on_pre_compress(&self) -> anyhow::Result<()> {
        Ok(())
    }
    /// 会话结束钩子。
    async fn on_session_end(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// 编排内建 + 最多一个外部 provider；工具路由一次性建表；失败隔离
/// （prefetch 失败只记 debug，sync 失败记 warning，绝不崩主循环）。
pub struct MemoryManager {
    pub store: MemoryStore,
    external_name: Option<String>,
    external: Option<Box<dyn MemoryProvider>>,
    tool_to_provider: HashMap<String, String>,
    /// 最近一次 `prefetch_all` 实际注入的回想状态（寒暄门短路/超时即清空，
    /// `describe_recall` 只读快照，永不展示陈旧计数）。
    last_recall: std::sync::Mutex<Vec<RecallStatus>>,
}

impl MemoryManager {
    pub fn new(store: MemoryStore) -> Self {
        Self {
            store,
            external_name: None,
            external: None,
            tool_to_provider: HashMap::new(),
            last_recall: std::sync::Mutex::new(vec![]),
        }
    }

    /// 只允许一个外部 provider；第二个直接拒绝并 warning（防 schema 膨胀与后端冲突）。
    pub fn register_external(
        &mut self,
        name: String,
        provider: Box<dyn MemoryProvider>,
    ) -> anyhow::Result<()> {
        if self.external.is_some() {
            tracing::warn!(
                "second external memory provider '{name}' rejected; active is '{}'. Set memory.provider to switch.",
                self.external_name.as_deref().unwrap_or("?")
            );
            anyhow::bail!("only one external memory provider allowed");
        }
        for t in provider.tool_schemas() {
            if self.tool_to_provider.contains_key(&t.name) {
                tracing::warn!("memory tool name conflict: {}; first wins", t.name);
                continue;
            }
            self.tool_to_provider.insert(t.name.clone(), name.clone());
        }
        self.external_name = Some(name);
        self.external = Some(provider);
        Ok(())
    }

    pub fn all_tool_definitions(&self) -> Vec<ToolDefinition> {
        let mut out = vec![];
        if let Some(d) = self.store.memory_tool_definition() {
            out.push(d);
            out.push(MemoryStore::memory_search_tool_definition());
            out.push(MemoryStore::session_search_tool_definition());
        }
        if let Some(e) = &self.external {
            out.extend(e.tool_schemas());
        }
        out
    }

    pub fn system_block(&self, frozen: &FrozenMemory) -> String {
        let mut s = frozen.system_block();
        s.push_str(&self.store.guidance_block());
        if let Some(e) = &self.external {
            s.push_str(&e.system_prompt_block());
        }
        s
    }

    pub async fn prefetch_all(&self, query: &str) -> String {
        // 寒暄/应答/斜杠命令跳过外部 recall（对标 Hermes prefetch 门）：
        // 省后端往返，且陈旧上下文不带偏单字回复；显式 recall/memory_search 工具不受影响。
        // 短路时同步清空回想快照：describe_recall 永不展示上一轮的陈旧计数。
        if is_trivial_prompt(query) {
            tracing::debug!("memory prefetch skipped for trivial prompt");
            self.last_recall.lock().unwrap().clear();
            return String::new();
        }
        let text = match &self.external {
            Some(e) => {
                match tokio::time::timeout(std::time::Duration::from_secs(3), e.prefetch()).await {
                    Ok(s) => s,
                    Err(_) => {
                        tracing::debug!("memory prefetch timed out");
                        String::new()
                    }
                }
            }
            None => String::new(),
        };
        // 快照本轮实际注入：空文本（无货/超时/无 provider）即无指示器。
        let mut last = self.last_recall.lock().unwrap();
        last.clear();
        if !text.trim().is_empty() {
            if let Some(e) = &self.external {
                last.extend(e.recall_status());
            }
        }
        text
    }

    /// 确定性 recall 指示行（如 `🧠 jsonl — recalled 2 memories`），无注入回空串。
    /// 紧跟 `prefetch_all` 调，让用户即使模型沉默也看得到记忆被用了
    /// （Hermes `describe_recall` 同约；`count == 0` 渲染通用文案）。
    pub fn describe_recall(&self) -> String {
        let last = self.last_recall.lock().unwrap();
        let parts: Vec<String> = last
            .iter()
            .map(|s| {
                let detail = match s.count {
                    0 => "recalled relevant memory".to_string(),
                    1 => "recalled 1 memory".to_string(),
                    n => format!("recalled {n} memories"),
                };
                format!("{} {} — {detail}", s.glyph, s.provider_label)
            })
            .collect();
        parts.join("  ")
    }

    pub async fn sync_all(&self, user: &str, assistant: &str) {
        if let Some(e) = &self.external {
            if let Err(err) = e.sync_turn(user, assistant).await {
                tracing::warn!("memory sync failed: {err:#}");
            }
        }
    }

    /// 压缩前钩子（Hermes `_compress_context` 对齐）：给外部 provider 落盘/收尾机会，失败只 warning。
    pub async fn pre_compress_all(&self) {
        if let Some(e) = &self.external {
            if let Err(err) = e.on_pre_compress().await {
                tracing::warn!("memory pre_compress failed: {err:#}");
            }
        }
    }

    pub async fn handle_tool_call(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> anyhow::Result<Option<String>> {
        if name == "memory" {
            let op = args
                .get("op")
                .and_then(|v| v.as_str())
                .unwrap_or("add")
                .to_string();
            let entry = args
                .get("entry")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let scope = args
                .get("scope")
                .and_then(|v| v.as_str())
                .unwrap_or("global")
                .to_string();
            let store = self.store.clone();
            let live =
                tokio::task::spawn_blocking(move || store.apply_write_scoped(&scope, &op, &entry))
                    .await
                    .map_err(|e| anyhow::anyhow!("memory write join: {e}"))??;
            if let Some(e) = &self.external {
                let _ = e
                    .on_memory_write(
                        args.get("op").and_then(|v| v.as_str()).unwrap_or("add"),
                        args.get("entry").and_then(|v| v.as_str()).unwrap_or(""),
                    )
                    .await;
            }
            return Ok(Some(format!(
                "memory updated (live). Takes effect in prompt next session.\n{live}"
            )));
        }
        if name == "memory_search" {
            let query = args
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let limit = args
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(5)
                .min(20) as usize;
            let store = self.store.clone();
            let hits = tokio::task::spawn_blocking(move || {
                store.with_sessions(|db| {
                    let mut hits = db.memory_search(&query, limit)?;
                    let norm =
                        |s: &str| s.replace("<b>", "").replace("</b>", "").trim().to_string();
                    for (target, line) in store.grep_memory_files(&query, limit) {
                        if hits.len() >= limit {
                            break;
                        }
                        let l = norm(&line);
                        if !hits.iter().any(|(_, s)| {
                            let n = norm(s);
                            n == l || l.contains(&n) || n.contains(&l)
                        }) {
                            hits.push((target, line));
                        }
                    }
                    Ok(hits)
                })
            })
            .await
            .map_err(|e| anyhow::anyhow!("memory_search join: {e}"))??;
            if hits.is_empty() {
                return Ok(Some("no matching memories".into()));
            }
            let lines: Vec<String> = hits
                .iter()
                .map(|(target, snippet)| format!("[{target}] {snippet}"))
                .collect();
            return Ok(Some(lines.join("\n---\n")));
        }
        if name == "session_search" {
            // 会话是“聊过的”：跨会话 FTS，按 session_id 分组展示，snippet 即上下文。
            let query = args
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let limit = args
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(5)
                .min(20) as usize;
            let store = self.store.clone();
            let hits = tokio::task::spawn_blocking(move || {
                store.with_sessions(|db| db.search(&query, limit))
            })
            .await
            .map_err(|e| anyhow::anyhow!("session_search join: {e}"))??;
            if hits.is_empty() {
                return Ok(Some("no matching sessions".into()));
            }
            let lines: Vec<String> = hits
                .iter()
                .map(|(sid, snippet)| format!("[session {sid}] {snippet}"))
                .collect();
            return Ok(Some(lines.join("\n---\n")));
        }
        if let Some(e) = &self.external {
            if self.tool_to_provider.contains_key(name) {
                return e.handle_tool_call(name, args).await;
            }
        }
        Ok(None)
    }
}

/// 示例外部 provider：JSONL 回放日志（`turns.jsonl`）。
/// 对标 Hermes 的 Honcho/Mem0 插件位：`prefetch` 读后台缓存绝不阻塞，
/// `sync_turn` 追加持久化，另带一个 `recall` 工具做关键词回想。
pub struct JsonlProvider {
    file: PathBuf,
    recent: std::sync::Mutex<Vec<String>>,
    recent_n: usize,
    /// 最近一次 `prefetch` 实际返回的条数（`recall_status` 只读此快照，不读旧账）。
    last_hits: std::sync::Mutex<usize>,
}

impl JsonlProvider {
    pub fn new(recent_n: usize) -> Self {
        Self {
            file: PathBuf::new(),
            recent: std::sync::Mutex::new(vec![]),
            recent_n,
            last_hits: std::sync::Mutex::new(0),
        }
    }

    fn append_line(&self, line: &str) -> anyhow::Result<()> {
        use std::io::Write as _;
        // 防腐：文件非空且缺尾换行（外部编辑/上次崩溃截断）时先补分隔符，
        // 否则新行与旧尾粘连成一条非法 JSON（上游 #8345 同修）。
        if self.file.exists() && std::fs::metadata(&self.file)?.len() > 0 {
            let bytes = std::fs::read(&self.file)?;
            if bytes.last() != Some(&b'\n') {
                let mut f = std::fs::OpenOptions::new().append(true).open(&self.file)?;
                f.write_all(b"\n")?;
            }
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.file)?;
        writeln!(f, "{line}")?;
        Ok(())
    }

    fn reload_cache(&self) {
        let content = std::fs::read_to_string(&self.file).unwrap_or_default();
        let mut lines: Vec<String> = content
            .lines()
            .rev()
            .take(self.recent_n)
            .map(|s| s.to_string())
            .collect();
        lines.reverse();
        *self.recent.lock().unwrap() = lines;
    }
}

#[async_trait]
impl MemoryProvider for JsonlProvider {
    async fn initialize(&mut self, home: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(home)?;
        self.file = home.join("turns.jsonl");
        if !self.file.exists() {
            std::fs::write(&self.file, "")?;
        }
        self.reload_cache();
        Ok(())
    }

    fn system_prompt_block(&self) -> String {
        "\n<ExternalMemory provider=\"jsonl\">Recent turns are prefetched below; use recall(query) to search history.</ExternalMemory>\n".to_string()
    }

    async fn prefetch(&self) -> String {
        // 必须立即返回：只读内存缓存，后台 sync 后刷新；同步记录条数供 recall_status
        let guard = self.recent.lock().unwrap();
        let n = if guard.iter().any(|l| !l.trim().is_empty()) {
            guard.len()
        } else {
            0
        };
        let out = guard.join("\n");
        *self.last_hits.lock().unwrap() = n;
        out
    }

    fn recall_status(&self) -> Option<RecallStatus> {
        let n = *self.last_hits.lock().unwrap();
        if n == 0 {
            None
        } else {
            Some(RecallStatus::new("jsonl", n))
        }
    }

    async fn sync_turn(&self, user: &str, assistant: &str) -> anyhow::Result<()> {
        let line = serde_json::json!({
            "ts": chrono::Utc::now().to_rfc3339(),
            "user": user.chars().take(500).collect::<String>(),
            "assistant": assistant.chars().take(500).collect::<String>(),
        })
        .to_string();
        self.append_line(&line)?;
        self.reload_cache();
        Ok(())
    }

    fn tool_schemas(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "recall".into(),
            description: "Search past turns in external memory".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"query": {"type": "string"}},
                "required": ["query"]
            }),
            prompt_snippet: Some("recall(query): search past turns".into()),
        }]
    }

    async fn handle_tool_call(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> anyhow::Result<Option<String>> {
        if name != "recall" {
            return Ok(None);
        }
        let q = args
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_lowercase();
        let content = std::fs::read_to_string(&self.file).unwrap_or_default();
        let hits: Vec<String> = content
            .lines()
            .filter(|l| l.to_lowercase().contains(&q))
            .rev()
            .take(5)
            .map(|s| s.to_string())
            .collect();
        Ok(Some(if hits.is_empty() {
            "no matches".to_string()
        } else {
            hits.join("\n")
        }))
    }
}

// ---- session store (SQLite + FTS5) ----
pub struct SessionStore {
    conn: rusqlite::Connection,
}

/// 一轮落盘的一条消息（事务内批量 INSERT）。
#[derive(Debug, Clone)]
pub struct SessionMessageRow {
    pub id: String,
    pub role: String,
    pub content: String,
    pub blocks: Option<String>,
}

pub mod session_io;
pub use session_io::{
    export_store_html, export_store_jsonl, export_tree_html, export_tree_jsonl, fork_session,
    format_session_status, import_into_store, import_jsonl, looks_like_session_path, open_session,
    persist_tree, remap_tree, resolve_session_id, resolve_session_ref, restore_tree,
    tree_from_records, ImportedSession, OpenedSession, SessionHeader,
};

/// 用户查询转 FTS5 短语：裸 `-` / `:` / `*` 等会被当运算符导致 syntax error，
/// 包一层双引号按字面短语查（分词仍按 tokenizer 来，不影响中英文关键词）。
fn fts_phrase(query: &str) -> String {
    format!("\"{}\"", query.replace('"', "\"\""))
}

/// 当前 schema：v2 trigram FTS，v3 messages.blocks。已到此版本则跳过 DDL/回填。
const SCHEMA_USER_VERSION: i64 = 3;

fn apply_connection_pragmas(conn: &rusqlite::Connection) -> anyhow::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.busy_timeout(Duration::from_millis(5_000))?;
    Ok(())
}

fn migrate_schema(conn: &rusqlite::Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS sessions(id TEXT PRIMARY KEY, profile TEXT, created_at TEXT, summary TEXT);
         CREATE TABLE IF NOT EXISTS messages(id TEXT PRIMARY KEY, session_id TEXT, role TEXT, content TEXT, created_at TEXT, blocks TEXT);
         -- trigram 分词：中英文统一按子串可召回（unicode61 把中文整句当一个 token，
         -- 子串永远查不到）；case_sensitive 0 保英文大小写不敏感
         CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(content, content='messages', content_rowid='rowid', tokenize='trigram case_sensitive 0');
         -- 外部内容表必须靠触发器同步，否则 FTS 永远查不到
         CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
           INSERT INTO messages_fts(rowid, content) VALUES (new.rowid, new.content);
         END;
         CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
           INSERT INTO messages_fts(messages_fts, rowid, content) VALUES ('delete', old.rowid, old.content);
         END;
         -- 扩展记忆镜像（Hermes memories 对齐）：MEMORY.md / failures.md 写入即镜像一行，
         -- `memory_search` 按需查，不注入每轮 prompt
         CREATE TABLE IF NOT EXISTS memories(
           id INTEGER PRIMARY KEY AUTOINCREMENT,
           target TEXT NOT NULL,
           content TEXT NOT NULL,
           created_at TEXT NOT NULL
         );
         CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(content, content='memories', content_rowid='rowid', tokenize='trigram case_sensitive 0');
         CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
           INSERT INTO memory_fts(rowid, content) VALUES (new.rowid, new.content);
         END;
         CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
           INSERT INTO memory_fts(memory_fts, rowid, content) VALUES ('delete', old.rowid, old.content);
         END;
         -- 存量补索引（老库升级路径）
         INSERT INTO messages_fts(rowid, content)
           SELECT rowid, content FROM messages
           WHERE rowid NOT IN (SELECT rowid FROM messages_fts);",
    )?;
    // 分词器迁移（user_version<2 的 unicode61 老库）：FTS 表只是内容表的索引位，
    // 删了按 trigram 重建再全量回填即可，触发器不受影响；新库建表即 trigram，直接标版本。
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 2 {
        conn.execute_batch(
            "DROP TABLE IF EXISTS messages_fts;
             CREATE VIRTUAL TABLE messages_fts USING fts5(content, content='messages', content_rowid='rowid', tokenize='trigram case_sensitive 0');
             INSERT INTO messages_fts(rowid, content) SELECT rowid, content FROM messages;
             DROP TABLE IF EXISTS memory_fts;
             CREATE VIRTUAL TABLE memory_fts USING fts5(content, content='memories', content_rowid='rowid', tokenize='trigram case_sensitive 0');
             INSERT INTO memory_fts(rowid, content) SELECT rowid, content FROM memories;
             PRAGMA user_version = 2;",
        )?;
    }
    // v3：messages.blocks —— 完整消息 JSON（含工具调用/结果/思考块），`--resume` 据此
    // 结构化回填；老库补列（content 列语义不变，FTS 只索引 content）。
    let has_blocks = {
        let mut st = conn.prepare_cached("PRAGMA table_info(messages)")?;
        let cols: Vec<String> = st
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<Result<Vec<_>, _>>()?;
        cols.iter().any(|c| c == "blocks")
    };
    if !has_blocks {
        conn.execute_batch("ALTER TABLE messages ADD COLUMN blocks TEXT;")?;
    }
    if version < SCHEMA_USER_VERSION {
        conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_USER_VERSION};"))?;
    }
    Ok(())
}

impl SessionStore {
    pub fn open(home: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(home)?;
        let db_path = home.join("sessions.db");
        let conn = rusqlite::Connection::open(&db_path)?;
        // 每条连接都要设：WAL / 同步级别 / 忙等。DDL 只在 schema 未到最新时跑
        //（同进程反复 open 已迁库不再重放 CREATE/FTS 回填；删库重建 user_version=0 会重跑）。
        apply_connection_pragmas(&conn)?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < SCHEMA_USER_VERSION {
            migrate_schema(&conn)?;
        }
        // v4：会话展示名 / cwd / 更新时间 / 父会话（--continue / --name / /fork）
        let sess_cols = {
            let mut st = conn.prepare("PRAGMA table_info(sessions)")?;
            let cols: Vec<String> = st
                .query_map([], |r| r.get::<_, String>(1))?
                .collect::<Result<Vec<_>, _>>()?;
            cols
        };
        let add = |conn: &rusqlite::Connection, col: &str, decl: &str| -> anyhow::Result<()> {
            if !sess_cols.iter().any(|c| c == col) {
                conn.execute_batch(&format!("ALTER TABLE sessions ADD COLUMN {col} {decl};"))?;
            }
            Ok(())
        };
        add(&conn, "name", "TEXT")?;
        add(&conn, "cwd", "TEXT")?;
        add(&conn, "updated_at", "TEXT")?;
        add(&conn, "parent_session", "TEXT")?;
        if version < 4 {
            conn.execute_batch("PRAGMA user_version = 4;")?;
        }
        Ok(Self { conn })
    }

    fn execute_cached(&self, sql: &str, params: impl rusqlite::Params) -> anyhow::Result<usize> {
        Ok(self.conn.prepare_cached(sql)?.execute(params)?)
    }

    pub fn create_session(&self, profile: &str) -> anyhow::Result<String> {
        self.create_session_ex(profile, None, None, None)
    }

    pub fn create_session_ex(
        &self,
        profile: &str,
        name: Option<&str>,
        cwd: Option<&str>,
        parent_session: Option<&str>,
    ) -> anyhow::Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now().to_rfc3339();
        self.execute_cached(
            "INSERT INTO sessions(id, profile, created_at, summary, name, cwd, updated_at, parent_session)
             VALUES(?,?,?,?,?,?,?,?)",
            rusqlite::params![
                id,
                profile,
                now,
                String::new(),
                name,
                cwd,
                now,
                parent_session
            ],
        )?;
        Ok(id)
    }

    pub fn set_name(&self, session_id: &str, name: &str) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE sessions SET name = ?, updated_at = ? WHERE id = ?",
            rusqlite::params![name, chrono::Utc::now().to_rfc3339(), session_id],
        )?;
        Ok(())
    }

    pub fn get_name(&self, session_id: &str) -> anyhow::Result<Option<String>> {
        let n: Option<String> = self.conn.query_row(
            "SELECT name FROM sessions WHERE id = ?",
            rusqlite::params![session_id],
            |r| r.get(0),
        )?;
        Ok(n.filter(|s| !s.is_empty()))
    }

    pub fn get_cwd(&self, session_id: &str) -> anyhow::Result<Option<String>> {
        let n: Option<String> = self.conn.query_row(
            "SELECT cwd FROM sessions WHERE id = ?",
            rusqlite::params![session_id],
            |r| r.get(0),
        )?;
        Ok(n.filter(|s| !s.is_empty()))
    }

    pub fn get_parent_session(&self, session_id: &str) -> anyhow::Result<Option<String>> {
        let n: Option<String> = self.conn.query_row(
            "SELECT parent_session FROM sessions WHERE id = ?",
            rusqlite::params![session_id],
            |r| r.get(0),
        )?;
        Ok(n.filter(|s| !s.is_empty()))
    }

    pub fn touch(&self, session_id: &str) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE sessions SET updated_at = ? WHERE id = ?",
            rusqlite::params![chrono::Utc::now().to_rfc3339(), session_id],
        )?;
        Ok(())
    }

    /// 最近会话：优先同 cwd，否则按 `updated_at`/`created_at` 倒序。
    pub fn latest_session(&self, cwd: Option<&str>) -> anyhow::Result<Option<String>> {
        if let Some(cwd) = cwd {
            let hit: Option<String> = self
                .conn
                .query_row(
                    "SELECT id FROM sessions WHERE cwd = ?
                     ORDER BY COALESCE(updated_at, created_at) DESC LIMIT 1",
                    rusqlite::params![cwd],
                    |r| r.get(0),
                )
                .optional()?;
            if hit.is_some() {
                return Ok(hit);
            }
        }
        let hit = self
            .conn
            .query_row(
                "SELECT id FROM sessions ORDER BY COALESCE(updated_at, created_at) DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        Ok(hit)
    }

    /// 写回压缩摘要（`sessions.summary`）。
    pub fn set_summary(&self, session_id: &str, summary: &str) -> anyhow::Result<()> {
        self.execute_cached(
            "UPDATE sessions SET summary = ? WHERE id = ?",
            rusqlite::params![summary, session_id],
        )?;
        Ok(())
    }

    /// 会话是否存在（`session-show` 未知 id 给提示，不与空会话混淆）。
    pub fn has_session(&self, session_id: &str) -> anyhow::Result<bool> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT COUNT(*) FROM sessions WHERE id = ?")?;
        Ok(stmt.query_row(rusqlite::params![session_id], |r| r.get::<_, i64>(0))? > 0)
    }

    /// 读回压缩摘要（resume 时可预热窗口；当前 CLI 只展示）。
    pub fn get_summary(&self, session_id: &str) -> anyhow::Result<String> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT summary FROM sessions WHERE id = ?")?;
        Ok(stmt.query_row(rusqlite::params![session_id], |r| r.get(0))?)
    }

    pub fn add_message(&self, session_id: &str, role: &str, content: &str) -> anyhow::Result<()> {
        self.add_message_with_id(&uuid::Uuid::new_v4().to_string(), session_id, role, content)
    }

    /// 指定行 id 写入（回合落盘时沿用树节点 id，resume 后短 id 稳定可 /goto）。
    pub fn add_message_with_id(
        &self,
        id: &str,
        session_id: &str,
        role: &str,
        content: &str,
    ) -> anyhow::Result<()> {
        self.add_message_full(id, session_id, role, content, None)
    }

    /// 完整落盘：`content` 存纯文本（FTS/展示），`blocks` 存整条消息 JSON（工具调用、
    /// 工具结果、思考块原样），resume 时优先按 `blocks` 回填。
    pub fn add_message_full(
        &self,
        id: &str,
        session_id: &str,
        role: &str,
        content: &str,
        blocks: Option<&str>,
    ) -> anyhow::Result<()> {
        self.persist_turn(
            session_id,
            &[SessionMessageRow {
                id: id.to_string(),
                role: role.to_string(),
                content: content.to_string(),
                blocks: blocks.map(str::to_string),
            }],
            None,
        )?;
        Ok(())
    }

    /// 一轮落盘：3–4 条 INSERT + touch（+ 可选 summary）合成一个事务。
    pub fn persist_turn(
        &self,
        session_id: &str,
        rows: &[SessionMessageRow],
        summary: Option<&str>,
    ) -> anyhow::Result<usize> {
        // Connection 在 `&self` 后：调用方（Mutex / with_sessions）已串行化。
        let tx = self.conn.unchecked_transaction()?;
        let now = chrono::Utc::now().to_rfc3339();
        {
            let mut ins = tx.prepare(
                "INSERT INTO messages(id, session_id, role, content, created_at, blocks) VALUES(?,?,?,?,?,?)",
            )?;
            for r in rows {
                ins.execute(rusqlite::params![
                    r.id, session_id, r.role, r.content, now, r.blocks
                ])?;
            }
        }
        tx.execute(
            "UPDATE sessions SET updated_at = ? WHERE id = ?",
            rusqlite::params![now, session_id],
        )?;
        if let Some(s) = summary {
            tx.execute(
                "UPDATE sessions SET summary = ? WHERE id = ?",
                rusqlite::params![s, session_id],
            )?;
        }
        tx.commit()?;
        Ok(rows.len())
    }

    /// session_search：跨会话全文检索（FTS5），供 agent 回忆历史上下文。
    /// FTS 表只存 content，session_id 回 join messages 取。
    pub fn search(&self, query: &str, limit: usize) -> anyhow::Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT m.session_id, snippet(messages_fts, 0, '<b>', '</b>', '...', 20)
             FROM messages_fts JOIN messages m ON m.rowid = messages_fts.rowid
             WHERE messages_fts MATCH ? ORDER BY rank LIMIT ?",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![fts_phrase(query), limit as i64], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<Result<Vec<(String, String)>, _>>()?;
        Ok(rows)
    }

    /// 最近会话：id / profile / 创建时间 / 消息数 / 展示名（倒序）。
    pub fn list_sessions(
        &self,
        limit: usize,
    ) -> anyhow::Result<Vec<(String, String, String, i64, String)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT s.id, s.profile, s.created_at, COUNT(m.id), COALESCE(s.name, '')
             FROM sessions s LEFT JOIN messages m ON m.session_id = s.id
             GROUP BY s.id ORDER BY COALESCE(s.updated_at, s.created_at) DESC LIMIT ?",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![limit as i64], |r| {
                let id: String = r.get(0)?;
                let profile: String = r.get(1)?;
                let created: String = r.get(2)?;
                let count: i64 = r.get(3)?;
                let name: String = r.get(4)?;
                Ok((id, profile, created, count, name))
            })?
            .collect::<Result<Vec<(String, String, String, i64, String)>, _>>()?;
        Ok(rows)
    }

    /// 扩展记忆镜像写入（target: 'memory' | 'failure'），供 `memory_search` 查询。
    pub fn mirror_memory_entry(&self, target: &str, content: &str) -> anyhow::Result<()> {
        self.execute_cached(
            "INSERT INTO memories(target, content, created_at) VALUES(?,?,?)",
            rusqlite::params![target, content, chrono::Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// memory_search：查 markdown 记忆镜像（MEMORY.md / failures.md 的成功写入）。
    /// 与 session_search 分开：记忆是“学到的”，会话是“聊过的”。
    pub fn memory_search(
        &self,
        query: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT m.target, snippet(memory_fts, 0, '<b>', '</b>', '...', 20)
             FROM memory_fts JOIN memories m ON m.rowid = memory_fts.rowid
             WHERE memory_fts MATCH ? ORDER BY rank LIMIT ?",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![fts_phrase(query), limit as i64], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<Result<Vec<(String, String)>, _>>()?;
        Ok(rows)
    }

    /// 会话明细：按时间正序（rowid 打平同秒并列）。返回 (行id, role, content, created)，
    /// 行 id 即树节点 id（落盘时沿用），resume 后 `/goto 短id` 可用。
    pub fn session_messages(
        &self,
        session_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<(String, String, String, String)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, role, content, created_at FROM messages WHERE session_id = ? ORDER BY created_at ASC, rowid ASC LIMIT ?",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![session_id, limit as i64], |r| {
                let id: String = r.get(0)?;
                let role: String = r.get(1)?;
                let content: String = r.get(2)?;
                let created: String = r.get(3)?;
                Ok((id, role, content, created))
            })?
            .collect::<Result<Vec<(String, String, String, String)>, _>>()?;
        Ok(rows)
    }

    /// 会话明细（结构化）：同 `session_messages` 排序，多带 `blocks`（完整消息 JSON，
    /// 老库行为 None）。`--resume` / TUI `/resume` 用它按结构回填工具上下文。
    pub fn session_records(
        &self,
        session_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<SessionRecord>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, role, content, created_at, blocks FROM messages WHERE session_id = ? ORDER BY created_at ASC, rowid ASC LIMIT ?",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![session_id, limit as i64], |r| {
                Ok(SessionRecord {
                    id: r.get(0)?,
                    role: r.get(1)?,
                    content: r.get(2)?,
                    created_at: r.get(3)?,
                    blocks: r.get(4)?,
                })
            })?
            .collect::<Result<Vec<SessionRecord>, _>>()?;
        Ok(rows)
    }
}

/// sessions.db 一行消息（`session_records` 返回）。
#[derive(Debug, Clone)]
pub struct SessionRecord {
    pub id: String,
    pub role: String,
    pub content: String,
    pub created_at: String,
    /// 完整消息 JSON（`rupi_core::Message`）；老库行 / 纯文本落盘为 None。
    pub blocks: Option<String>,
}

impl SessionRecord {
    /// 库行 → 消息：有 `blocks` 按结构回填（工具调用/结果原样）；否则退回纯文本
    /// （老库 assistant/user 行；理论上不存在的老 `tool` 行降级为用户可见的结果文本）。
    pub fn to_message(&self) -> rupi_core::Message {
        if let Some(json) = &self.blocks {
            if let Ok(m) = serde_json::from_str::<rupi_core::Message>(json) {
                return m;
            }
        }
        match self.role.as_str() {
            "assistant" => {
                rupi_core::Message::text(rupi_core::Role::Assistant, self.content.clone())
            }
            "tool" => rupi_core::Message::text(
                rupi_core::Role::User,
                format!("[tool result]\n{}", self.content),
            ),
            _ => rupi_core::Message::text(rupi_core::Role::User, self.content.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frozen_snapshot_is_stable_across_writes() {
        let home = std::env::temp_dir().join(format!("rupi-mem-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let store = MemoryStore::new(home.clone());
        store.apply_write("add", "likes tea").unwrap();
        let frozen = store.frozen_snapshot();
        store.apply_write("add", "likes coffee").unwrap();
        // 快照保持冻结，新写入只在下次 snapshot 可见
        assert!(frozen.memory.contains("tea"));
        assert!(!frozen.memory.contains("coffee"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn fit_to_limit_never_panics_on_multibyte_tail() {
        // 单行 6000 个 CJK 字符：旧字节切片会在字符中间切断而 panic
        let line: String = "中".repeat(6000);
        let out = MemoryStore::fit_to_limit(&line, 5000);
        assert!(out.starts_with("...[truncated]"));
        assert!(out.ends_with('中'));
        assert!(out.chars().count() <= 5000 + 32);
    }

    #[test]
    fn single_external_provider_limit() {
        struct P;
        #[async_trait]
        impl MemoryProvider for P {
            async fn initialize(&mut self, _home: &Path) -> anyhow::Result<()> {
                Ok(())
            }
        }
        let home = std::env::temp_dir().join("rupi-mem-x");
        let mut m = MemoryManager::new(MemoryStore::new(home));
        assert!(m.register_external("a".into(), Box::new(P)).is_ok());
        struct Q;
        #[async_trait]
        impl MemoryProvider for Q {
            async fn initialize(&mut self, _home: &Path) -> anyhow::Result<()> {
                Ok(())
            }
        }
        assert!(m.register_external("b".into(), Box::new(Q)).is_err());
    }

    #[test]
    fn trivial_prompt_gate_matches_hermes_contract() {
        // 命中：空/空白、斜杠命令、纯招呼与确认（含标点尾与大小写）
        for t in [
            "",
            "   ",
            "/tree",
            "/model gpt",
            "hi",
            "hi!",
            "Hi :)",
            "thanks",
            "Thank you",
            "thank you!",
            "done???",
            "OK",
            "ok.",
            "y",
            "k",
            "lgtm",
            "go ahead.",
            "do it!",
            "proceed",
            "got it",
            "yeah...",
            "  nope  ",
        ] {
            assert!(is_trivial_prompt(t), "应判寒暄：{t:?}");
        }
        // 不命中：词内前缀（k8s/yolo/note）、带实义尾巴、正常提问
        for t in [
            "k8s",
            "yolo",
            "note",
            "hi there",
            "hello world",
            "superman",
            "ok then",
            "notes on auth?",
            "how are you?",
            "thanks for the fish recipe",
            "done with the migration?",
            "next step is what",
            "continue from line 42",
        ] {
            assert!(!is_trivial_prompt(t), "误判寒暄：{t:?}");
        }
    }

    #[tokio::test]
    async fn manager_prefetch_skips_trivial_prompts() {
        let home = std::env::temp_dir().join(format!("rupi-mem-gate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let mut mgr = MemoryManager::new(MemoryStore::new(home.clone()));
        let mut p = JsonlProvider::new(10);
        p.initialize(&home).await.unwrap();
        mgr.register_external("jsonl".into(), Box::new(p)).unwrap();
        mgr.sync_all("buy oolong tea", "noted").await;
        // 实义 query 走外部 recall，非空
        assert!(!mgr.prefetch_all("what tea did I buy?").await.is_empty());
        // 寒暄/斜杠直接短路，纵使缓存有货也不打扰
        assert_eq!(mgr.prefetch_all("hi").await, "");
        assert_eq!(mgr.prefetch_all("thanks!").await, "");
        assert_eq!(mgr.prefetch_all("/tree").await, "");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn recall_status_tracks_last_prefetch_only() {
        // provider 级：未 prefetch/空缓存 → None；有货 → Some(n)，只反映 LAST。
        let home = std::env::temp_dir().join(format!("rupi-mem-recall-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let mut p = JsonlProvider::new(10);
        p.initialize(&home).await.unwrap();
        assert!(p.recall_status().is_none(), "未 prefetch 不应有状态");
        assert_eq!(p.prefetch().await, "");
        assert!(p.recall_status().is_none(), "空缓存不应有状态");
        p.sync_turn("buy oolong tea", "noted").await.unwrap();
        p.sync_turn("buy green tea", "noted").await.unwrap();
        let ctx = p.prefetch().await;
        assert!(ctx.contains("oolong"));
        let st = p.recall_status().expect("有注入应有状态");
        assert_eq!(st.provider_label, "jsonl");
        assert_eq!(st.count, 2);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn describe_recall_reports_and_clears_on_trivial() {
        // manager 级：实义 query 报计数；随后寒暄短路必须同步清空（陈旧计数不清零就是 bug）。
        let home = std::env::temp_dir().join(format!("rupi-mem-describe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let mut mgr = MemoryManager::new(MemoryStore::new(home.clone()));
        let mut p = JsonlProvider::new(10);
        p.initialize(&home).await.unwrap();
        mgr.register_external("jsonl".into(), Box::new(p)).unwrap();
        assert_eq!(mgr.describe_recall(), "", "无注入时无指示器");
        mgr.sync_all("buy oolong tea", "noted").await;
        mgr.sync_all("buy green tea", "noted").await;
        let ctx = mgr.prefetch_all("what tea did I buy?").await;
        assert!(!ctx.is_empty());
        assert_eq!(mgr.describe_recall(), "🧠 jsonl — recalled 2 memories");
        assert_eq!(mgr.prefetch_all("hi").await, "");
        assert_eq!(mgr.describe_recall(), "", "寒暄门必须同步清空回想快照");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn jsonl_provider_prefetch_sync_recall() {
        let home = std::env::temp_dir().join(format!("rupi-mem-jsonl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let mut p = JsonlProvider::new(10);
        p.initialize(&home).await.unwrap();
        assert_eq!(p.prefetch().await, "");
        p.sync_turn("hello world", "hi there").await.unwrap();
        assert!(p.prefetch().await.contains("hello world"));
        let hit = p
            .handle_tool_call("recall", serde_json::json!({"query": "hello"}))
            .await
            .unwrap()
            .unwrap();
        assert!(hit.contains("hello world"));
        let miss = p
            .handle_tool_call("recall", serde_json::json!({"query": "zzz-no-match"}))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(miss, "no matches");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn jsonl_append_repairs_missing_trailing_newline() {
        let home = std::env::temp_dir().join(format!("rupi-mem-jsonl-eol-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let mut p = JsonlProvider::new(10);
        p.initialize(&home).await.unwrap();
        // 模拟外部编辑/崩溃留下的无尾换行文件
        std::fs::write(home.join("turns.jsonl"), r#"{"ts":"t0","user":"old"}"#).unwrap();
        p.sync_turn("new turn", "reply").await.unwrap();
        let content = std::fs::read_to_string(home.join("turns.jsonl")).unwrap();
        // 两行各自合法 JSON：旧尾与新行没有粘连
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        for l in lines {
            serde_json::from_str::<serde_json::Value>(l).expect("each line valid JSON");
        }
        assert!(content.ends_with('\n'));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn manager_routes_external_recall_tool() {
        let home = std::env::temp_dir().join(format!("rupi-mem-mgr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let mut mgr = MemoryManager::new(MemoryStore::new(home.clone()));
        let mut p = JsonlProvider::new(10);
        p.initialize(&home).await.unwrap();
        mgr.register_external("jsonl".into(), Box::new(p)).unwrap();
        // recall 工具对模型可见
        assert!(mgr
            .all_tool_definitions()
            .iter()
            .any(|d| d.name == "recall"));
        // sync 后经 manager 路由可查到
        mgr.sync_all("buy oolong tea", "noted").await;
        let hit = mgr
            .handle_tool_call("recall", serde_json::json!({"query": "oolong"}))
            .await
            .unwrap()
            .unwrap();
        assert!(hit.contains("oolong"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn manager_routes_session_search_tool() {
        // session_search 对模型可见 + 经 manager 路由查 sessions.db FTS。
        let home = std::env::temp_dir().join(format!("rupi-mem-sess-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let mgr = MemoryManager::new(MemoryStore::new(home.clone()));
        assert!(mgr
            .all_tool_definitions()
            .iter()
            .any(|d| d.name == "session_search"));
        let db = SessionStore::open(&home).unwrap();
        db.add_message("sess-old", "user", "purple elephant deployment notes")
            .unwrap();
        let hit = mgr
            .handle_tool_call("session_search", serde_json::json!({"query": "elephant"}))
            .await
            .unwrap()
            .unwrap();
        assert!(hit.contains("sess-old"), "{hit}");
        assert!(hit.contains("elephant"), "{hit}");
        let miss = mgr
            .handle_tool_call(
                "session_search",
                serde_json::json!({"query": "zzz-no-match"}),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(miss, "no matching sessions");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn secrets_are_refused_not_persisted() {
        let home = std::env::temp_dir().join(format!("rupi-mem-sec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let store = MemoryStore::new(home.clone());
        assert!(store
            .apply_write("add", "my api_key = sk-abcdef1234567890")
            .is_err());
        assert!(store
            .apply_write("add", "token: ghp_deadbeefcafe1234")
            .is_err());
        assert!(store
            .apply_write("add", "-----BEGIN RSA PRIVATE KEY-----")
            .is_err());
        assert!(store.record_failure("AKIAIOSFODNN7EXAMPLE leaked").is_err());
        // 拒绝后无残留
        assert!(store.memory_text().is_empty());
        assert!(store.failures_text().is_empty());
        // 正常写入不受影响
        store.apply_write("add", "likes tea").unwrap();
        assert!(store.memory_text().contains("tea"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn guidance_present_when_enabled_absent_when_fully_disabled() {
        let home = std::env::temp_dir().join(format!("rupi-mem-guide-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let store = MemoryStore::new(home.clone());
        // 空库也有指导块：否则模型永远不调 memory 工具
        let g = store.guidance_block();
        assert!(g.contains("MemoryGuidance"));
        assert!(g.contains("memory_search"));
        // manager 系统块 = 冻结内容 + 指导（静态文本，前缀缓存安全）
        let mgr = MemoryManager::new(store);
        let frozen = FrozenMemory::default();
        assert!(mgr.system_block(&frozen).contains("MemoryGuidance"));
        // 双关：工具 schema 与指导块一并撤下，模型收不到用不了的工具
        assert!(mgr
            .all_tool_definitions()
            .iter()
            .any(|t| t.name == "memory"));
        let mut off = MemoryStore::new(home.clone());
        off.memory_enabled = false;
        off.user_profile_enabled = false;
        assert!(off.guidance_block().is_empty());
        assert!(off.memory_tool_definition().is_none());
        let mgr_off = MemoryManager::new(off);
        assert!(!mgr_off.system_block(&frozen).contains("MemoryGuidance"));
        assert!(mgr_off
            .all_tool_definitions()
            .iter()
            .all(|t| t.name != "memory" && t.name != "memory_search"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn failures_record_and_freeze_into_prompt() {
        let home = std::env::temp_dir().join(format!("rupi-mem-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let store = MemoryStore::new(home.clone());
        store
            .record_failure("rm -rf deleted worktree; use trash instead")
            .unwrap();
        let frozen = store.frozen_snapshot();
        assert!(frozen.failures.contains("trash"));
        assert!(frozen.system_block().contains("FailureMemory"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn memory_add_is_idempotent_across_review_rounds() {
        // 逐轮复盘把同一事实反复建议：add 必须幂等，空白差异也算重复；不同表述照存
        let home = std::env::temp_dir().join(format!("rupi-mem-dedup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let store = MemoryStore::new(home.clone());
        store.apply_write("add", "my editor is vim").unwrap();
        store.apply_write("add", "  my editor is vim  ").unwrap();
        store
            .apply_write("add", "my editor is vim with plugins")
            .unwrap();
        let content = std::fs::read_to_string(home.join("memories").join("MEMORY.md")).unwrap();
        assert_eq!(content.matches("my editor is vim").count(), 2, "{content}");
        assert_eq!(content.lines().filter(|l| !l.trim().is_empty()).count(), 2);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn failure_entries_dedup_across_rounds() {
        // 同一纠正反复出现只记一条（日期前缀不同也算重复）；不同教训照记
        let home = std::env::temp_dir().join(format!("rupi-mem-faildup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let store = MemoryStore::new(home.clone());
        store
            .record_failure("wrong directory; check pwd first")
            .unwrap();
        store
            .record_failure("wrong directory; check pwd first")
            .unwrap();
        store.record_failure("forgot to run tests").unwrap();
        let content = std::fs::read_to_string(home.join("memories").join("failures.md")).unwrap();
        assert_eq!(content.matches("wrong directory").count(), 1, "{content}");
        assert_eq!(content.lines().filter(|l| !l.trim().is_empty()).count(), 2);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn memory_writes_mirror_into_sqlite_and_searchable() {
        let home = std::env::temp_dir().join(format!("rupi-mem-mirror-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let store = MemoryStore::new(home.clone());
        store
            .apply_write("add", "prefers oolong tea over coffee")
            .unwrap();
        store
            .record_failure("used bash pipe wrong; check exit codes")
            .unwrap();
        let db = SessionStore::open(&home).unwrap();
        let hits = db.memory_search("oolong", 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "memory");
        let fhits = db.memory_search("exit codes", 5).unwrap();
        assert_eq!(fhits.len(), 1);
        assert_eq!(fhits[0].0, "failure");
        assert!(db.memory_search("zzz-no-match", 5).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn memory_search_ranks_best_match_first() {
        // bm25 排名：高频条目排前，limit=1 直接拿到最相关的（召回质量即记忆 harness 质量）
        let home = std::env::temp_dir().join(format!("rupi-mem-rank-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let db = SessionStore::open(&home).unwrap();
        db.mirror_memory_entry("memory", "likes tea").unwrap();
        db.mirror_memory_entry("memory", "tea tea tea tea ritual")
            .unwrap();
        let hits = db.memory_search("tea", 5).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits[0].1.contains("ritual"), "高频条目未排前: {:?}", hits);
        // 会话检索同享排名：limit=1 截到的是最相关的那条（标志词放句首，避开 snippet 尾截断）
        let sid = db.create_session("test").unwrap();
        db.add_message(&sid, "user", "mentions tea once").unwrap();
        db.add_message(&sid, "user", "bravo tea tea tea tea")
            .unwrap();
        let shits = db.search("tea", 1).unwrap();
        assert_eq!(shits.len(), 1);
        assert!(
            shits[0].1.contains("bravo"),
            "session limit=1 未截到最相关: {:?}",
            shits
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn memory_search_matches_chinese_substring() {
        // 中文按字面子串可召回（unicode61 把整句当一个 token，子串永远查不到）
        let home = std::env::temp_dir().join(format!("rupi-mem-cjk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let db = SessionStore::open(&home).unwrap();
        db.mirror_memory_entry("memory", "请记住我爱喝乌龙茶")
            .unwrap();
        let hits = db.memory_search("乌龙茶", 5).unwrap();
        assert_eq!(hits.len(), 1, "中文子串未召回: {hits:?}");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn legacy_unicode61_db_migrates_to_trigram() {
        // 手工造 unicode61 老库（user_version 0）：open 后应自动迁移且中文可查
        let home = std::env::temp_dir().join(format!("rupi-mem-legacy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let conn = rusqlite::Connection::open(home.join("sessions.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions(id TEXT PRIMARY KEY, profile TEXT, created_at TEXT, summary TEXT);
             CREATE TABLE messages(id TEXT PRIMARY KEY, session_id TEXT, role TEXT, content TEXT, created_at TEXT);
             CREATE VIRTUAL TABLE messages_fts USING fts5(content, content='messages', content_rowid='rowid');
             CREATE TABLE memories(id INTEGER PRIMARY KEY AUTOINCREMENT, target TEXT NOT NULL, content TEXT NOT NULL, created_at TEXT NOT NULL);
             CREATE VIRTUAL TABLE memory_fts USING fts5(content, content='memories', content_rowid='rowid');
             INSERT INTO memories(target, content, created_at) VALUES('memory', '请记住我爱喝乌龙茶', '2026-01-01');
             INSERT INTO memory_fts(rowid, content) SELECT rowid, content FROM memories;",
        )
        .unwrap();
        drop(conn);
        let db = SessionStore::open(&home).unwrap();
        let hits = db.memory_search("乌龙茶", 5).unwrap();
        assert_eq!(hits.len(), 1, "老库迁移后中文仍查不到: {hits:?}");
        let v: i64 = db
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        // v2 = trigram；v3 = messages.blocks；v4 = name/cwd/updated_at/parent_session
        assert_eq!(v, 4);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn manager_routes_memory_search_tool() {
        let home = std::env::temp_dir().join(format!("rupi-mem-ms-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let mgr = MemoryManager::new(MemoryStore::new(home.clone()));
        assert!(mgr
            .all_tool_definitions()
            .iter()
            .any(|d| d.name == "memory_search"));
        mgr.handle_tool_call(
            "memory",
            serde_json::json!({"op": "add", "entry": "deploy via gondolin sandbox"}),
        )
        .await
        .unwrap();
        let hit = mgr
            .handle_tool_call("memory_search", serde_json::json!({"query": "gondolin"}))
            .await
            .unwrap()
            .unwrap();
        assert!(hit.contains("gondolin"));
        let miss = mgr
            .handle_tool_call(
                "memory_search",
                serde_json::json!({"query": "zzz-no-match"}),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(miss, "no matching memories");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn project_tier_discovery_read_write() {
        let base = std::env::temp_dir().join(format!("rupi-mem-proj-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("demo");
        std::fs::create_dir_all(root.join("sub").join("deep")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        // 上溯发现
        let found = MemoryStore::discover_project(&root.join("sub").join("deep")).unwrap();
        assert_eq!(found, root);
        assert!(MemoryStore::discover_project(&base.join("orphan")).is_none());
        // 无项目根写 project 报错
        let home = base.join("home");
        let plain = MemoryStore::new(home.clone());
        assert!(plain.apply_write_scoped("project", "add", "x").is_err());
        // 有项目根：项目写入独立文件，全局不受影响
        let store = MemoryStore::new(home.clone()).with_project(root.clone());
        store
            .apply_write_scoped("project", "add", "uses pixi envs")
            .unwrap();
        assert!(root.join(".rupi").join("MEMORY.md").exists());
        assert!(!store.memory_text().is_empty());
        assert!(store.memory_text().contains("[project:demo]"));
        assert!(store.memory_text().contains("pixi"));
        // 全局文件里没有项目内容
        let global =
            std::fs::read_to_string(home.join("memories").join("MEMORY.md")).unwrap_or_default();
        assert!(!global.contains("pixi"));
        // 快照带项目分区
        assert!(store.frozen_snapshot().memory.contains("[project:demo]"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn manager_routes_project_scope_and_search() {
        let base = std::env::temp_dir().join(format!("rupi-mem-proj-mgr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("site");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let home = base.join("home");
        let mgr = MemoryManager::new(MemoryStore::new(home.clone()).with_project(root));
        mgr.handle_tool_call(
            "memory",
            serde_json::json!({"op": "add", "entry": "staging deploys on fridays", "scope": "project"}),
        )
        .await
        .unwrap();
        let hit = mgr
            .handle_tool_call("memory_search", serde_json::json!({"query": "fridays"}))
            .await
            .unwrap()
            .unwrap();
        assert!(hit.contains("[project]"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn write_path_compacts_oldest_lines_first() {
        let home = std::env::temp_dir().join(format!("rupi-mem-compact-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let mut store = MemoryStore::new(home.clone());
        store.memory_char_limit = 200;
        for i in 0..30 {
            store
                .apply_write("add", &format!("fact number {i}"))
                .unwrap();
        }
        // 磁盘文件有界，保最新、丢最旧，且按整行切（无半截行）
        let raw = std::fs::read_to_string(home.join("memories").join("MEMORY.md")).unwrap();
        assert!(raw.len() <= 200 + 128);
        assert!(raw.contains("fact number 29"));
        assert!(!raw.contains("fact number 0"));
        assert!(raw.lines().all(|l| !l.ends_with("fact num")));
        // 读路径同样整行
        assert!(store.memory_text().contains("fact number 29"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn session_store_roundtrip_with_fts() {
        let home = std::env::temp_dir().join(format!("rupi-mem-sess-b-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let store = SessionStore::open(&home).unwrap();
        let sid = store.create_session("default").unwrap();
        store
            .add_message(&sid, "user", "brewing oolong tea")
            .unwrap();
        store
            .add_message(&sid, "assistant", "enjoy your tea")
            .unwrap();
        // FTS 真能查到（触发器同步）
        let hits = store.search("oolong", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, sid);
        // 列表与明细
        let list = store.list_sessions(10).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].3, 2);
        store.set_name(&sid, "tea chat").unwrap();
        assert_eq!(store.get_name(&sid).unwrap().as_deref(), Some("tea chat"));
        assert_eq!(
            store.latest_session(None).unwrap().as_deref(),
            Some(sid.as_str())
        );
        let msgs = store.session_messages(&sid, 10).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].1, "user");
        // 行 id 非空（resume 时沿用为树节点 id）
        assert!(!msgs[0].0.is_empty());
        // 摘要写回与读回
        store.set_summary(&sid, "talked tea").unwrap();
        assert_eq!(store.get_summary(&sid).unwrap(), "talked tea");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn session_store_pragmas_reopen_and_recreate() {
        let home = std::env::temp_dir().join(format!(
            "rupi-mem-pragma-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&home);
        let db = SessionStore::open(&home).unwrap();
        let sync: i64 = db
            .conn
            .query_row("PRAGMA synchronous", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sync, 1, "synchronous should be NORMAL");
        let busy: i64 = db
            .conn
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .unwrap();
        assert_eq!(busy, 5000);
        let sid = db.create_session("p").unwrap();
        drop(db);
        // 同进程再开：user_version 已是最新，跳过 DDL；pragma 仍要落到新连接上
        let db2 = SessionStore::open(&home).unwrap();
        assert!(db2.has_session(&sid).unwrap());
        let sync2: i64 = db2
            .conn
            .query_row("PRAGMA synchronous", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sync2, 1);
        drop(db2);
        let _ = std::fs::remove_dir_all(&home);
        // 同路径删库再建：空库 user_version=0，必须重跑 DDL
        let db3 = SessionStore::open(&home).unwrap();
        assert!(db3.create_session("q").is_ok());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn persist_turn_batches_messages_in_one_call() {
        let home = std::env::temp_dir().join(format!("rupi-mem-batch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let store = SessionStore::open(&home).unwrap();
        let sid = store.create_session("default").unwrap();
        let n = store
            .persist_turn(
                &sid,
                &[
                    SessionMessageRow {
                        id: "u1".into(),
                        role: "user".into(),
                        content: "one".into(),
                        blocks: None,
                    },
                    SessionMessageRow {
                        id: "a1".into(),
                        role: "assistant".into(),
                        content: "two".into(),
                        blocks: None,
                    },
                ],
                Some("sum"),
            )
            .unwrap();
        assert_eq!(n, 2);
        assert_eq!(store.session_messages(&sid, 10).unwrap().len(), 2);
        assert_eq!(store.get_summary(&sid).unwrap(), "sum");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn memory_search_reuses_store_connection() {
        let home = std::env::temp_dir().join(format!("rupi-mem-reuse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let store = MemoryStore::new(home.clone());
        store.apply_write("add", "cached oolong fact").unwrap();
        assert!(
            store.sessions.lock().unwrap().is_some(),
            "mirror should cache sessions.db"
        );
        let mgr = MemoryManager::new(store.clone());
        let hit = mgr
            .handle_tool_call("memory_search", serde_json::json!({"query": "oolong"}))
            .await
            .unwrap()
            .unwrap();
        assert!(hit.contains("oolong"), "{hit}");
        {
            let g = store.sessions.lock().unwrap();
            let db = g.as_ref().expect("cached sessions.db");
            let sid = db.create_session("default").unwrap();
            db.add_message(&sid, "user", "session recall of oolong")
                .unwrap();
        }
        let sess = mgr
            .handle_tool_call("session_search", serde_json::json!({"query": "oolong"}))
            .await
            .unwrap()
            .unwrap();
        assert!(
            sess.contains("recall") || sess.contains("oolong") || sess.contains("oolo"),
            "{sess}"
        );
        assert!(
            store.sessions.lock().unwrap().is_some(),
            "session_search should reuse the cached sessions.db"
        );
        let _ = std::fs::remove_dir_all(&home);
    }
}

#[cfg(test)]
mod merge_tests {
    use super::*;

    #[test]
    fn core_tier_without_tags_injects_everything() {
        let (out, ext) = core_tier("- likes tea\n- uses zsh\n");
        assert_eq!(out, "- likes tea\n- uses zsh\n");
        assert_eq!(ext, 0);
    }

    #[test]
    fn core_tier_keeps_only_tagged_lines_and_counts_extended() {
        let (out, ext) = core_tier("- [core] prefers tabs\n- [CORE] repo uses cargo\n- some verbose note\n\n- another note\n");
        assert!(
            out.starts_with("- prefers tabs\n- repo uses cargo\n"),
            "{out}"
        );
        assert!(out.contains("2 extended entries not shown"), "{out}");
        assert_eq!(ext, 2);
        // 多字节字符不在字节边界上切（get(..6) 安全）
        let (out, _) = core_tier("- 中文条目\n- [core] 核心\n");
        assert!(out.starts_with("- 核心"), "{out}");
    }

    #[test]
    fn memory_text_applies_core_tier_and_grep_finds_extended() {
        let home =
            std::env::temp_dir().join(format!("rupi-core-tier-{}-{}", std::process::id(), line!()));
        let _ = std::fs::remove_dir_all(&home);
        let store = MemoryStore::new(home.clone());
        store
            .apply_write("add", "[core] always run cargo test")
            .unwrap();
        store
            .apply_write("add", "the deploy script lives in ops/deploy.sh")
            .unwrap();
        let text = store.memory_text();
        assert!(text.contains("always run cargo test"), "{text}");
        assert!(!text.contains("[core]"), "{text}");
        assert!(
            !text.contains("ops/deploy.sh"),
            "extended must stay out of prompt: {text}"
        );
        let hits = store.grep_memory_files("DEPLOY.sh", 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "memory");
        assert!(hits[0].1.contains("ops/deploy.sh"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn session_records_roundtrip_blocks_and_legacy_rows() {
        let home = std::env::temp_dir().join(format!(
            "rupi-sess-blocks-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&home);
        let db = SessionStore::open(&home).unwrap();
        let sid = db.create_session("t").unwrap();
        let mut asst = rupi_core::Message::text(rupi_core::Role::Assistant, "calling");
        asst.blocks.push(rupi_core::ContentBlock::ToolCall {
            id: "c1".into(),
            name: "read".into(),
            arguments: serde_json::json!({"path": "a"}),
        });
        let tool = rupi_core::Message {
            id: "m2".into(),
            role: rupi_core::Role::Tool,
            blocks: vec![rupi_core::ContentBlock::ToolResult {
                tool_call_id: "c1".into(),
                content: "file body".into(),
                is_error: false,
            }],
            provider: None,
            created_at: chrono::Utc::now(),
        };
        db.add_message_with_id("n0", &sid, "user", "legacy user row")
            .unwrap();
        db.add_message_full(
            "n1",
            &sid,
            "assistant",
            &asst.full_text(),
            serde_json::to_string(&asst).ok().as_deref(),
        )
        .unwrap();
        db.add_message_full(
            "n2",
            &sid,
            "tool",
            &tool.full_text(),
            serde_json::to_string(&tool).ok().as_deref(),
        )
        .unwrap();
        let recs = db.session_records(&sid, 10).unwrap();
        assert_eq!(recs.len(), 3);
        assert!(recs[0].blocks.is_none());
        assert_eq!(recs[0].to_message().role, rupi_core::Role::User);
        let m1 = recs[1].to_message();
        assert_eq!(m1.role, rupi_core::Role::Assistant);
        assert!(m1.blocks.iter().any(
            |b| matches!(b, rupi_core::ContentBlock::ToolCall { name, .. } if name == "read")
        ));
        let m2 = recs[2].to_message();
        assert_eq!(m2.role, rupi_core::Role::Tool);
        assert!(
            matches!(&m2.blocks[0], rupi_core::ContentBlock::ToolResult { content, .. } if content == "file body")
        );
        // 老接口仍可用且 FTS 能搜到工具结果文本
        assert_eq!(db.session_messages(&sid, 10).unwrap().len(), 3);
        assert!(!db.search("file body", 5).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn legacy_db_without_blocks_column_is_migrated() {
        let home =
            std::env::temp_dir().join(format!("rupi-migrate3-{}-{}", std::process::id(), line!()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        {
            let conn = rusqlite::Connection::open(home.join("sessions.db")).unwrap();
            conn.execute_batch(
                "CREATE TABLE sessions(id TEXT PRIMARY KEY, profile TEXT, created_at TEXT, summary TEXT);
                 CREATE TABLE messages(id TEXT PRIMARY KEY, session_id TEXT, role TEXT, content TEXT, created_at TEXT);
                 INSERT INTO sessions VALUES('s1','p','2026-01-01','');
                 INSERT INTO messages VALUES('m1','s1','user','old row','2026-01-01');
                 PRAGMA user_version = 2;",
            )
            .unwrap();
        }
        let db = SessionStore::open(&home).unwrap();
        let recs = db.session_records("s1", 10).unwrap();
        assert_eq!(recs.len(), 1);
        assert!(recs[0].blocks.is_none());
        db.add_message_full("m2", "s1", "assistant", "new", Some("{}"))
            .unwrap();
        let v: i64 = db
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 4);
        let _ = std::fs::remove_dir_all(&home);
    }
}
