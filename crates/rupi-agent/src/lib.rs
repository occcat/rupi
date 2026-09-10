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
pub use review::{HeuristicReviewer, ReviewSuggestion, Reviewer, TurnTranscript};

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

pub struct AgentLoop {
    pub max_turns: u32,
    pub builder: PromptBuilder,
    /// 后台 review：主循环结束后安静复盘，提炼记忆/Skill 建议。默认关闭。
    pub reviewer: Option<Arc<dyn Reviewer>>,
    pub on_suggestion: Option<Arc<dyn Fn(ReviewSuggestion) + Send + Sync>>,
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
        on_event: &dyn Fn(AgentEvent),
    ) -> anyhow::Result<StopReason> {
        session.push(Message::text(Role::User, user_input));
        // 记忆 prefetch：注入到本轮（不污染冻结快照）
        let recalled = mem.prefetch_all().await;
        let mut system = self
            .builder
            .build(frozen, mem, skills, &tools.definitions(), extensions);
        if !recalled.is_empty() {
            system.push_str(&format!("\n<Recalled>\n{recalled}\n</Recalled>\n"));
        }

        let mut tool_names: Vec<String> = vec![];
        for turn in 1..=self.max_turns {
            on_event(AgentEvent::TurnStart { turn });
            let history: Vec<Message> = session.history().into_iter().cloned().collect();
            let req = ChatRequest {
                system: system.clone(),
                messages: history,
                tools: tools.definitions(),
                max_tokens: None,
                temperature: Some(0.2),
            };
            let resp = provider.complete(req).await?;
            let has_calls = resp
                .message
                .blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolCall { .. }));
            // 文本增量事件
            for b in &resp.message.blocks {
                if let ContentBlock::Text { text } = b {
                    if !text.is_empty() {
                        on_event(AgentEvent::TextDelta {
                            delta: text.clone(),
                        });
                    }
                }
            }
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
            for (id, name, args) in calls {
                tool_names.push(name.clone());
                on_event(AgentEvent::ToolStart {
                    tool_call_id: id.clone(),
                    name: name.clone(),
                    arguments: args.clone(),
                });
                let out = if name == "load_skill" {
                    let sk = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    match skills.load_skill(sk) {
                        Some(body) => rupi_tools::ToolOutput::ok(body),
                        None => rupi_tools::ToolOutput::err(format!("unknown skill {sk}")),
                    }
                } else if let Some(routed) = mem.handle_tool_call(&name, args.clone()).await? {
                    rupi_tools::ToolOutput::ok(routed)
                } else {
                    tools.execute(&name, args.clone()).await?
                };
                on_event(AgentEvent::ToolEnd {
                    tool_call_id: id.clone(),
                    name: name.clone(),
                    content: out.content.clone(),
                    is_error: out.is_error,
                });
                results.push(ContentBlock::ToolResult {
                    tool_call_id: id,
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
    use rupi_llm::MockProvider;
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
}
