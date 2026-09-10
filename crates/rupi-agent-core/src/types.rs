use rupi_ai::{Message, Model, ThinkingLevel, ToolDefinition};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use rupi_ai::ThinkingLevel as AgentThinkingLevel;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolExecutionMode {
    Sequential,
    Parallel,
}

impl Default for ToolExecutionMode {
    fn default() -> Self {
        Self::Parallel
    }
}

#[derive(Debug, Clone)]
pub struct AgentContext {
    pub system_prompt: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
}

impl AgentContext {
    pub fn new(system_prompt: impl Into<String>) -> Self {
        Self {
            system_prompt: system_prompt.into(),
            messages: Vec::new(),
            tools: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct BeforeToolCallContext {
    pub assistant_message: Message,
    pub tool_call_id: String,
    pub tool_name: String,
    pub args: Value,
}

#[derive(Debug, Clone, Default)]
pub struct BeforeToolCallResult {
    pub block: bool,
    pub reason: Option<String>,
    pub terminate: bool,
}

#[derive(Debug, Clone)]
pub struct AfterToolCallContext {
    pub assistant_message: Message,
    pub tool_call_id: String,
    pub tool_name: String,
    pub args: Value,
    pub is_error: bool,
    pub result_text: String,
}

#[derive(Debug, Clone, Default)]
pub struct AfterToolCallResult {
    pub content: Option<String>,
    pub is_error: Option<bool>,
    pub terminate: Option<bool>,
    pub details: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct TurnUpdate {
    pub context: Option<AgentContext>,
    pub model: Option<Model>,
    pub thinking_level: Option<ThinkingLevel>,
}

#[derive(Debug, Clone)]
pub struct ShouldStopAfterTurnContext {
    pub message: Message,
    pub tool_results: Vec<Message>,
    pub context: AgentContext,
    pub new_messages: Vec<Message>,
}

pub type ConvertToLlm = Box<dyn Fn(&[Message]) -> Vec<Message> + Send + Sync>;

pub fn default_convert_to_llm(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .filter(|m| matches!(m, Message::User { .. } | Message::Assistant { .. } | Message::ToolResult { .. }))
        .cloned()
        .collect()
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("busy")]
    Busy,
    #[error("cannot continue: {0}")]
    Continue(String),
    #[error("tool error: {0}")]
    Tool(String),
    #[error("harness: {0}")]
    Harness(String),
    #[error(transparent)]
    Ai(#[from] rupi_ai::AiError),
}

pub type AgentResult<T> = Result<T, AgentError>;
