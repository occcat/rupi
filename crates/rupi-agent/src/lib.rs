//! rupi-agent: Agent 主循环（ReAct）+ Prompt 组装 + 扩展注册表。
//! 系统提示 = base + 记忆冻结块 + Skill 索引 + MCP promptSnippet 清单 + 扩展片段。

use rupi_core::{
    AgentEvent, ContentBlock, Extension, Message, Role, SessionTree, StopReason, ToolDefinition,
};
use rupi_llm::{ChatRequest, LlmProvider};
use rupi_memory::{FrozenMemory, MemoryManager};
use rupi_skills::SkillRegistry;
use rupi_tools::{Tool, ToolRegistry};
use std::sync::Arc;

pub mod review;
pub use review::{HeuristicReviewer, LlmReviewer, ReviewSuggestion, Reviewer, TurnTranscript};
pub mod policy;
pub use policy::{
    ApprovalAnswer, Approver, ChainPolicy, Decision, Policy, RulePolicy, SessionApprovalCache,
};
pub mod subagent;
pub use subagent::{run_subagents, SubagentResult, SubagentTask, SubagentTool, SUBAGENT_TOOL_NAME};
pub mod hooks;
pub use hooks::{
    DenyToolsHook, HookDecision, RecordedCall, RecordingHook, RedirectCommandHook, ToolHook,
};

#[derive(Clone)]
pub struct PromptBuilder {
    pub base: String,
}

impl PromptBuilder {
    pub fn new(base: impl Into<String>) -> Self {
        Self { base: base.into() }
    }

    pub fn build(
        &self,
        frozen: &FrozenMemory,
        mem: &MemoryManager,
        skills: &SkillRegistry,
        extra_tools: &[ToolDefinition],
        extensions: &[Arc<dyn Extension>],
    ) -> String {
        let mut s = self.base.clone();
        s.push_str(&mem.system_block(frozen));
        s.push_str(&skills.index_block());
        if !extra_tools.is_empty() {
            s.push_str("\n<AvailableTools>\n");
            for t in extra_tools {
                s.push_str(&t.prompt_line());
                s.push('\n');
            }
            s.push_str("</AvailableTools>\n");
        }
        for e in extensions {
            if let Some(sn) = e.system_prompt_snippet() {
                s.push('\n');
                s.push_str(&sn);
            }
        }
        s
    }
}

/// 工具执行策略：对标上游 `toolExecution: "parallel" | "sequential"`。
/// 默认串行（历史行为；审批问询顺序确定）。并行时“门”（before 钩子/策略/审批/计划模式）
/// 仍在第一阶段顺序执行，只并发第二阶段的真实执行；事件流与结果顺序保持原序。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolExecution {
    #[default]
    Sequential,
    Parallel,
}

/// 第一阶段产物：门已过完（或已拒绝），等第二阶段真实执行。
struct PendingCall {
    id: String,
    name: String,
    args: serde_json::Value,
    denied: Option<rupi_tools::ToolOutput>,
}

#[derive(Clone)]
pub struct AgentLoop {
    pub max_turns: u32,
    pub builder: PromptBuilder,
    /// 后台 review：主循环结束后安静复盘，提炼记忆/Skill 建议。默认关闭。
    pub reviewer: Option<Arc<dyn Reviewer>>,
    pub on_suggestion: Option<Arc<dyn Fn(ReviewSuggestion) + Send + Sync>>,
    /// 会话压缩：历史超 `compress_threshold_chars` 时摘要最旧部分，只把摘要 + 近期送模型。
    pub compress_threshold_chars: usize,
    pub compress_keep_last: usize,
    /// 权限门：默认全放行；计划模式/规则/审批按需装配。
    pub policy: Arc<dyn Policy>,
    pub approver: Option<Arc<dyn Approver>>,
    /// 计划模式：禁 write/edit/bash（只侦察、不动手），并在系统提示中声明。
    pub plan_mode: bool,
    /// 工具调用钩子（对标上游 beforeToolCall / afterToolCall）：
    /// before 在权限门之前（可改写参数/提前拒绝），after 包住一切结果（含拒绝路径）。
    pub hooks: Vec<Arc<dyn ToolHook>>,
    /// 工具执行策略，默认串行。
    pub tool_execution: ToolExecution,
}

impl AgentLoop {
    pub fn new(max_turns: u32) -> Self {
        Self {
            max_turns,
            builder: PromptBuilder::new(
                "You are rupi, a minimal coding agent (Rust port of Pi). Use tools to act. Be concise.",
            ),
            reviewer: None,
            on_suggestion: None,
            compress_threshold_chars: 60_000,
            compress_keep_last: 20,
            policy: Arc::new(policy::AllowAll),
            approver: None,
            plan_mode: false,
            hooks: vec![],
            tool_execution: ToolExecution::Sequential,
        }
    }

