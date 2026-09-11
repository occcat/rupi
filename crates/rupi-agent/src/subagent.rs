//! 子任务分发：对标 Pi 生态的 sub-agents 扩展。
//!
//! 两层：
//! - [`run_subagents`]：把任务扇出到分叉会话上并发跑（`SessionTree::branch_from` 隔离上下文，
//!   修坏不污染主会话，完事只汇总结果），`join_all` 同 executor 并发，无 `'static` 约束。
//! - [`SubagentTool`]：模型可调用的 `subagent` 工具（单任务委托）；深度 guard 防无限递归。

use crate::AgentLoop;
use rupi_core::{AgentEvent, CancelFlag, SessionTree, StopReason};
use rupi_llm::LlmProvider;
use rupi_memory::{FrozenMemory, MemoryManager};
use rupi_skills::SkillRegistry;
use rupi_tools::ToolRegistry;
use std::sync::Arc;

pub const SUBAGENT_TOOL_NAME: &str = "subagent";

#[derive(Debug, Clone)]
pub struct SubagentTask {
    pub goal: String,
    pub max_turns: u32,
}

#[derive(Debug, Clone)]
pub struct SubagentResult {
    pub goal: String,
    pub summary: String,
    pub tool_calls: usize,
    pub stop_reason: StopReason,
}

/// 并发扇出：每个任务 fork 主会话游标（空会话则全新），跑完汇总。
/// 共享的 provider/tools/mem/skills 只读借用；各任务会话相互隔离。
/// `cancel` 透传给各子循环（父循环中止可打断扇出中的子任务）。
#[allow(clippy::too_many_arguments)]
pub async fn run_subagents(
    agent: &AgentLoop,
    provider: &dyn LlmProvider,
    base: &SessionTree,
    tasks: Vec<SubagentTask>,
    tools: &ToolRegistry,
    mem: &MemoryManager,
    frozen: &FrozenMemory,
    skills: &SkillRegistry,
    extensions: &[Arc<dyn crate::Extension>],
    on_event: &(dyn Fn(AgentEvent) + Sync),
    cancel: &CancelFlag,
) -> Vec<SubagentResult> {
    let tip = base.current_path.last().cloned();
    let futs = tasks.into_iter().map(|t| {
        let mut session = match &tip {
            Some(id) => base.branch_from(id).unwrap_or_else(SessionTree::new),
            None => SessionTree::new(),
        };
        let scoped = AgentLoop {
            max_turns: t.max_turns.min(agent.max_turns.max(1)),
            ..agent.clone()
        };
        async move {
            let before = session.history().len();
            let reason = scoped
                .run(
                    provider,
                    &mut session,
                    &t.goal,
                    tools,
                    mem,
                    frozen,
                    skills,
                    extensions,
                    on_event,
                    cancel,
                )
                .await
                .unwrap_or(StopReason::Aborted);
            let summary = session
                .history()
                .iter()
                .rev()
                .find(|m| m.role == rupi_core::Role::Assistant)
                .map(|m| m.full_text())
                .unwrap_or_default();
            let tool_calls = session.history().len().saturating_sub(before);
            SubagentResult {
                goal: t.goal,
                summary,
                tool_calls,
                stop_reason: reason,
            }
        }
    });
    futures::future::join_all(futs).await
}

/// 模型可调用的委托工具：自包含子任务（无父会话上下文），跑完只回摘要。
/// 非交互：无审批器，Ask 一律拒绝；递归深度达 `max_depth` 时子会话不再配 `subagent` 工具。
/// 父取消经 `execute_with_cancel` 直透内层循环（Esc/Ctrl-C 下一检查点停，不再跑到头）。
pub struct SubagentTool {
    provider: Arc<dyn LlmProvider>,
    tools: Arc<ToolRegistry>,
    mem: Arc<MemoryManager>,
    frozen: FrozenMemory,
    skills: Arc<SkillRegistry>,
    max_turns: u32,
    plan_mode: bool,
    /// 父会话思考强度快照（构造时传入；`run_subagents` 扇出走 agent clone 自动继承，
    /// 这里委托工具自建循环，需显式透传，否则子任务永远跑 provider 默认档）。
    thinking: Option<rupi_llm::ThinkingLevel>,
    depth: u8,
    max_depth: u8,
}

