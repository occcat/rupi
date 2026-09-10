use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    Length,
    Aborted,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    #[default]
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ThinkingLevel {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "off" => Some(Self::Off),
            "minimal" => Some(Self::Minimal),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::Xhigh),
            "max" => Some(Self::Max),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    OpenAi,
    Anthropic,
    Google,
    OpenRouter,
    #[serde(rename = "openai-compat")]
    OpenAiCompat,
    Faux,
}

impl ProviderKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "openai" => Some(Self::OpenAi),
            "anthropic" => Some(Self::Anthropic),
            "google" | "gemini" => Some(Self::Google),
            "openrouter" => Some(Self::OpenRouter),
            "openai-compat" | "compat" | "ollama" | "groq" | "deepseek" => Some(Self::OpenAiCompat),
            "faux" => Some(Self::Faux),
            _ => None,
        }
    }

    pub fn default_base_url(self) -> &'static str {
        match self {
            Self::OpenAi => "https://api.openai.com/v1",
            Self::Anthropic => "https://api.anthropic.com",
            Self::Google => "https://generativelanguage.googleapis.com/v1beta",
            Self::OpenRouter => "https://openrouter.ai/api/v1",
            Self::OpenAiCompat => "http://127.0.0.1:11434/v1",
            Self::Faux => "",
        }
    }

    pub fn env_key(self) -> &'static str {
        match self {
            Self::OpenAi => "OPENAI_API_KEY",
            Self::Anthropic => "ANTHROPIC_API_KEY",
            Self::Google => "GEMINI_API_KEY",
            Self::OpenRouter => "OPENROUTER_API_KEY",
            Self::OpenAiCompat => "OPENAI_API_KEY",
            Self::Faux => "",
        }
    }
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::Google => "google",
            Self::OpenRouter => "openrouter",
            Self::OpenAiCompat => "openai-compat",
            Self::Faux => "faux",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ContentBlock {
    Text { text: String },
    Thinking { thinking: String },
    ToolCall {
        id: String,
        name: String,
        #[serde(default)]
        arguments: Value,
        #[serde(default)]
        arguments_json: String,
    },
    Image {
        mime_type: String,
        data: String,
    },
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text { text } => Some(text),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Message {
    System {
        content: String,
        #[serde(default)]
        timestamp: Option<DateTime<Utc>>,
    },
    User {
        content: Vec<ContentBlock>,
        #[serde(default)]
        timestamp: Option<DateTime<Utc>>,
    },
    Assistant(AssistantMessage),
    Tool {
        tool_call_id: String,
        tool_name: String,
        content: Vec<ContentBlock>,
        #[serde(default)]
        is_error: bool,
        #[serde(default)]
        timestamp: Option<DateTime<Utc>>,
    },
}

impl Message {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self::User {
            content: vec![ContentBlock::text(text)],
            timestamp: Some(Utc::now()),
        }
    }

    pub fn system(text: impl Into<String>) -> Self {
        Self::System {
            content: text.into(),
            timestamp: Some(Utc::now()),
        }
    }

    pub fn tool_result(tool_call_id: String, tool_name: String, text: String, is_error: bool) -> Self {
        Self::Tool {
            tool_call_id,
            tool_name,
            content: vec![ContentBlock::text(text)],
            is_error,
            timestamp: Some(Utc::now()),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    #[serde(default)]
    pub input: u32,
    #[serde(default)]
    pub output: u32,
    #[serde(default)]
    pub cache_read: u32,
    #[serde(default)]
    pub cache_write: u32,
    #[serde(default)]
    pub total_cost: f64,
}

impl Usage {
    pub fn add(&mut self, other: &Usage) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        self.total_cost += other.total_cost;
    }

    pub fn total_tokens(&self) -> u32 {
        self.input + self.output + self.cache_read + self.cache_write
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    pub content: Vec<ContentBlock>,
    pub stop_reason: StopReason,
    #[serde(default)]
    pub usage: Usage,
    #[serde(default)]
    pub error_message: Option<String>,
    #[serde(default)]
    pub timestamp: Option<DateTime<Utc>>,
    #[serde(default)]
    pub model: Option<String>,
}

impl AssistantMessage {
    pub fn text_only(text: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::text(text)],
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
            error_message: None,
            timestamp: Some(Utc::now()),
            model: None,
        }
    }

    pub fn error(msg: impl Into<String>) -> Self {
        Self {
            content: vec![],
            stop_reason: StopReason::Error,
            usage: Usage::default(),
            error_message: Some(msg.into()),
            timestamp: Some(Utc::now()),
            model: None,
        }
    }

    pub fn aborted() -> Self {
        Self {
            content: vec![],
            stop_reason: StopReason::Aborted,
            usage: Usage::default(),
            error_message: Some("aborted".into()),
            timestamp: Some(Utc::now()),
            model: None,
        }
    }

    pub fn combined_text(&self) -> String {
        self.content
            .iter()
            .filter_map(|c| c.as_text())
            .collect::<Vec<_>>()
            .join("")
    }

    pub fn tool_calls(&self) -> Vec<&ContentBlock> {
        self.content
            .iter()
            .filter(|c| matches!(c, ContentBlock::ToolCall { .. }))
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub provider: ProviderKind,
    pub display_name: String,
    #[serde(default)]
    pub context_window: u32,
    #[serde(default)]
    pub max_output: u32,
    #[serde(default)]
    pub supports_tools: bool,
    #[serde(default)]
    pub supports_images: bool,
    #[serde(default)]
    pub supports_thinking: bool,
}

impl Model {
    pub fn new(id: impl Into<String>, provider: ProviderKind) -> Self {
        let id = id.into();
        Self {
            display_name: id.clone(),
            id,
            provider,
            context_window: 128_000,
            max_output: 16_384,
            supports_tools: true,
            supports_images: false,
            supports_thinking: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, Default)]
pub struct StreamOptions {
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub thinking: ThinkingLevel,
    pub abort: Option<tokio::sync::watch::Receiver<bool>>,
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
    TextDelta(String),
    ThinkingDelta(String),
    ToolCallStart { id: String, name: String },
    ToolCallDelta { id: String, arguments_delta: String },
    Usage(Usage),
    Done(AssistantMessage),
}

/// Rough token estimate used by compaction (Pi uses a similar chars/4 heuristic).
pub fn estimate_tokens(text: &str) -> u32 {
    ((text.chars().count() as f64) / 4.0).ceil() as u32
}

pub fn estimate_messages_tokens(messages: &[Message]) -> u32 {
    messages.iter().map(estimate_message_tokens).sum()
}

pub fn estimate_message_tokens(message: &Message) -> u32 {
    match message {
        Message::System { content, .. } => estimate_tokens(content) + 4,
        Message::User { content, .. } | Message::Tool { content, .. } => {
            content.iter().map(estimate_block_tokens).sum::<u32>() + 4
        }
        Message::Assistant(m) => m.content.iter().map(estimate_block_tokens).sum::<u32>() + 4,
    }
}

fn estimate_block_tokens(block: &ContentBlock) -> u32 {
    match block {
        ContentBlock::Text { text } => estimate_tokens(text),
        ContentBlock::Thinking { thinking } => estimate_tokens(thinking),
        ContentBlock::ToolCall {
            name,
            arguments,
            arguments_json,
            ..
        } => {
            estimate_tokens(name)
                + if arguments_json.is_empty() {
                    estimate_tokens(&arguments.to_string())
                } else {
                    estimate_tokens(arguments_json)
                }
        }
        ContentBlock::Image { .. } => 800,
    }
}

pub fn content_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| b.as_text())
        .collect::<Vec<_>>()
        .join("\n")
}