    pub fn with_reviewer(
        mut self,
        reviewer: Arc<dyn Reviewer>,
        on_suggestion: Arc<dyn Fn(ReviewSuggestion) + Send + Sync>,
    ) -> Self {
        self.reviewer = Some(reviewer);
        self.on_suggestion = Some(on_suggestion);
        self
    }

    pub fn with_compression(mut self, threshold_chars: usize, keep_last: usize) -> Self {
        self.compress_threshold_chars = threshold_chars;
        self.compress_keep_last = keep_last;
        self
    }

    pub fn with_policy(mut self, policy: Arc<dyn Policy>) -> Self {
        self.policy = policy;
        self
    }

    pub fn with_approver(mut self, approver: Arc<dyn Approver>) -> Self {
        self.approver = Some(approver);
        self
    }

    pub fn with_plan_mode(mut self, plan_mode: bool) -> Self {
        self.plan_mode = plan_mode;
        self
    }

    pub fn with_hook(mut self, hook: Arc<dyn ToolHook>) -> Self {
        self.hooks.push(hook);
        self
    }

    pub fn with_tool_execution(mut self, tool_execution: ToolExecution) -> Self {
        self.tool_execution = tool_execution;
        self
    }

    /// 第二阶段：真实执行一个放行的工具调用（memory 路由 + skill 内建 + 注册表）。
    /// 串行/并行共用；denied 短路由调用方处理，这里只管执行，错误一律转 tool error。
    async fn execute_allowed(
        tools: &ToolRegistry,
        mem: &MemoryManager,
        skills: &SkillRegistry,
        name: &str,
        args: serde_json::Value,
    ) -> rupi_tools::ToolOutput {
        if name == "load_skill" {
            let sk = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
            match skills.load_skill(sk) {
                Some(body) => rupi_tools::ToolOutput::ok(body),
                None => rupi_tools::ToolOutput::err(format!("unknown skill {sk}")),
            }
        } else if name == "read_resource" {
            let sk = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let rel = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            match skills.read_resource(sk, rel) {
                Ok(body) => {
                    // 资源文件可能很大：截断保窗口
                    let mut capped = body.chars().take(20_000).collect::<String>();
                    if body.chars().count() > 20_000 {
                        capped.push_str("\n...[truncated: file larger than 20k chars]");
                    }
                    rupi_tools::ToolOutput::ok(capped)
                }
                Err(e) => rupi_tools::ToolOutput::err(format!("read_resource failed: {e:#}")),
            }
        } else {
            // 工具执行错误一律转 tool error 回模型，主循环不中断
            // （与权限拒绝/未知工具同语义；此前 `?` 会直接 abort 整轮）
            match mem.handle_tool_call(&name, args.clone()).await {
                Ok(Some(routed)) => rupi_tools::ToolOutput::ok(routed),
                Ok(None) => tools
                    .execute(&name, args.clone())
                    .await
                    .unwrap_or_else(|e| {
                        rupi_tools::ToolOutput::err(format!("tool {name} failed: {e:#}"))
                    }),
                Err(e) => rupi_tools::ToolOutput::err(format!("memory tool {name} failed: {e:#}")),
            }
        }
    }

    fn is_mutating(tool: &str) -> bool {
        matches!(tool, "write" | "edit" | "bash")
    }

