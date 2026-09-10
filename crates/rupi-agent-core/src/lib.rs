//! Agent runtime matching `@earendil-works/pi-agent-core` 0.85.1.
//!
//! Low-level loop: `agent_loop` / `agent_loop_continue`.
//! High-level: [`Agent`] with steering/follow-up queues.
//! Harness extras: compaction, JSONL session tree, permission gates, sub-agents.

mod agent;
mod compaction;
mod events;
mod loop_;
mod permissions;
mod queue;
mod session;
mod subagent;
mod system_prompt;
mod tools;
mod types;

pub use agent::Agent;
pub use compaction::{
    compact_messages, estimate_context_tokens, find_cut_point, should_compact, CompactionSettings,
    DEFAULT_COMPACTION_SETTINGS,
};
pub use events::{AgentEvent, EventLog};
pub use loop_::{agent_loop, agent_loop_continue, AgentLoopConfig};
pub use permissions::{PermissionDecision, PermissionGate, PathGuard};
pub use queue::{MessageQueue, QueueMode};
pub use session::{SessionEntry, SessionStore};
pub use subagent::{SubagentConfig, SubagentResult, run_subagent};
pub use system_prompt::{
    build_system_prompt, escape_xml, format_skills_for_system_prompt, SkillPromptEntry,
    SystemPromptParts,
};
pub use tools::{AgentTool, AgentToolResult, ToolExecutor, ToolSet};
pub use types::*;