impl SubagentTool {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        tools: Arc<ToolRegistry>,
        mem: Arc<MemoryManager>,
        frozen: FrozenMemory,
        skills: Arc<SkillRegistry>,
        max_turns: u32,
    ) -> Self {
        Self {
            provider,
            tools,
            mem,
            frozen,
            skills,
            max_turns,
            plan_mode: false,
            thinking: None,
            depth: 0,
            max_depth: 2,
        }
    }

    pub fn with_depth(mut self, depth: u8, max_depth: u8) -> Self {
        self.depth = depth;
        self.max_depth = max_depth;
        self
    }

    pub fn with_plan_mode(mut self, plan_mode: bool) -> Self {
        self.plan_mode = plan_mode;
        self
    }

    pub fn with_thinking(mut self, thinking: Option<rupi_llm::ThinkingLevel>) -> Self {
        self.thinking = thinking;
        self
    }
}

#[async_trait::async_trait]
impl rupi_tools::Tool for SubagentTool {
    fn definition(&self) -> rupi_core::ToolDefinition {
        rupi_core::ToolDefinition {
            name: SUBAGENT_TOOL_NAME.into(),
            description: "Delegate a self-contained subtask to a subagent; returns a summary"
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"goal": {"type": "string", "description": "subtask goal"}},
                "required": ["goal"]
            }),
            prompt_snippet: Some(
                "subagent(goal): delegate self-contained subtask, returns summary".into(),
            ),
        }
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
    ) -> anyhow::Result<rupi_tools::ToolOutput> {
        self.run_subagent(arguments, &CancelFlag::new()).await
    }

    /// 父取消直透内层循环：Esc/trl-C 置位后子 agent 在下一个检查点（turn 边界/
    /// 流中/工具间隙）停下，本工具调用回取消错误，不再等子循环跑完。
    async fn execute_with_cancel(
        &self,
        arguments: serde_json::Value,
        cancel: &CancelFlag,
    ) -> anyhow::Result<rupi_tools::ToolOutput> {
        self.run_subagent(arguments, cancel).await
    }
}