    /// 运行一轮用户请求直到 `done` / 无工具调用 / max_turns。每步推 `AgentEvent`。
    #[allow(clippy::too_many_arguments)]
    pub async fn run(
        &self,
        provider: &dyn LlmProvider,
        session: &mut SessionTree,
        user_input: &str,
        tools: &ToolRegistry,
        mem: &MemoryManager,
        frozen: &FrozenMemory,
        skills: &SkillRegistry,
        extensions: &[Arc<dyn Extension>],
        on_event: &(dyn Fn(AgentEvent) + Sync),
    ) -> anyhow::Result<StopReason> {
        session.push(Message::text(Role::User, user_input));
        // 长会话先压缩：摘要最旧部分（树不动，只影响 prompt 窗口）
        self.maybe_compress(provider, session, mem).await;
        // 记忆 + 外部 provider + skill 工具全部暴露给模型
        // （memory/recall 走 mem 路由执行，load_skill/read_resource 走 skills 路由执行）
        let mut all_tools = tools.definitions();
        all_tools.extend(mem.all_tool_definitions());
        all_tools.extend(skills.tool_definitions());
        // 记忆 prefetch：注入到本轮（不污染冻结快照）
        let recalled = mem.prefetch_all().await;
        let mut system = self
            .builder
            .build(frozen, mem, skills, &all_tools, extensions);
        if !recalled.is_empty() {
            system.push_str(&format!("\n<Recalled>\n{recalled}\n</Recalled>\n"));
        }
        if self.plan_mode {
            system.push_str("\n<PlanMode>\nYou are in PLAN MODE: explore with read-only tools, then describe the plan. Do NOT call write/edit/bash.\n</PlanMode>\n");
        }

        let mut tool_names: Vec<String> = vec![];
        for turn in 1..=self.max_turns {
            on_event(AgentEvent::TurnStart { turn });
            let history: Vec<Message> = session.prompt_history(self.compress_keep_last);
            let req = ChatRequest {
                system: system.clone(),
                messages: history,
                tools: all_tools.clone(),
                max_tokens: None,
                temperature: Some(0.2),
            };
            // 流式补全：delta 到达即推 TextDelta（TUI 逐字渲染），最终仍得完整响应
            let (tx, mut rx) = tokio::sync::mpsc::channel::<rupi_llm::StreamEvent>(64);
            let fut = provider.complete_streaming(req, tx);
            tokio::pin!(fut);
            let resp = loop {
                tokio::select! {
                    r = &mut fut => break r?,
                    msg = rx.recv() => match msg {
                        Some(rupi_llm::StreamEvent::TextDelta(delta)) => {
                            on_event(AgentEvent::TextDelta { delta });
                        }
                        // 发送端已关闭（provider 收尾中）：直接等完成，
                        // 否则关闭后的 recv 永远就绪空转，空烧 CPU。
                        None => break fut.await?,
                    },
                }
            };
            // select 竞速可能提前 break，排空残留 delta 保顺序完整
            while let Ok(rupi_llm::StreamEvent::TextDelta(delta)) = rx.try_recv() {
                on_event(AgentEvent::TextDelta { delta });
            }
            let has_calls = resp
                .message
                .blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolCall { .. }));
            session.push(resp.message.clone());

            if !has_calls {
                // 后台记忆 sync（fire-and-forget 语义：失败只 warning）
                mem.sync_all(user_input, &resp.message.full_text()).await;
                self.run_review(user_input, &resp.message.full_text(), &tool_names)
                    .await;
                on_event(AgentEvent::TurnEnd {
                    turn,
                    stop_reason: StopReason::Done,
                });
                for e in extensions {
                    e.on_event(&AgentEvent::TurnEnd {
                        turn,
                        stop_reason: StopReason::Done,
                    })
                    .await?;
                }
                return Ok(StopReason::Done);
            }

