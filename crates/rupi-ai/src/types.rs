use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::usage::Usage;

/// Stop reason for an assistant turn. Matches pi-ai.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    Stop,
    #[serde(rename = "toolUse")]
    ToolUse,
    Length,
    Error,
    Aborted,
}

impl StopReason {
    pub fn is_terminal_failure(self) -> bool {
        matches!(self, StopReason::Error | StopReason::Aborted)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
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

    pub fn tool_call(id: impl Into<String>, name: impl Into<String>, arguments: Value) -> Self {
        Self::ToolCall {
            id: id.into(),
            name: name.into(),
            arguments,
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text { text } => Some(text),
            _ => None,
        }
    }

    pub fn as_tool_call(&self) -> Option<(&str, &str, &Value)> {
        match self {
            Self::ToolCall { id, name, arguments } => Some((id, name, arguments)),
            _ => None,
        }
    }
}

pub fn content_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| b.as_text())
        .collect::<Vec<_>>()
        .join("")
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Message {
    User {
        content: Vec<ContentBlock>,
        #[serde(default = "now_ms")]
        timestamp: i64,
    },
    Assistant {
        content: Vec<ContentBlock>,
        stop_reason: StopReason,
        #[serde(default)]
        usage: Usage,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error_message: Option<String>,
        #[serde(default = "now_ms")]
        timestamp: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
    },
    #[serde(rename = "toolResult")]
    ToolResult {
        tool_call_id: String,
        tool_name: String,
        content: Vec<ContentBlock>,
        #[serde(default)]
        is_error: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
        #[serde(default = "now_ms")]
        timestamp: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        terminate: Option<bool>,
    },
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self::User {
            content: vec![ContentBlock::text(text)],
            timestamp: now_ms(),
        }
    }

    pub fn assistant_text(text: impl Into<String>) -> Self {
        Self::Assistant {
            content: vec![ContentBlock::text(text)],
            stop_reason: StopReason::Stop,
            usage: Usage::default(),
            error_message: None,
            timestamp: now_ms(),
            model: None,
            provider: None,
        }
    }

    pub fn assistant_tool_calls(calls: Vec<ContentBlock>) -> Self {
        Self::Assistant {
            content: calls,
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
            error_message: None,
            timestamp: now_ms(),
            model: None,
            provider: None,
        }
    }

    pub fn error(msg: impl Into<String>) -> Self {
        Self::Assistant {
            content: vec![],
            stop_reason: StopReason::Error,
            usage: Usage::default(),
            error_message: Some(msg.into()),
            timestamp: now_ms(),
            model: None,
            provider: None,
        }
    }

    pub fn aborted(msg: impl Into<String>) -> Self {
        Self::Assistant {
            content: vec![],
            stop_reason: StopReason::Aborted,
            usage: Usage::default(),
            error_message: Some(msg.into()),
            timestamp: now_ms(),
            model: None,
            provider: None,
        }
    }

    pub fn tool_result(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        text: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self::ToolResult {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            content: vec![ContentBlock::text(text)],
            is_error,
            details: None,
            timestamp: now_ms(),
            terminate: None,
        }
    }

    pub fn role_name(&self) -> &'static str {
        match self {
            Self::User { .. } => "user",
            Self::Assistant { .. } => "assistant",
            Self::ToolResult { .. } => "toolResult",
        }
    }

    pub fn is_assistant(&self) -> bool {
        matches!(self, Self::Assistant { .. })
    }

    pub fn stop_reason(&self) -> Option<StopReason> {
        match self {
            Self::Assistant { stop_reason, .. } => Some(*stop_reason),
            _ => None,
        }
    }

    pub fn tool_calls(&self) -> Vec<&ContentBlock> {
        match self {
            Self::Assistant { content, .. } => content
                .iter()
                .filter(|c| matches!(c, ContentBlock::ToolCall { .. }))
                .collect(),
            _ => vec![],
        }
    }

    pub fn timestamp(&self) -> i64 {
        match self {
            Self::User { timestamp, .. }
            | Self::Assistant { timestamp, .. }
            | Self::ToolResult { timestamp, .. } => *timestamp,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Context {
    pub system_prompt: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
}

#[derive(Debug, Clone, Default)]
pub struct StreamOptions {
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub thinking_level: Option<crate::model::ThinkingLevel>,
    pub headers: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub enum AssistantEvent {
    Start,
    TextDelta { text: String },
    ThinkingDelta { thinking: String },
    ToolCallStart { id: String, name: String },
    ToolCallDelta { id: String, arguments_delta: String },
    ToolCallEnd { id: String, name: String, arguments: Value },
    Done { message: Message },
}

pub fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

pub fn now() -> DateTime<Utc> {
    Utc::now()
}

#[derive(Debug, thiserror::Error)]
pub enum AiError {
    #[error("provider error: {0}")]
    Provider(String),
    #[error("http error: {0}")]
    Http(String),
    #[error("aborted")]
    Aborted,
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error("unknown model {provider}/{id}")]
    UnknownModel { provider: String, id: String },
}

impl From<reqwest::Error> for AiError {
    fn from(value: reqwest::Error) -> Self {
        Self::Http(value.to_string())
    }
}

pub type AiResult<T> = Result<T, AiError>;
