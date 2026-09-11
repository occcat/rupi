//! 可嵌入 SDK（对标 Pi `createAgentSession`）。
//!
//! 宿主进程内持有 [`AgentSession`]，用 [`AgentSession::prompt`] 跑一轮，
//! 用 [`MessageInbox`] 在工具间隙注入转向或在整轮结束后跟进。
//! CLI `--mode rpc` 与 TUI 都走这一层，而不是再复制一套循环。

use crate::{AgentLoop, MessageInbox, QueueMode, QueuedMessage};
use rupi_core::{AgentEvent, CancelFlag, Extension, Message, SessionTree, StopReason};
use rupi_llm::{load_models, provider_or_mock, LlmProvider, ProviderOptions, ThinkingLevel};
use rupi_memory::{
    import_into_store, import_jsonl, persist_tree, remap_tree, resolve_session_ref, restore_tree,
    FrozenMemory, MemoryManager, MemoryStore, SessionStore,
};
use rupi_skills::SkillRegistry;
use rupi_tools::ToolRegistry;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// `createAgentSession` 等价入口：最小装配（内建工具 + 空记忆家目录）。
pub fn create_agent_session(provider: Arc<dyn LlmProvider>) -> AgentSession {
    AgentSession::builder().provider(provider).build()
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentSessionState {
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
    pub thinking_level: Option<ThinkingLevel>,
    pub is_streaming: bool,
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    pub message_count: usize,
    pub pending_message_count: usize,
    pub auto_compaction_enabled: bool,
}

pub struct AgentSessionBuilder {
    provider: Option<Arc<dyn LlmProvider>>,
    max_turns: u32,
    home: Option<PathBuf>,
    tools: Option<ToolRegistry>,
    skills: Option<SkillRegistry>,
    session: Option<SessionTree>,
    session_id: Option<String>,
    session_name: Option<String>,
    /// `--session` / RPC / `-r` 共用：id 或 JSONL 路径（见 [`rupi_memory::open_session`]）。
    session_path: Option<String>,
}

impl Default for AgentSessionBuilder {
    fn default() -> Self {
        Self {
            provider: None,
            max_turns: 20,
            home: None,
            tools: None,
            skills: None,
            session: None,
            session_id: None,
            session_name: None,
            session_path: None,
        }
    }
}

impl AgentSessionBuilder {
    pub fn provider(mut self, provider: Arc<dyn LlmProvider>) -> Self {
        self.provider = Some(provider);
        self
    }
    pub fn max_turns(mut self, n: u32) -> Self {
        self.max_turns = n;
        self
    }
    pub fn home(mut self, home: PathBuf) -> Self {
        self.home = Some(home);
        self
    }
    pub fn tools(mut self, tools: ToolRegistry) -> Self {
        self.tools = Some(tools);
        self
    }
    pub fn skills(mut self, skills: SkillRegistry) -> Self {
        self.skills = Some(skills);
        self
    }
    pub fn session(mut self, session: SessionTree, id: impl Into<String>) -> Self {
        self.session = Some(session);
        self.session_id = Some(id.into());
        self
    }
    pub fn session_name(mut self, name: impl Into<String>) -> Self {
        self.session_name = Some(name.into());
        self
    }

    /// 打开已有会话：SQLite id（或唯一前缀）或 Pi JSONL 路径。与 CLI `--session` 同一装入。
    pub fn session_path(mut self, spec: impl Into<String>) -> Self {
        self.session_path = Some(spec.into());
        self
    }

    pub fn build(self) -> AgentSession {
        self.try_build()
            .unwrap_or_else(|e| panic!("AgentSession::build failed: {e:#}"))
    }

    pub fn try_build(self) -> anyhow::Result<AgentSession> {
        let provider = self
            .provider
            .unwrap_or_else(|| Arc::new(rupi_llm::MockProvider::new(vec![])));
        let home = self.home.unwrap_or_else(|| {
            std::env::var("RUPI_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| std::env::temp_dir().join("rupi-embed"))
        });
        let _ = std::fs::create_dir_all(&home);
        let mut session = self.session;
        let mut session_id = self.session_id;
        let mut session_name = self.session_name;
        if let Some(spec) = &self.session_path {
            let db = rupi_memory::SessionStore::open(&home)?;
            let cwd = std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| ".".into());
            let opened = rupi_memory::open_session(&db, spec, &cwd)?;
            session = Some(opened.tree);
            session_id = Some(opened.id);
            if session_name.is_none() {
                session_name = opened.name;
            }
        }
        let store = MemoryStore::new(home);
        let frozen = store.frozen_snapshot();
        let mem = MemoryManager::new(store);
        let tools = self.tools.unwrap_or_else(ToolRegistry::with_builtins);
        let skills = self.skills.unwrap_or_default();
        let session = session.unwrap_or_default();
        let session_id = session_id.unwrap_or_else(|| session.id.clone());
        let inbox = Arc::new(MessageInbox::new());
        let mut agent = AgentLoop::new(self.max_turns);
        agent.inbox = Some(inbox.clone());
        Ok(AgentSession {
            provider,
            agent,
            session,
            tools,
            mem,
            frozen,
            skills,
            extensions: vec![],
            inbox,
            cancel: CancelFlag::new(),
            session_id,
            session_name,
            streaming: Arc::new(AtomicBool::new(false)),
            last_usage: Arc::new(Mutex::new(None)),
            sess_db: None,
            persist: false,
            command_dirs: Vec::new(),
            provider_opts: ProviderOptions::default(),
        })
    }
}

/// 进程内会话：`prompt` 驱动 [`AgentLoop`]，转向/跟进走共享信箱。
pub struct AgentSession {
    pub provider: Arc<dyn LlmProvider>,
    pub agent: AgentLoop,
    pub session: SessionTree,
    pub tools: ToolRegistry,
    pub mem: MemoryManager,
    pub frozen: FrozenMemory,
    pub skills: SkillRegistry,
    pub extensions: Vec<Arc<dyn Extension>>,
    pub inbox: Arc<MessageInbox>,
    pub cancel: CancelFlag,
    pub session_id: String,
    pub session_name: Option<String>,
    streaming: Arc<AtomicBool>,
    last_usage: Arc<Mutex<Option<(u64, u64)>>>,
    pub sess_db: Option<Arc<Mutex<SessionStore>>>,
    pub persist: bool,
    pub command_dirs: Vec<std::path::PathBuf>,
    pub provider_opts: ProviderOptions,
}

impl AgentSession {
    pub fn builder() -> AgentSessionBuilder {
        AgentSessionBuilder::default()
    }

    /// 从 CLI/TUI 已装配的部件包一层 SDK（保证 `agent.inbox` 与信箱同一把）。
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        provider: Arc<dyn LlmProvider>,
        mut agent: AgentLoop,
        session: SessionTree,
        tools: ToolRegistry,
        mem: MemoryManager,
        frozen: FrozenMemory,
        skills: SkillRegistry,
        session_id: String,
    ) -> Self {
        let inbox = agent
            .inbox
            .clone()
            .unwrap_or_else(|| Arc::new(MessageInbox::new()));
        agent.inbox = Some(inbox.clone());
        Self {
            provider,
            agent,
            session,
            tools,
            mem,
            frozen,
            skills,
            extensions: vec![],
            inbox,
            cancel: CancelFlag::new(),
            session_id,
            session_name: None,
            streaming: Arc::new(AtomicBool::new(false)),
            last_usage: Arc::new(Mutex::new(None)),
            sess_db: None,
            persist: false,
            command_dirs: Vec::new(),
            provider_opts: ProviderOptions::default(),
        }
    }

    pub fn is_streaming(&self) -> bool {
        self.streaming.load(Ordering::SeqCst)
    }

    pub fn last_usage(&self) -> Option<(u64, u64)> {
        *self.last_usage.lock().unwrap()
    }

    pub fn steer(&self, message: impl Into<String>) {
        self.inbox.steer(message);
    }

    pub fn follow_up(&self, message: impl Into<String>) {
        self.inbox.follow_up(message);
    }

    pub fn abort(&self) {
        self.cancel.cancel();
    }

    pub fn clear_queue(&self) -> (Vec<String>, Vec<String>) {
        self.inbox.clear()
    }

    pub fn set_steering_mode(&self, mode: QueueMode) {
        self.inbox.set_steering_mode(mode);
    }

    pub fn set_follow_up_mode(&self, mode: QueueMode) {
        self.inbox.set_follow_up_mode(mode);
    }

    pub fn state(&self) -> AgentSessionState {
        AgentSessionState {
            session_id: self.session_id.clone(),
            session_name: self.session_name.clone(),
            thinking_level: self.agent.thinking,
            is_streaming: self.is_streaming(),
            steering_mode: self.inbox.steering_mode(),
            follow_up_mode: self.inbox.follow_up_mode(),
            message_count: self.session.history().len(),
            pending_message_count: self.inbox.pending_count(),
            auto_compaction_enabled: self.agent.compaction_enabled,
        }
    }

    /// 发送一条用户消息并跑到停；整轮结束后按 `followUpMode` 继续跟进。
    pub async fn prompt(
        &mut self,
        message: &str,
        on_event: &(dyn Fn(AgentEvent) + Sync),
    ) -> anyhow::Result<StopReason> {
        self.prompt_queued(QueuedMessage::from_text(message), on_event)
            .await
    }

    pub async fn prompt_queued(
        &mut self,
        message: QueuedMessage,
        on_event: &(dyn Fn(AgentEvent) + Sync),
    ) -> anyhow::Result<StopReason> {
        self.cancel.reset();
        self.streaming.store(true, Ordering::SeqCst);
        let usage = self.last_usage.clone();
        let wrap = |e: AgentEvent| {
            if let AgentEvent::Usage {
                input_tokens,
                output_tokens,
            } = &e
            {
                *usage.lock().unwrap() = Some((*input_tokens, *output_tokens));
            }
            on_event(e);
        };
        let mut next = Some(message);
        let mut last = StopReason::Done;
        while let Some(q) = next.take() {
            last = self
                .agent
                .run_with_user(
                    &*self.provider,
                    &mut self.session,
                    q.to_user_message(),
                    &self.tools,
                    &self.mem,
                    &self.frozen,
                    &self.skills,
                    &self.extensions,
                    &wrap,
                    &self.cancel,
                )
                .await?;
            if matches!(last, StopReason::Aborted) {
                break;
            }
            let more = self.inbox.take_follow_up_msgs();
            if more.is_empty() {
                break;
            }
            next = Some(merge_queued(more));
        }
        self.streaming.store(false, Ordering::SeqCst);
        Ok(last)
    }

    pub fn messages(&self) -> Vec<Message> {
        self.session.history().into_iter().cloned().collect()
    }

    pub fn new_session(&mut self) {
        self.session = SessionTree::new();
        self.session_id = self.session.id.clone();
        self.inbox.clear();
        self.cancel.reset();
    }

    pub fn set_model(&mut self, spec: &str) -> serde_json::Value {
        let parsed = rupi_llm::parse_model_spec(spec);
        if let Some(t) = parsed.thinking {
            self.agent.thinking = Some(t);
        }
        let mut p = provider_or_mock(spec, &self.provider_opts);
        rupi_llm::apply_session_settings(&mut *p, Some(&self.session_id));
        self.provider = p.into();
        let provider = self.provider.name().to_string();
        let model = self
            .provider
            .model_id()
            .unwrap_or(&parsed.model)
            .to_string();
        rupi_tools::export_model(&provider, &model);
        if let Some(t) = self.agent.thinking {
            rupi_tools::export_reasoning_level(t.as_str());
        }
        serde_json::json!({
            "provider": provider,
            "id": model,
            "name": spec,
        })
    }

    pub fn available_models() -> serde_json::Value {
        let models: Vec<serde_json::Value> = load_models()
            .into_iter()
            .map(|m| {
                serde_json::json!({
                    "id": m.id,
                    "provider": m.provider,
                    "name": m.name,
                    "contextWindow": m.context,
                })
            })
            .collect();
        serde_json::json!({ "models": models })
    }

    pub fn switch_session(&mut self, dest: &str) -> anyhow::Result<serde_json::Value> {
        let dest = dest.trim();
        let path = std::path::Path::new(dest);
        if path.is_file() || dest.ends_with(".jsonl") {
            let raw = std::fs::read_to_string(path)?;
            if let Some(db) = &self.sess_db {
                let cwd = std::env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| ".".into());
                let (id, tree) = import_into_store(&db.lock().unwrap(), &raw, &cwd)?;
                self.adopt_session(id, tree);
                rupi_tools::export_session_file(dest);
            } else {
                let imported = import_jsonl(&raw)?;
                let id = imported.tree.id.clone();
                self.adopt_session(id, imported.tree);
                rupi_tools::export_session_file(dest);
            }
        } else {
            let db = self
                .sess_db
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("no session store"))?;
            let db = db.lock().unwrap();
            let id = resolve_session_ref(&db, dest)?;
            let tree = restore_tree(&db, &id)?;
            drop(db);
            self.adopt_session(id, tree);
        }
        Ok(serde_json::json!({
            "cancelled": false,
            "sessionId": self.session_id,
        }))
    }

    fn adopt_session(&mut self, id: String, tree: SessionTree) {
        self.session = tree;
        self.session.id = id.clone();
        self.session_id = id.clone();
        self.inbox.clear();
        rupi_tools::export_session_id(&id);
        let mut p = provider_or_mock(
            self.provider.model_id().unwrap_or("gpt-4o-mini"),
            &self.provider_opts,
        );
        rupi_llm::apply_session_settings(&mut *p, Some(&id));
        self.provider = p.into();
    }

    pub fn fork_session(&mut self, entry_id: Option<&str>) -> anyhow::Result<serde_json::Value> {
        if let Some(id) = entry_id {
            if !self.session.rewind_to(id) && !self.session.goto_node(id) {
                anyhow::bail!("unknown entryId: {id}");
            }
        }
        let text = self
            .session
            .history()
            .last()
            .map(|m| m.full_text())
            .unwrap_or_default();
        self.duplicate_session(true)?;
        Ok(serde_json::json!({
            "cancelled": false,
            "text": text,
            "sessionId": self.session_id,
        }))
    }

    pub fn clone_session(&mut self) -> anyhow::Result<serde_json::Value> {
        self.duplicate_session(false)?;
        Ok(serde_json::json!({
            "cancelled": false,
            "sessionId": self.session_id,
        }))
    }

    fn duplicate_session(&mut self, path_only: bool) -> anyhow::Result<()> {
        let new_tree = remap_tree(&self.session, path_only);
        if let (true, Some(db)) = (self.persist, &self.sess_db) {
            let cwd = std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| ".".into());
            let parent = self.session_id.clone();
            let db = db.lock().unwrap();
            let id = db.create_session_ex(
                if path_only { "fork" } else { "clone" },
                None,
                Some(&cwd),
                Some(&parent),
            )?;
            persist_tree(&db, &id, &new_tree)?;
            drop(db);
            self.adopt_session(id, new_tree);
        } else {
            let id = new_tree.id.clone();
            self.adopt_session(id, new_tree);
        }
        Ok(())
    }

    pub fn get_tree(&self) -> serde_json::Value {
        let entries: Vec<serde_json::Value> = self
            .session
            .tree_entries()
            .into_iter()
            .map(|e| {
                serde_json::json!({
                    "id": e.id,
                    "depth": e.depth,
                    "onPath": e.on_path,
                    "role": e.role,
                    "preview": e.preview,
                })
            })
            .collect();
        serde_json::json!({
            "sessionId": self.session_id,
            "tree": self.session.tree_view(),
            "entries": entries,
        })
    }

    pub fn set_session_name(&mut self, name: &str) -> anyhow::Result<()> {
        let name = name.trim();
        self.session_name = if name.is_empty() {
            None
        } else {
            Some(name.to_string())
        };
        if self.persist {
            if let Some(db) = &self.sess_db {
                db.lock().unwrap().set_name(&self.session_id, name)?;
            }
        }
        Ok(())
    }

    pub fn get_commands(&self) -> serde_json::Value {
        let mut commands = Vec::new();
        for (name, desc) in rupi_core::commands::list(&self.command_dirs) {
            commands.push(serde_json::json!({
                "name": name,
                "description": desc,
                "source": "prompt",
            }));
        }
        for (name, desc) in self.skills.command_entries() {
            commands.push(serde_json::json!({
                "name": format!("skill:{name}"),
                "description": desc,
                "source": "skill",
            }));
        }
        for ext in &self.extensions {
            for c in ext.commands() {
                commands.push(serde_json::json!({
                    "name": c.name,
                    "description": c.description,
                    "source": "extension",
                }));
            }
        }
        serde_json::json!({ "commands": commands })
    }
}