            // 依次执行工具调用（含 memory 路由 + skill load_skill 内建）
            let calls: Vec<(String, String, serde_json::Value)> = resp
                .message
                .blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolCall {
                        id,
                        name,
                        arguments,
                    } => Some((id.clone(), name.clone(), arguments.clone())),
                    _ => None,
                })
                .collect();
            let mut results = vec![];
            // 第一阶段（顺序）：before 钩子改写/拒绝 → ToolStart → 权限门/审批/计划模式。
            // 问询类动作保持原序；真实执行留到第二阶段（串行或并发），事件与结果保原序。
            let mut pending: Vec<PendingCall> = vec![];
            for (id, name, args) in calls {
                tool_names.push(name.clone());
                // before 钩子：可改写 args（后续钩子/策略门/执行看到新值）或提前拒绝
                let mut args = args;
                let mut hook_denied: Option<rupi_tools::ToolOutput> = None;
                for h in &self.hooks {
                    match h.before(&name, &args).await {
                        HookDecision::Proceed { args: Some(a) } => args = a,
                        HookDecision::Proceed { args: None } => {}
                        HookDecision::Deny { reason } => {
                            hook_denied = Some(rupi_tools::ToolOutput::err(format!(
                                "denied by hook: {reason}"
                            )));
                            break;
                        }
                    }
                }
                on_event(AgentEvent::ToolStart {
                    tool_call_id: id.clone(),
                    name: name.clone(),
                    arguments: args.clone(),
                });
                // 权限门：拒绝 / 无审批的 Ask 一律转 tool error 回模型，主循环不中断
                let denied: Option<rupi_tools::ToolOutput> =
                    hook_denied.or(match self.policy.decide(&name, &args) {
                        Decision::Allow => None,
                        Decision::Deny(reason) => Some(rupi_tools::ToolOutput::err(format!(
                            "denied by policy: {reason}"
                        ))),
                        Decision::Ask(reason) => {
                            let ok = match &self.approver {
                                Some(a) => a.approve(&name, &args, &reason),
                                None => false,
                            };
                            if ok {
                                None
                            } else {
                                Some(rupi_tools::ToolOutput::err(format!(
                                    "denied (approval required): {reason}"
                                )))
                            }
                        }
                    });
                // 计划模式兜底：即使策略放行，变更类工具也不执行
                let denied = denied.or_else(|| {
                    if self.plan_mode && Self::is_mutating(&name) {
                        Some(rupi_tools::ToolOutput::err(format!(
                            "plan mode: {name} is disabled; describe the plan instead"
                        )))
                    } else {
                        None
                    }
                });
                pending.push(PendingCall {
                    id,
                    name,
                    args,
                    denied,
                });
            }
            // 第二阶段：真实执行（串行逐个 await；并行 join_all 并发，结果保原序）
            let executed: Vec<rupi_tools::ToolOutput> = match self.tool_execution {
                ToolExecution::Sequential => {
                    let mut outs = Vec::with_capacity(pending.len());
                    for p in &pending {
                        outs.push(match &p.denied {
                            Some(o) => o.clone(),
                            None => {
                                Self::execute_allowed(tools, mem, skills, &p.name, p.args.clone())
                                    .await
                            }
                        });
                    }
                    outs
                }
                ToolExecution::Parallel => {
                    futures::future::join_all(pending.iter().map(|p| async {
                        match &p.denied {
                            Some(o) => o.clone(),
                            None => {
                                Self::execute_allowed(tools, mem, skills, &p.name, p.args.clone())
                                    .await
                            }
                        }
                    }))
                    .await
                }
            };
            // 第三阶段（顺序）：after 钩子 → ToolEnd → 结果入历史，保持原序
            for (p, mut out) in pending.into_iter().zip(executed) {
                // after 钩子：观察/改写结果（含拒绝路径），再回模型
                for h in &self.hooks {
                    out = h.after(&p.name, &p.args, out).await;
                }
                on_event(AgentEvent::ToolEnd {
                    tool_call_id: p.id.clone(),
                    name: p.name.clone(),
                    content: out.content.clone(),
                    is_error: out.is_error,
                });
                results.push(ContentBlock::ToolResult {
                    tool_call_id: p.id,
                    content: out.content,
                    is_error: out.is_error,
                });
            }
            session.push(Message {
                id: uuid::Uuid::new_v4().to_string(),
                role: Role::Tool,
                blocks: results,
                provider: None,
                created_at: chrono::Utc::now(),
            });
            on_event(AgentEvent::TurnEnd {
                turn,
                stop_reason: StopReason::Done,
            });
        }
        Ok(StopReason::MaxTurns)
    }

    /// 会话压缩：历史超阈值时，用 provider 把最旧部分摘要掉；失败则启发式兜底。永不抛错。
    pub async fn maybe_compress(
        &self,
        provider: &dyn LlmProvider,
        session: &mut SessionTree,
        mem: &MemoryManager,
    ) {
        if session.history_chars() <= self.compress_threshold_chars {
            return;
        }
        // 已压缩过且新增不足一窗：跳过，避免每轮重复烧模型
        // tail = 上次压缩点之后未压缩的消息数；首轮压缩后 tail == keep，
        // 新增 new_count 条后 tail == keep + new_count；new_count <= keep 时跳过。
        if let Some(through) = &session.summary_through {
            if session.summary.is_some() {
                let pos = session
                    .current_path
                    .iter()
                    .position(|id| id == through)
                    .map(|i| i + 1)
                    .unwrap_or(0);
                if session.current_path.len().saturating_sub(pos) <= self.compress_keep_last * 2 {
                    return;
                }
            }
        }
        let total = session.current_path.len();
        if total <= self.compress_keep_last {
            return;
        }
        let cut = total - self.compress_keep_last;
        let chunk: Vec<String> = session.history()[..cut]
            .iter()
            .map(|m| m.full_text())
            .collect();
        let through = session.current_path[cut - 1].clone();
        // 先给外部记忆落盘/收尾机会（Hermes on_pre_compress）
        mem.pre_compress_all().await;

        let mut input = String::new();
        if let Some(old) = &session.summary {
            input.push_str(&format!("Previous summary:\n{old}\n\nNew messages:\n"));
        }
        input.push_str(&chunk.join("\n---\n"));
        let req = ChatRequest {
            system: "Summarize this conversation prefix concisely. Keep durable facts, decisions, and open loops. Be brief.".into(),
            messages: vec![Message::text(Role::User, input)],
            tools: vec![],
            max_tokens: None,
            temperature: Some(0.0),
        };
        let summary = match provider.complete(req).await {
            Ok(r) => r.message.full_text(),
            Err(e) => {
                tracing::warn!("summarization failed, heuristic fallback: {e:#}");
                // 兜底：每条取首行拼接，保证窗口一定能缩小
                chunk
                    .iter()
                    .take(10)
                    .map(|t| {
                        t.lines()
                            .next()
                            .unwrap_or("")
                            .chars()
                            .take(120)
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        };
        session.set_summary(summary, through);
        tracing::info!(
            "session compressed: {total} msgs, kept last {}",
            self.compress_keep_last
        );
    }

    /// 后台 review：主流程结束后安静复盘，非空建议推给 `on_suggestion`。永不抛错。
    async fn run_review(&self, user: &str, assistant: &str, tool_names: &[String]) {
        let (Some(reviewer), Some(cb)) = (&self.reviewer, &self.on_suggestion) else {
            return;
        };
        let t = TurnTranscript {
            user: user.to_string(),
            assistant: assistant.to_string(),
            tool_names: tool_names.to_vec(),
        };
        let s = review::review_with_timeout(reviewer, &t, std::time::Duration::from_secs(5)).await;
        if !s.is_empty() {
            cb(s);
        }
    }
}

/// 把任意 `Tool` 适配成 `Extension`（Pi 的 registerTool 语义）。
pub struct ToolExtension {
    name: String,
    tool: Arc<dyn Tool>,
}

impl ToolExtension {
    pub fn new(name: &str, tool: Arc<dyn Tool>) -> Self {
        Self {
            name: name.into(),
            tool,
        }
    }
}

#[async_trait::async_trait]
impl Extension for ToolExtension {
    fn name(&self) -> &str {
        &self.name
    }
    fn tools(&self) -> Vec<ToolDefinition> {
        vec![self.tool.definition()]
    }
    fn system_prompt_snippet(&self) -> Option<String> {
        Some(self.tool.definition().prompt_line())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rupi_llm::{ChatResponse, MockProvider};
    use rupi_memory::MemoryStore;

    #[tokio::test]
    async fn loop_finishes_without_tool_calls() {
        let provider = MockProvider::new(vec![MockProvider::text_response("hello")]);
        let agent = AgentLoop::new(3);
        let mut session = SessionTree::new();
        let tools = ToolRegistry::with_builtins();
        let home = std::env::temp_dir().join("rupi-agent-test");
        let mem = MemoryManager::new(MemoryStore::new(home));
        let frozen = FrozenMemory::default();
        let skills = SkillRegistry::default();
        let events = std::sync::Mutex::new(vec![]);
        let reason = agent
            .run(
                &provider,
                &mut session,
                "hi",
                &tools,
                &mem,
                &frozen,
                &skills,
                &[],
                &|e| {
                    events.lock().unwrap().push(format!("{e:?}"));
                },
            )
            .await
            .unwrap();
        assert!(matches!(reason, StopReason::Done));
        assert!(session.history().len() >= 2);
    }

    #[tokio::test]
    async fn compress_summarizes_prefix_and_shrinks_window() {
        // 剧本：首个 complete 是压缩摘要，第二个是正常回合答复
        let provider = MockProvider::new(vec![
            MockProvider::text_response("SUMMARY: talked about tea"),
            MockProvider::text_response("hello"),
        ]);
        let agent = AgentLoop::new(3).with_compression(50, 2);
        let mut session = SessionTree::new();
        for i in 0..6 {
            session.push(Message::text(
                Role::User,
                format!("long message number {i} with padding xxxxxxxxxx"),
            ));
        }
        let tools = ToolRegistry::with_builtins();
        let home = std::env::temp_dir().join("rupi-agent-compress");
        let mem = MemoryManager::new(MemoryStore::new(home));
        let frozen = FrozenMemory::default();
        let skills = SkillRegistry::default();
        agent
            .run(
                &provider,
                &mut session,
                "hi",
                &tools,
                &mem,
                &frozen,
                &skills,
                &[],
                &|_| {},
            )
            .await
            .unwrap();
        let summary = session.summary.as_ref().expect("summary set");
        assert!(summary.contains("SUMMARY"));
        // 树全量保留，prompt 窗口缩小
        assert!(session.history().len() >= 8);
        assert!(session.prompt_history(2).len() <= 4);
    }

    #[tokio::test]
    async fn compress_skips_when_little_new_content() {
        let agent = AgentLoop::new(3).with_compression(50, 2);
        let mut session = SessionTree::new();
        for i in 0..6 {
            session.push(Message::text(
                Role::User,
                format!("long message number {i} with padding xxxxxxxxxx"),
            ));
        }
        let home = std::env::temp_dir().join("rupi-agent-compress-skip");
        let mem = MemoryManager::new(MemoryStore::new(home));
        // 首轮压出 SUMMARY-1；次轮纵使 provider 给出 SUMMARY-2 也不应采用
        let p1 = MockProvider::new(vec![MockProvider::text_response("SUMMARY-1")]);
        agent.maybe_compress(&p1, &mut session, &mem).await;
        assert!(session.summary.as_ref().unwrap().contains("SUMMARY-1"));
        session.push(Message::text(Role::User, "tiny"));
        let p2 = MockProvider::new(vec![MockProvider::text_response("SUMMARY-2")]);
        agent.maybe_compress(&p2, &mut session, &mem).await;
        assert!(session.summary.as_ref().unwrap().contains("SUMMARY-1"));
    }

    #[tokio::test]
    async fn compress_falls_back_when_provider_fails() {
        struct Fail;
        #[async_trait::async_trait]
        impl rupi_llm::LlmProvider for Fail {
            fn name(&self) -> &str {
                "fail"
            }
            async fn complete(&self, _req: ChatRequest) -> anyhow::Result<rupi_llm::ChatResponse> {
                anyhow::bail!("down")
            }
        }
        let agent = AgentLoop::new(3).with_compression(10, 1);
        let mut session = SessionTree::new();
        for i in 0..4 {
            session.push(Message::text(
                Role::User,
                format!("message {i} padding yyyyy"),
            ));
        }
        let home = std::env::temp_dir().join("rupi-agent-compress-fb");
        let mem = MemoryManager::new(MemoryStore::new(home));
        agent.maybe_compress(&Fail, &mut session, &mem).await;
        // 兜底摘要照样落盘，窗口照样缩小
        assert!(session.summary.is_some());
        assert_eq!(session.prompt_history(1).len(), 2);
    }

    #[tokio::test]
    async fn plan_mode_blocks_write_without_executing() {
        use rupi_core::ContentBlock;
        let script = vec![
            ChatResponse {
                message: Message {
                    id: "a".into(),
                    role: Role::Assistant,
                    blocks: vec![ContentBlock::ToolCall {
                        id: "c1".into(),
                        name: "write".into(),
                        arguments: serde_json::json!({"path": "/tmp/rupi-plan-guard.txt", "content": "x"}),
                    }],
                    provider: None,
                    created_at: chrono::Utc::now(),
                },
                stop_reason: "tool_calls".into(),
            },
            MockProvider::text_response("planned"),
        ];
        let provider = MockProvider::new(script);
        let agent = AgentLoop::new(5).with_plan_mode(true);
        let mut session = SessionTree::new();
        let tools = ToolRegistry::with_builtins();
        let home = std::env::temp_dir().join("rupi-agent-plan");
        let mem = MemoryManager::new(MemoryStore::new(home));
        agent
            .run(
                &provider,
                &mut session,
                "create file",
                &tools,
                &mem,
                &FrozenMemory::default(),
                &SkillRegistry::default(),
                &[],
                &|_| {},
            )
            .await
            .unwrap();
        assert!(!std::path::Path::new("/tmp/rupi-plan-guard.txt").exists());
        let all: String = session
            .history()
            .iter()
            .map(|m| m.full_text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all.contains("plan mode"));
    }

    #[tokio::test]
    async fn skill_tools_visible_and_executable_end_to_end() {
        use rupi_core::ContentBlock;
        // 磁盘 skill：全文 + references 资源
        let base =
            std::env::temp_dir().join(format!("rupi-agent-skilltools-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let dir = base.join("ops");
        std::fs::create_dir_all(dir.join("references")).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: ops-skill\ndescription: ops runbook\n---\n\n# Ops\nFollow runbook.\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("references").join("runbook.md"),
            "RUNBOOK-SECRET-SAUCE",
        )
        .unwrap();
        let skills = SkillRegistry::discover(&[base.clone()]);
        // 模型能看见两个工具 schema
        let defs = skills.tool_definitions();
        assert!(defs.iter().any(|d| d.name == "load_skill"));
        assert!(defs.iter().any(|d| d.name == "read_resource"));
        // 剧本：先 load_skill 全文，再 read_resource 读 references，最后文本收尾
        let mk_call = |name: &str, args: serde_json::Value| ChatResponse {
            message: Message {
                id: "a".into(),
                role: Role::Assistant,
                blocks: vec![ContentBlock::ToolCall {
                    id: "c1".into(),
                    name: name.into(),
                    arguments: args,
                }],
                provider: None,
                created_at: chrono::Utc::now(),
            },
            stop_reason: "tool_calls".into(),
        };
        let provider = MockProvider::new(vec![
            mk_call("load_skill", serde_json::json!({"name": "ops-skill"})),
            mk_call(
                "read_resource",
                serde_json::json!({"name": "ops-skill", "path": "references/runbook.md"}),
            ),
            MockProvider::text_response("used the skill"),
        ]);
        let agent = AgentLoop::new(5);
        let mut session = SessionTree::new();
        let tools = ToolRegistry::with_builtins();
        let home = std::env::temp_dir().join("rupi-agent-skilltools-mem");
        let mem = MemoryManager::new(MemoryStore::new(home));
        agent
            .run(
                &provider,
                &mut session,
                "do ops",
                &tools,
                &mem,
                &FrozenMemory::default(),
                &skills,
                &[],
                &|_| {},
            )
            .await
            .unwrap();
        let all: String = session
            .history()
            .iter()
            .map(|m| m.full_text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all.contains("Follow runbook"));
        assert!(all.contains("RUNBOOK-SECRET-SAUCE"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn ask_policy_with_approver_gates_execution() {
        use rupi_core::ContentBlock;
        struct Yes;
        impl crate::Approver for Yes {
            fn approve(&self, _t: &str, _a: &serde_json::Value, _r: &str) -> bool {
                true
            }
        }
        let mk_call = || ChatResponse {
            message: Message {
                id: "a".into(),
                role: Role::Assistant,
                blocks: vec![ContentBlock::ToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command": "echo approved"}),
                }],
                provider: None,
                created_at: chrono::Utc::now(),
            },
            stop_reason: "tool_calls".into(),
        };
        let ask = RulePolicy {
            ask_tools: vec!["bash".into()],
            ..Default::default()
        };
        // 无审批器 → 拒绝
        let agent = AgentLoop::new(5).with_policy(Arc::new(ask.clone()));
        let mut session = SessionTree::new();
        let tools = ToolRegistry::with_builtins();
        let home = std::env::temp_dir().join("rupi-agent-ask");
        let mem = MemoryManager::new(MemoryStore::new(home));
        agent
            .run(
                &MockProvider::new(vec![mk_call(), MockProvider::text_response("d")]),
                &mut session,
                "go",
                &tools,
                &mem,
                &FrozenMemory::default(),
                &SkillRegistry::default(),
                &[],
                &|_| {},
            )
            .await
            .unwrap();
        let all: String = session
            .history()
            .iter()
            .map(|m| m.full_text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all.contains("approval required"));
        // 有审批器放行 → 真执行
        let agent = AgentLoop::new(5)
            .with_policy(Arc::new(ask))
            .with_approver(Arc::new(Yes));
        let mut session = SessionTree::new();
        agent
            .run(
                &MockProvider::new(vec![mk_call(), MockProvider::text_response("d")]),
                &mut session,
                "go",
                &tools,
                &mem,
                &FrozenMemory::default(),
                &SkillRegistry::default(),
                &[],
                &|_| {},
            )
            .await
            .unwrap();
        let all: String = session
            .history()
            .iter()
            .map(|m| m.full_text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all.contains("approved"));
    }

    #[tokio::test]
    async fn hook_deny_short_circuits_before_execution() {
        let mk_call = || ChatResponse {
            message: Message {
                id: "a".into(),
                role: Role::Assistant,
                blocks: vec![ContentBlock::ToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command": "echo should-not-run"}),
                }],
                provider: None,
                created_at: chrono::Utc::now(),
            },
            stop_reason: "tool_calls".into(),
        };
        let rec = Arc::new(RecordingHook::default());
        let agent = AgentLoop::new(5)
            .with_hook(Arc::new(DenyToolsHook::new(["bash"])))
            .with_hook(rec.clone());
        let mut session = SessionTree::new();
        let tools = ToolRegistry::with_builtins();
        let home = std::env::temp_dir().join("rupi-agent-hook-deny");
        let mem = MemoryManager::new(MemoryStore::new(home));
        agent
            .run(
                &MockProvider::new(vec![mk_call(), MockProvider::text_response("d")]),
                &mut session,
                "go",
                &tools,
                &mem,
                &FrozenMemory::default(),
                &SkillRegistry::default(),
                &[],
                &|_| {},
            )
            .await
            .unwrap();
        let all: String = session
            .history()
            .iter()
            .map(|m| m.full_text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all.contains("denied by hook"));
        // 精确证明未执行：Tool 角色消息里只有拒绝结果，没有真实回包
        let tool_results: Vec<_> = session
            .history()
            .iter()
            .filter(|m| m.role == Role::Tool)
            .flat_map(|m| m.blocks.iter())
            .collect();
        assert!(!tool_results.is_empty());
        for b in tool_results {
            match b {
                ContentBlock::ToolResult {
                    content, is_error, ..
                } => {
                    assert!(*is_error);
                    assert!(content.contains("denied by hook"));
                }
                _ => {}
            }
        }
        // ToolCall 参数原文仍在历史里（模型发出的请求），不作否定断言。
        // after 钩子同样包住拒绝路径
        let calls = rec.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "bash");
        assert!(calls[0].is_error);
    }

    #[tokio::test]
    async fn hook_rewrite_applies_before_execution() {
        let mk_call = || ChatResponse {
            message: Message {
                id: "a".into(),
                role: Role::Assistant,
                blocks: vec![ContentBlock::ToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command": "original-cmd"}),
                }],
                provider: None,
                created_at: chrono::Utc::now(),
            },
            stop_reason: "tool_calls".into(),
        };
        let agent = AgentLoop::new(5).with_hook(Arc::new(RedirectCommandHook::new(vec![(
            "original-cmd",
            "echo rewritten-ok",
        )])));
        let mut session = SessionTree::new();
        let tools = ToolRegistry::with_builtins();
        let home = std::env::temp_dir().join("rupi-agent-hook-rewrite");
        let mem = MemoryManager::new(MemoryStore::new(home));
        agent
            .run(
                &MockProvider::new(vec![mk_call(), MockProvider::text_response("d")]),
                &mut session,
                "go",
                &tools,
                &mem,
                &FrozenMemory::default(),
                &SkillRegistry::default(),
                &[],
                &|_| {},
            )
            .await
            .unwrap();
        let all: String = session
            .history()
            .iter()
            .map(|m| m.full_text())
            .collect::<Vec<_>>()
            .join("\n");
        // 未改写会是 exit 127；改写后真实执行 echo
        assert!(all.contains("rewritten-ok"));
        assert!(!all.contains("exit 127"));
    }

    #[test]
    fn default_tool_execution_is_sequential() {
        assert_eq!(AgentLoop::new(3).tool_execution, ToolExecution::Sequential);
    }

    fn two_bash_calls(cmd1: &str, cmd2: &str) -> ChatResponse {
        ChatResponse {
            message: Message {
                id: "a".into(),
                role: Role::Assistant,
                blocks: vec![
                    ContentBlock::ToolCall {
                        id: "c1".into(),
                        name: "bash".into(),
                        arguments: serde_json::json!({"command": cmd1}),
                    },
                    ContentBlock::ToolCall {
                        id: "c2".into(),
                        name: "bash".into(),
                        arguments: serde_json::json!({"command": cmd2}),
                    },
                ],
                provider: None,
                created_at: chrono::Utc::now(),
            },
            stop_reason: "tool_calls".into(),
        }
    }

    fn tool_result_texts(session: &SessionTree) -> Vec<String> {
        session
            .history()
            .iter()
            .filter(|m| m.role == Role::Tool)
            .flat_map(|m| m.blocks.iter())
            .filter_map(|b| match b {
                ContentBlock::ToolResult { content, .. } => Some(content.trim().to_owned()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn parallel_preserves_result_order() {
        use rupi_core::ContentBlock;
        let agent = AgentLoop::new(5).with_tool_execution(ToolExecution::Parallel);
        let mut session = SessionTree::new();
        let tools = ToolRegistry::with_builtins();
        let home = std::env::temp_dir().join("rupi-agent-par-order");
        let mem = MemoryManager::new(MemoryStore::new(home));
        agent
            .run(
                &MockProvider::new(vec![
                    two_bash_calls("echo par-a", "echo par-b"),
                    MockProvider::text_response("d"),
                ]),
                &mut session,
                "go",
                &tools,
                &mem,
                &FrozenMemory::default(),
                &SkillRegistry::default(),
                &[],
                &|_| {},
            )
            .await
            .unwrap();
        assert_eq!(tool_result_texts(&session), vec!["par-a", "par-b"]);
    }

    #[tokio::test]
    async fn parallel_runs_concurrently() {
        let agent = AgentLoop::new(5).with_tool_execution(ToolExecution::Parallel);
        let mut session = SessionTree::new();
        let tools = ToolRegistry::with_builtins();
        let home = std::env::temp_dir().join("rupi-agent-par-time");
        let mem = MemoryManager::new(MemoryStore::new(home));
        let t = std::time::Instant::now();
        agent
            .run(
                &MockProvider::new(vec![
                    two_bash_calls("sleep 2", "sleep 2"),
                    MockProvider::text_response("d"),
                ]),
                &mut session,
                "go",
                &tools,
                &mem,
                &FrozenMemory::default(),
                &SkillRegistry::default(),
                &[],
                &|_| {},
            )
            .await
            .unwrap();
        // 串行至少 4s；并行约 2s，3.5s 上限留足余量
        assert!(
            t.elapsed() < std::time::Duration::from_millis(3500),
            "parallel took {:?}",
            t.elapsed()
        );
        assert_eq!(tool_result_texts(&session).len(), 2);
    }
}
