//! rupi-memory: Hermes 风格分层记忆。
//! - 内建层：`MEMORY.md` / `USER.md`，session 启动时冻结快照注入系统提示（保 prefix cache），
//!   会话内写盘即时生效但快照不变，下个 session 才可见。
//! - 外部层：`MemoryProvider` trait（7 方法生命周期），`MemoryManager` 编排且只允许一个外部 provider。
//! - 会话层：SQLite 会话/消息库 + FTS5 + session_search；后台 review 钩子沉淀记忆与 Skill。

use async_trait::async_trait;
use rupi_core::ToolDefinition;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const MEMORY_FILE: &str = "MEMORY.md";
pub const USER_FILE: &str = "USER.md";

/// 内建记忆文件：受 char limit 保护（默认 ~800 tokens / ~500 tokens），超限截断保尾部。
#[derive(Debug, Clone)]
pub struct MemoryStore {
    pub home: PathBuf,
    pub memory_enabled: bool,
    pub user_profile_enabled: bool,
    pub memory_char_limit: usize,
    pub user_char_limit: usize,
}

impl MemoryStore {
    pub fn new(home: PathBuf) -> Self {
        Self {
            home,
            memory_enabled: true,
            user_profile_enabled: true,
            memory_char_limit: 2200,
            user_char_limit: 1375,
        }
    }

    fn memories_dir(&self) -> PathBuf {
        self.home.join("memories")
    }

    fn read_limited(&self, path: &Path, limit: usize) -> String {
        let content = std::fs::read_to_string(path).unwrap_or_default();
        if content.len() <= limit {
            return content;
        }
        // 超限保留尾部（最新写入在尾部）
        let start = content.len() - limit;
        format!("...[truncated]\n{}", &content[start..])
    }

    pub fn memory_text(&self) -> String {
        if !self.memory_enabled {
            return String::new();
        }
        self.read_limited(
            &self.memories_dir().join(MEMORY_FILE),
            self.memory_char_limit,
        )
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
        }
    }

    /// agent 经 `memory` 工具写入：即时落盘，返回实时状态（但不改变已冻结快照）。
    pub fn apply_write(&self, op: &str, entry: &str) -> anyhow::Result<String> {
        std::fs::create_dir_all(self.memories_dir())?;
        let path = self.memories_dir().join(MEMORY_FILE);
        let mut content = std::fs::read_to_string(&path).unwrap_or_default();
        match op {
            "add" => {
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
        Ok(content)
    }

    pub fn memory_tool_definition(&self) -> Option<ToolDefinition> {
        if !self.memory_enabled && !self.user_profile_enabled {
            return None;
        }
        Some(ToolDefinition {
            name: "memory".into(),
            description: "Manage long-term memory (MEMORY.md): add/replace/remove entries".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "op": {"type": "string", "enum": ["add", "replace", "remove"]},
                    "entry": {"type": "string"}
                },
                "required": ["op", "entry"]
            }),
            prompt_snippet: Some("memory(op, entry): persist durable facts across sessions".into()),
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct FrozenMemory {
    pub memory: String,
    pub user: String,
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
        s
    }
}

/// 外部记忆 provider 契约：7 方法生命周期（Hermes `MemoryProvider` 对齐）。
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
}

impl MemoryManager {
    pub fn new(store: MemoryStore) -> Self {
        Self {
            store,
            external_name: None,
            external: None,
            tool_to_provider: HashMap::new(),
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
        }
        if let Some(e) = &self.external {
            out.extend(e.tool_schemas());
        }
        out
    }

    pub fn system_block(&self, frozen: &FrozenMemory) -> String {
        let mut s = frozen.system_block();
        if let Some(e) = &self.external {
            s.push_str(&e.system_prompt_block());
        }
        s
    }

    pub async fn prefetch_all(&self) -> String {
        match &self.external {
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
        }
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
            let op = args.get("op").and_then(|v| v.as_str()).unwrap_or("add");
            let entry = args.get("entry").and_then(|v| v.as_str()).unwrap_or("");
            let live = self.store.apply_write(op, entry)?;
            if let Some(e) = &self.external {
                let _ = e.on_memory_write(op, entry).await;
            }
            return Ok(Some(format!(
                "memory updated (live). Takes effect in prompt next session.\n{live}"
            )));
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
}

impl JsonlProvider {
    pub fn new(recent_n: usize) -> Self {
        Self {
            file: PathBuf::new(),
            recent: std::sync::Mutex::new(vec![]),
            recent_n,
        }
    }

    fn append_line(&self, line: &str) -> anyhow::Result<()> {
        use std::io::Write as _;
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
        // 必须立即返回：只读内存缓存，后台 sync 后刷新
        self.recent.lock().unwrap().join("\n")
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

impl SessionStore {
    pub fn open(home: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(home)?;
        let conn = rusqlite::Connection::open(home.join("sessions.db"))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS sessions(id TEXT PRIMARY KEY, profile TEXT, created_at TEXT, summary TEXT);
             CREATE TABLE IF NOT EXISTS messages(id TEXT PRIMARY KEY, session_id TEXT, role TEXT, content TEXT, created_at TEXT);
             CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(content, content='messages', content_rowid='rowid');",
        )?;
        Ok(Self { conn })
    }

    pub fn create_session(&self, profile: &str) -> anyhow::Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        self.conn.execute(
            "INSERT INTO sessions(id, profile, created_at, summary) VALUES(?,?,?,?)",
            rusqlite::params![id, profile, chrono::Utc::now().to_rfc3339(), String::new()],
        )?;
        Ok(id)
    }

    pub fn add_message(&self, session_id: &str, role: &str, content: &str) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT INTO messages(id, session_id, role, content, created_at) VALUES(?,?,?,?,?)",
            rusqlite::params![
                uuid::Uuid::new_v4().to_string(),
                session_id,
                role,
                content,
                chrono::Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    /// session_search：跨会话全文检索（FTS5），供 agent 回忆历史上下文。
    pub fn search(&self, query: &str, limit: usize) -> anyhow::Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id, snippet(messages_fts, 0, '<b>', '</b>', '...', 20) FROM messages_fts WHERE messages_fts MATCH ? LIMIT ?",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![query, limit as i64], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<Result<Vec<(String, String)>, _>>()?;
        Ok(rows)
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
}