fn merge_queued(msgs: Vec<QueuedMessage>) -> QueuedMessage {
    let mut text = String::new();
    let mut images = Vec::new();
    for m in msgs {
        if !m.text.is_empty() {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&m.text);
        }
        images.extend(m.images);
    }
    QueuedMessage { text, images }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rupi_llm::MockProvider;

    #[tokio::test]
    async fn create_session_prompt_and_follow_up() {
        let provider = MockProvider::new(vec![
            MockProvider::text_response("first"),
            MockProvider::text_response("second"),
        ]);
        let mut sess = create_agent_session(Arc::new(provider));
        sess.follow_up("and then this");
        let reason = sess.prompt("hi", &|_| {}).await.unwrap();
        assert!(matches!(reason, StopReason::Done));
        let texts: Vec<String> = sess.messages().iter().map(|m| m.full_text()).collect();
        assert!(texts.iter().any(|t| t.contains("hi")));
        assert!(texts.iter().any(|t| t.contains("and then this")));
        assert!(texts.iter().any(|t| t.contains("first")));
        assert!(texts.iter().any(|t| t.contains("second")));
        assert!(!sess.state().is_streaming);
        assert_eq!(sess.state().pending_message_count, 0);
    }

    #[test]
    fn builder_session_path_loads_jsonl() {
        let home = std::env::temp_dir().join(format!(
            "rupi-sdk-sess-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let db = rupi_memory::SessionStore::open(&home).unwrap();
        let mut tree = SessionTree::new();
        tree.push(rupi_core::Message::text(
            rupi_core::Role::User,
            "from jsonl",
        ));
        let jsonl = rupi_memory::export_tree_jsonl(&tree, "/tmp/p", Some("sdk"), None);
        let path = home.join("s.jsonl");
        std::fs::write(&path, jsonl).unwrap();
        drop(db);
        let sess = AgentSession::builder()
            .home(home.clone())
            .session_path(path.to_string_lossy().as_ref())
            .try_build()
            .unwrap();
        assert!(sess
            .messages()
            .iter()
            .any(|m| m.full_text().contains("from jsonl")));
        assert_eq!(sess.session_name.as_deref(), Some("sdk"));
        let _ = std::fs::remove_dir_all(&home);
    }
}
