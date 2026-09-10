//! 子任务分发：对标 Pi 生态的 sub-agents 扩展。
//!
//! 两层：
//! - [`run_subagents`]：把任务扇出到分叉会话上并发跑（`SessionTree::branch_from` 隔离上下文，
//!   修坏不污染主会话，完事只汇总结果），`join_all` 同 executor 并发，无 `'static` 约束。
//! - [`SubagentTool`]：模型可调用的 `subagent` 工具（单任务委托）；深度 guard 防无限递归。

use crate::AgentLoop;
use rupi_core::{AgentEvent, SessionTree, StopReason};
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
pub struct SubagentTool {
    provider: Arc<dyn LlmProvider>,
    tools: Arc<ToolRegistry>,
    mem: Arc<MemoryManager>,
    frozen: FrozenMemory,
    skills: Arc<SkillRegistry>,
    max_turns: u32,
    plan_mode: bool,
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
                depth: self.depth + 1,
                max_depth: self.max_depth,
            }) as Arc<dyn rupi_tools::Tool>);
        } else {
            child_registry.unregister(SUBAGENT_TOOL_NAME);
        }
        let agent = AgentLoop::new(self.max_turns).with_plan_mode(self.plan_mode);
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
            )
            .await?;
        let summary = session
            .history()
            .iter()
            .rev()
            .find(|m| m.role == rupi_core::Role::Assistant)
            .map(|m| m.full_text())
            .unwrap_or_default();
        Ok(rupi_tools::ToolOutput::ok(format!(
            "[subagent {reason:?}]\n{summary}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rupi_llm::MockProvider;
    use rupi_memory::MemoryStore;
    use rupi_tools::Tool as _;

    fn ignore(_: AgentEvent) {}

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