impl SubagentTool {
    async fn run_subagent(
        &self,
        arguments: serde_json::Value,
        cancel: &CancelFlag,
    ) -> anyhow::Result<rupi_tools::ToolOutput> {
        let goal = arguments
            .get("goal")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if goal.is_empty() {
            return Ok(rupi_tools::ToolOutput::err("goal required"));
        }
        // 子会话工具表：未达深度上限则带上下一级委托，达上限则摘掉，掐断递归
        let mut child_registry = self.tools.as_ref().clone();
        if self.depth + 1 < self.max_depth {
            child_registry.register(Arc::new(SubagentTool {
                provider: self.provider.clone(),
                tools: self.tools.clone(),
                mem: self.mem.clone(),
                frozen: self.frozen.clone(),
                skills: self.skills.clone(),
                max_turns: self.max_turns,
                plan_mode: self.plan_mode,
                thinking: self.thinking,
                depth: self.depth + 1,
                max_depth: self.max_depth,
            }) as Arc<dyn rupi_tools::Tool>);
        } else {
            child_registry.unregister(SUBAGENT_TOOL_NAME);
        }
        let mut agent = AgentLoop::new(self.max_turns).with_plan_mode(self.plan_mode);
        if let Some(t) = self.thinking {
            agent = agent.with_thinking(t);
        }
        let mut session = SessionTree::new();
        fn noop(_: AgentEvent) {}
        let reason = agent
            .run(
                self.provider.as_ref(),
                &mut session,
                &goal,
                &child_registry,
                self.mem.as_ref(),
                &self.frozen,
                self.skills.as_ref(),
                &[],
                &noop,
                cancel,
            )
            .await?;
        let summary = session
            .history()
            .iter()
            .rev()
            .find(|m| m.role == rupi_core::Role::Assistant)
            .map(|m| m.full_text())
            .unwrap_or_default();
        // 子任务工具清单：父模型可见子 agent 动了哪些工具（只回摘要时黑盒，
        // 父无法判断摘要可信度/是否需追问）；失败调用标 `!`（按 id 回查工具名）。
        let mut names = std::collections::HashMap::new();
        for m in session.history() {
            for b in &m.blocks {
                if let rupi_core::ContentBlock::ToolCall { id, name, .. } = b {
                    names.insert(id.clone(), name.clone());
                }
            }
        }
        let mut calls: Vec<String> = vec![];
        for m in session.history() {
            for b in &m.blocks {
                match b {
                    rupi_core::ContentBlock::ToolCall { name, .. } => calls.push(name.clone()),
                    rupi_core::ContentBlock::ToolResult {
                        tool_call_id,
                        is_error: true,
                        ..
                    } => {
                        let n = names.get(tool_call_id).cloned().unwrap_or_default();
                        calls.push(format!("{n}!"));
                    }
                    _ => {}
                }
            }
        }
        let manifest = if calls.is_empty() {
            "no tool calls".to_string()
        } else {
            format!("{} tool call(s): {}", calls.len(), calls.join(", "))
        };
        Ok(rupi_tools::ToolOutput::ok(format!(
            "[subagent {reason:?} | {manifest}]\n{summary}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rupi_llm::MockProvider;
    use rupi_memory::MemoryStore;
    use rupi_tools::Tool as _;

    fn ctx() -> (
        Arc<MockProvider>,
        Arc<ToolRegistry>,
        Arc<MemoryManager>,
        FrozenMemory,
        Arc<SkillRegistry>,
    ) {
        (
            Arc::new(MockProvider::new(vec![MockProvider::text_response(
                "sub done",
            )])),
            Arc::new(ToolRegistry::with_builtins()),
            Arc::new(MemoryManager::new(MemoryStore::new(
                std::env::temp_dir().join("rupi-sub-mem"),
            ))),
            FrozenMemory::default(),
            Arc::new(SkillRegistry::default()),
        )
    }

    #[tokio::test]
    async fn subagent_tool_returns_summary() {
        let (p, t, m, f, s) = ctx();
        let tool = SubagentTool::new(p, t, m, f, s, 3);
        let out = tool
            .execute(serde_json::json!({"goal": "do thing"}))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("sub done"));
        // 无工具调用时清单明示（父模型不猜）
        assert!(out.content.contains("no tool calls"), "{}", out.content);
    }

    #[tokio::test]
    async fn subagent_receipt_lists_tool_calls() {
        // 子任务调过工具：回执头带清单（含失败标记位），父模型可见子干了什么。
        use rupi_core::{ContentBlock, Message, Role};
        use rupi_llm::ChatResponse;
        let read_call = ChatResponse {
            message: Message {
                id: "m1".into(),
                role: Role::Assistant,
                blocks: vec![ContentBlock::ToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path": "/no/such/file.txt"}),
                }],
                provider: None,
                created_at: chrono::Utc::now(),
            },
            stop_reason: "tool_calls".into(),
        };
        let (p, t, m, f, s) = ctx();
        let tool = SubagentTool::new(
            Arc::new(MockProvider::new(vec![
                read_call,
                MockProvider::text_response("sub done"),
            ])),
            t,
            m,
            f,
            s,
            3,
        );
        let out = tool
            .execute(serde_json::json!({"goal": "do thing"}))
            .await
            .unwrap();
        assert!(!out.is_error);
        // read 不存在的文件：调用 + 失败标记双双进清单
        assert!(out.content.contains("read"), "{}", out.content);
        assert!(out.content.contains("read!"), "{}", out.content);
        let _ = p;
    }

    /// 父取消直透子循环：预置位的 flag 进来，内层 run 起手即停，
    /// provider 零调用（此前传 fresh flag，子循环会完整跑完）。
    #[tokio::test]
    async fn subagent_inherits_parent_cancellation() {
        let (p, t, m, f, s) = ctx();
        let tool = SubagentTool::new(p.clone(), t, m, f, s, 3);
        let cancel = CancelFlag::new();
        cancel.cancel();
        let start = std::time::Instant::now();
        let out = tool
            .execute_with_cancel(serde_json::json!({"goal": "do thing"}), &cancel)
            .await
            .unwrap();
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
        assert!(p.seen_tools.lock().unwrap().is_empty());
        let _ = out;
    }

    /// 子任务思考强度透传：父档位进子循环请求，默认 None 不干预。
    #[tokio::test]
    async fn thinking_level_reaches_child_loop() {        struct Capture {
            seen: std::sync::Mutex<Vec<Option<rupi_llm::ThinkingLevel>>>,
        }
        #[async_trait::async_trait]
        impl rupi_llm::LlmProvider for Capture {
            fn name(&self) -> &str {
                "capture"
            }
            async fn complete(
                &self,
                req: rupi_llm::ChatRequest,
            ) -> anyhow::Result<rupi_llm::ChatResponse> {
                self.seen.lock().unwrap().push(req.thinking);
                Ok(MockProvider::text_response("sub done"))
            }
        }
        async fn run_with(
            thinking: Option<rupi_llm::ThinkingLevel>,
        ) -> Vec<Option<rupi_llm::ThinkingLevel>> {
            let cap = Arc::new(Capture {
                seen: std::sync::Mutex::new(vec![]),
            });
            let tool = SubagentTool::new(
                cap.clone() as Arc<dyn rupi_llm::LlmProvider>,
                Arc::new(ToolRegistry::with_builtins()),
                Arc::new(MemoryManager::new(MemoryStore::new(
                    std::env::temp_dir().join("rupi-sub-think"),
                ))),
                FrozenMemory::default(),
                Arc::new(SkillRegistry::default()),
                2,
            )
            .with_thinking(thinking);
            let out = tool
                .execute(serde_json::json!({"goal": "x"}))
                .await
                .unwrap();
            assert!(!out.is_error);
            let seen = cap.seen.lock().unwrap().clone();
            assert!(!seen.is_empty());
            seen
        }
        assert_eq!(
            run_with(Some(rupi_llm::ThinkingLevel::High)).await,
            vec![Some(rupi_llm::ThinkingLevel::High)]
        );
        assert_eq!(run_with(None).await, vec![None]);
    }

    #[tokio::test]
    async fn depth_limit_removes_nested_subagent_tool() {
        let (p, t, m, f, s) = ctx();
        // 到上限的 execute：子会话工具表里不再有 subagent
        let tool = SubagentTool::new(p, t.clone(), m, f, s, 3).with_depth(1, 2);
        let out = tool
            .execute(serde_json::json!({"goal": "x"}))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("sub done"));
    }

    #[tokio::test]
    async fn fanout_runs_tasks_isolated_and_concurrent() {
        let (p, t, m, f, s) = ctx();
        // Mock 剧本按调用顺序消费；两任务各跑一轮
        let agent = AgentLoop::new(2);
        let base = SessionTree::new();
        let results = run_subagents(
            &agent,
            p.as_ref(),
            &base,
            vec![
                SubagentTask {
                    goal: "task one".into(),
                    max_turns: 2,
                },
                SubagentTask {
                    goal: "task two".into(),
                    max_turns: 2,
                },
            ],
            t.as_ref(),
            m.as_ref(),
            &f,
            s.as_ref(),
            &[],
            &|_| {},
            &CancelFlag::new(),
        )
        .await;
        assert_eq!(results.len(), 2);
        assert!(results
            .iter()
            .all(|r| matches!(r.stop_reason, StopReason::Done)));
        // 主会话不受污染
        assert_eq!(base.history().len(), 0);
    }
}
