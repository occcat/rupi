//! rupi-llm: 统一 LLM API（对标 pi-ai）。
//! Provider 无关：上层只依赖 [`LlmProvider`] trait；会话可混用多家消息，provider 只做尽力兼容。

use async_trait::async_trait;
use rupi_core::{ContentBlock, Message, Role, ToolDefinition};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    pub message: Message,
    pub stop_reason: String,
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn name(&self) -> &str;
    async fn complete(&self, req: ChatRequest) -> anyhow::Result<ChatResponse>;
}

/// 将内部消息转换为 OpenAI chat-completions 风格的 JSON。
pub fn to_openai_messages(system: &str, messages: &[Message]) -> Vec<serde_json::Value> {
    let mut out = vec![serde_json::json!({"role": "system", "content": system})];
    for m in messages {
        match m.role {
            Role::System => out.push(serde_json::json!({"role":"system","content": m.full_text()})),
            Role::User => out.push(serde_json::json!({"role":"user","content": m.full_text()})),
            Role::Assistant => {
                let calls: Vec<_> = m
                    .blocks
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolCall { id, name, arguments } => Some(
                            serde_json::json!({"id": id, "type":"function","function":{"name": name, "arguments": arguments.to_string()}}),
                        ),
                        _ => None,
                    })
                    .collect();
                let text: String = m
                    .blocks
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if calls.is_empty() {
                    out.push(serde_json::json!({"role":"assistant","content": text}));
                } else {
                    out.push(serde_json::json!({"role":"assistant","content": text, "tool_calls": calls}));
                }
            }
            Role::Tool => {
                for b in &m.blocks {
                    if let ContentBlock::ToolResult {
                        tool_call_id,
                        content,
                        ..
                    } = b
                    {
                        out.push(serde_json::json!({"role":"tool","tool_call_id": tool_call_id, "content": content}));
                    }
                }
            }
        }
    }
    out
}

pub fn to_openai_tools(tools: &[ToolDefinition]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "type": "function",
                "function": {"name": t.name, "description": t.description, "parameters": t.input_schema}
            })
        })
        .collect()
}

/// OpenAI-compatible provider：覆盖 OpenAI / DeepSeek / Moonshot / 本地 Ollama 等。
/// 通过 `base_url + api_key + model` 配置，默认 `https://api.openai.com/v1`。
#[derive(Debug, Clone)]
pub struct OpenAiCompatProvider {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    client: reqwest::Client,
}

impl OpenAiCompatProvider {
    pub fn new(base_url: String, api_key: String, model: String) -> Self {
        Self {
            base_url,
            api_key,
            model,
            client: reqwest::Client::new(),
        }
    }

    pub fn from_env(model: String) -> anyhow::Result<Self> {
        let base_url = std::env::var("RUPI_BASE_URL")
            .or_else(|_| std::env::var("OPENAI_BASE_URL"))
            .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
        let api_key = std::env::var("RUPI_API_KEY").or_else(|_| std::env::var("OPENAI_API_KEY"))?;
        Ok(Self::new(base_url, api_key, model))
    }
}

#[async_trait]
impl LlmProvider for OpenAiCompatProvider {
    fn name(&self) -> &str {
        "openai-compat"
    }

    async fn complete(&self, req: ChatRequest) -> anyhow::Result<ChatResponse> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let body = serde_json::json!({
            "model": self.model,
            "messages": to_openai_messages(&req.system, &req.messages),
            "tools": to_openai_tools(&req.tools),
            "temperature": req.temperature.unwrap_or(0.2),
        });
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?
            .error_for_status()?;
        let v: serde_json::Value = resp.json().await?;
        parse_openai_response(v)
    }
}

fn parse_openai_response(v: serde_json::Value) -> anyhow::Result<ChatResponse> {
    let choice = v
        .pointer("/choices/0/message")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("bad openai response: {v}"))?;
    let mut blocks = vec![];
    if let Some(text) = choice.get("content").and_then(|c| c.as_str()) {
        if !text.is_empty() {
            blocks.push(ContentBlock::Text {
                text: text.to_string(),
            });
        }
    }
    if let Some(calls) = choice.get("tool_calls").and_then(|c| c.as_array()) {
        for c in calls {
            let id = c
                .get("id")
                .and_then(|s| s.as_str())
                .unwrap_or("call-0")
                .to_string();
            let name = c
                .pointer("/function/name")
                .and_then(|s| s.as_str())
                .unwrap_or("unknown")
                .to_string();
            let args_str = c
                .pointer("/function/arguments")
                .and_then(|s| s.as_str())
                .unwrap_or("{}");
            let arguments: serde_json::Value =
                serde_json::from_str(args_str).unwrap_or(serde_json::json!({}));
            blocks.push(ContentBlock::ToolCall {
                id,
                name,
                arguments,
            });
        }
    }
    if blocks.is_empty() {
        blocks.push(ContentBlock::Text {
            text: String::new(),
        });
    }
    Ok(ChatResponse {
        message: Message {
            id: uuid::Uuid::new_v4().to_string(),
            role: Role::Assistant,
            blocks,
            provider: Some("openai-compat".into()),
            created_at: chrono::Utc::now(),
        },
        stop_reason: v
            .pointer("/choices/0/finish_reason")
            .and_then(|s| s.as_str())
            .unwrap_or("stop")
            .to_string(),
    })
}

/// Mock provider：单测 / 无 Key 演示用，按预设剧本返回。
pub struct MockProvider {
    pub script: std::sync::Mutex<Vec<ChatResponse>>,
}

impl MockProvider {
    pub fn new(script: Vec<ChatResponse>) -> Self {
        Self {
            script: std::sync::Mutex::new(script),
        }
    }

    pub fn text_response(text: &str) -> ChatResponse {
        ChatResponse {
            message: Message::text(Role::Assistant, text),
            stop_reason: "stop".into(),
        }
    }
}

#[async_trait]
impl LlmProvider for MockProvider {
    fn name(&self) -> &str {
        "mock"
    }
    async fn complete(&self, _req: ChatRequest) -> anyhow::Result<ChatResponse> {
        let mut g = self.script.lock().unwrap();
        if g.is_empty() {
            Ok(Self::text_response("done"))
        } else {
            Ok(g.remove(0))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_message_mapping_keeps_tool_calls() {
        let m = Message {
            id: "1".into(),
            role: Role::Assistant,
            blocks: vec![ContentBlock::ToolCall {
                id: "c1".into(),
                name: "read".into(),
                arguments: serde_json::json!({"path":"a.txt"}),
            }],
            provider: None,
            created_at: chrono::Utc::now(),
        };
        let msgs = to_openai_messages("sys", &[m]);
        assert_eq!(msgs.len(), 2);
        assert!(msgs[1].get("tool_calls").is_some());
    }
}
