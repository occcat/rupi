use rupi_ai::{AssistantMessage, Message, Usage};

#[derive(Debug, Clone)]
pub enum AgentEvent {
    AgentStart,
    TurnStart,
    MessageStart { role: String },
    TextDelta { text: String },
    ThinkingDelta { text: String },
    ToolCallStart { id: String, name: String },
    ToolExecutionStart { id: String, name: String, args: serde_json::Value },
    ToolExecutionEnd { id: String, name: String, is_error: bool, preview: String },
    MessageEnd { message: Message },
    TurnEnd { message: AssistantMessage },
    AgentEnd { messages: Vec<Message>, usage: Usage },
    Compaction { summary: String, tokens_before: u32 },
    Error { message: String },
}
