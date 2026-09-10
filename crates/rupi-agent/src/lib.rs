//! Pi-style agent runtime: tool-calling loop, sessions, compaction, coding tools.

mod compaction;
mod context_files;
mod events;
mod loop_;
mod messages;
mod session;
mod system_prompt;
mod tools;
mod truncate;

pub use compaction::{compact_messages, should_compact, CompactionSettings, CompactResult};
pub use context_files::{load_context_files, ContextFile};
pub use events::AgentEvent;
pub use loop_::{run_agent_loop, AgentLoopConfig, QueueMode, ToolExecutionMode};
pub use messages::convert_to_llm;
pub use session::{Session, SessionEntry, SessionHeader, SessionManager};
pub use system_prompt::{
    build_system_prompt, BuildSystemPromptOptions, SkillPromptEntry, ToolSnippet,
};
pub use tools::{
    builtin_tools, create_tool, Tool, ToolContext, ToolError, ToolName, ToolRegistry, ToolResult,
};
pub use truncate::{format_size, truncate_head, truncate_tail, TruncationResult, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};

pub use rupi_ai::{Message, ToolSpec};
