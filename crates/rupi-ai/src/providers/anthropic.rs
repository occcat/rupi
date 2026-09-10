use super::openai::MpscStream;
use super::{EventStream, Provider, StreamRequest};
use crate::error::AiError;
use crate::retry::{retry_with_backoff, RetryPolicy};
use crate::sse::SseParser;
use crate::types::{
    AssistantMessage, ContentBlock, Message, ProviderKind, StopReason, StreamEvent, ToolSpec, Usage,
};
use async_trait::async_trait;
use chrono::Utc;
use futures::StreamExt;
use reqwest::Client;
use serde_json::{json, Value};
use tokio::sync::mpsc;

pub struct AnthropicProvider {
    api_key: Option<String>,
    base_url: String,
    client: Client,
    retry: RetryPolicy,
}

impl AnthropicProvider {
    pub fn new(api_key: Option<String>, base_url: Option<String>) -> Self {
        let base_url = base_url
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| ProviderKind::Anthropic.default_base_url().to_string())
            .trim_end_matches('/')
            .to_string();
        Self {
            api_key,
            base_url,
            client: Client::builder()
                .user_agent("rupi/0.1 (pi-compatible rust agent)")
                .build()
                .expect("reqwest client"),
            retry: RetryPolicy::default(),
        }
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Anthropic
    }

    async fn stream(&self, request: StreamRequest) -> Result<EventStream, AiError> {
        let api_key = self
            .api_key
            .clone()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| AiError::MissingApiKey("anthropic".into()))?;
        let body = build_anthropic_body(&request);
        let url = format!("{}/v1/messages", self.base_url);
        let client = self.client.clone();
        let retry = self.retry.clone();
        let model_id = request.model.id.clone();

        let response = retry_with_backoff(&retry, || {
            let client = client.clone();
            let url = url.clone();
            let api_key = api_key.clone();
            let body = body.clone();
            async move {
                let resp = client
                    .post(&url)
                    .header("x-api-key", api_key)
                    .header("anthropic-version", "2023-06-01")
                    .header("content-type", "application/json")
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| AiError::Network(e.to_string()))?;
                let status = resp.status();
                if !status.is_success() {
                    let text = resp.text().await.unwrap_or_default();
                    return Err(AiError::Http {
                        status: status.as_u16(),
                        body: text,
                    });
                }
                Ok(resp)
            }
        })
        .await?;

        let (tx, rx) = mpsc::channel(64);
        let mut byte_stream = response.bytes_stream();
        tokio::spawn(async move {
            let mut parser = SseParser::default();
            let mut content: Vec<ContentBlock> = Vec::new();
            let mut usage = Usage::default();
            let mut stop = StopReason::EndTurn;
            let mut current_tool: Option<(String, String, String)> = None; // id, name, json
            let mut current_text = String::new();
            let mut current_thinking = String::new();

            while let Some(chunk) = byte_stream.next().await {
                let bytes = match chunk {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = tx.send(Err(AiError::Network(e.to_string()))).await;
                        return;
                    }
                };
                for ev in parser.push(&String::from_utf8_lossy(&bytes)) {
                    let v: Value = match serde_json::from_str(&ev.data) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    match ev.event.as_str() {
                        "error" => {
                            let msg = v
                                .pointer("/error/message")
                                .and_then(|x| x.as_str())
                                .unwrap_or(&ev.data)
                                .to_string();
                            let _ = tx.send(Ok(StreamEvent::Done(AssistantMessage::error(msg)))).await;
                            return;
                        }
                        "content_block_start" => {
                            flush_text(&mut current_text, &mut content);
                            flush_thinking(&mut current_thinking, &mut content);
                            if let Some(block) = v.get("content_block") {
                                let ty = block.get("type").and_then(|x| x.as_str()).unwrap_or("");
                                if ty == "tool_use" {
                                    let id = block
                                        .get("id")
                                        .and_then(|x| x.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    let name = block
                                        .get("name")
                                        .and_then(|x| x.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    let _ = tx
                                        .send(Ok(StreamEvent::ToolCallStart {
                                            id: id.clone(),
                                            name: name.clone(),
                                        }))
                                        .await;
                                    current_tool = Some((id, name, String::new()));
                                }
                            }
                        }
                        "content_block_delta" => {
                            if let Some(delta) = v.get("delta") {
                                let ty = delta.get("type").and_then(|x| x.as_str()).unwrap_or("");
                                if ty == "text_delta" {
                                    if let Some(t) = delta.get("text").and_then(|x| x.as_str()) {
                                        current_text.push_str(t);
                                        let _ = tx.send(Ok(StreamEvent::TextDelta(t.to_string()))).await;
                                    }
                                } else if ty == "thinking_delta" {
                                    if let Some(t) = delta.get("thinking").and_then(|x| x.as_str()) {
                                        current_thinking.push_str(t);
                                        let _ = tx
                                            .send(Ok(StreamEvent::ThinkingDelta(t.to_string())))
                                            .await;
                                    }
                                } else if ty == "input_json_delta" {
                                    if let Some(partial) =
                                        delta.get("partial_json").and_then(|x| x.as_str())
                                    {
                                        if let Some((id, _, json_buf)) = current_tool.as_mut() {
                                            json_buf.push_str(partial);
                                            let _ = tx
                                                .send(Ok(StreamEvent::ToolCallDelta {
                                                    id: id.clone(),
                                                    arguments_delta: partial.to_string(),
                                                }))
                                                .await;
                                        }
                                    }
                                }
                            }
                        }
                        "content_block_stop" => {
                            flush_text(&mut current_text, &mut content);
                            flush_thinking(&mut current_thinking, &mut content);
                            if let Some((id, name, json_buf)) = current_tool.take() {
                                let parsed: Value =
                                    serde_json::from_str(&json_buf).unwrap_or(json!({}));
                                content.push(ContentBlock::ToolCall {
                                    id,
                                    name,
                                    arguments: parsed,
                                    arguments_json: json_buf,
                                });
                            }
                        }
                        "message_delta" => {
                            if let Some(u) = v.get("usage") {
                                usage.output =
                                    u.get("output_tokens").and_then(|x| x.as_u64()).unwrap_or(0)
                                        as u32;
                            }
                            if let Some(sr) = v
                                .pointer("/delta/stop_reason")
                                .and_then(|x| x.as_str())
                            {
                                stop = match sr {
                                    "tool_use" => StopReason::ToolUse,
                                    "max_tokens" => StopReason::Length,
                                    _ => StopReason::EndTurn,
                                };
                            }
                        }
                        "message_start" => {
                            if let Some(u) = v.pointer("/message/usage") {
                                usage.input =
                                    u.get("input_tokens").and_then(|x| x.as_u64()).unwrap_or(0)
                                        as u32;
                                usage.cache_read = u
                                    .get("cache_read_input_tokens")
                                    .and_then(|x| x.as_u64())
                                    .unwrap_or(0)
                                    as u32;
                                usage.cache_write = u
                                    .get("cache_creation_input_tokens")
                                    .and_then(|x| x.as_u64())
                                    .unwrap_or(0)
                                    as u32;
                            }
                        }
                        _ => {}
                    }
                }
            }

            flush_text(&mut current_text, &mut content);
            flush_thinking(&mut current_thinking, &mut content);
            if let Some((id, name, json_buf)) = current_tool.take() {
                let parsed: Value = serde_json::from_str(&json_buf).unwrap_or(json!({}));
                content.push(ContentBlock::ToolCall {
                    id,
                    name,
                    arguments: parsed,
                    arguments_json: json_buf,
                });
            }
            if content.iter().any(|c| matches!(c, ContentBlock::ToolCall { .. }))
                && stop == StopReason::EndTurn
            {
                stop = StopReason::ToolUse;
            }
            let _ = tx
                .send(Ok(StreamEvent::Done(AssistantMessage {
                    content,
                    stop_reason: stop,
                    usage,
                    error_message: None,
                    timestamp: Some(Utc::now()),
                    model: Some(model_id),
                })))
                .await;
        });

        Ok(Box::pin(MpscStream::new(rx)))
    }
}

fn flush_text(buf: &mut String, content: &mut Vec<ContentBlock>) {
    if !buf.is_empty() {
        content.push(ContentBlock::text(std::mem::take(buf)));
    }
}

fn flush_thinking(buf: &mut String, content: &mut Vec<ContentBlock>) {
    if !buf.is_empty() {
        content.push(ContentBlock::Thinking {
            thinking: std::mem::take(buf),
        });
    }
}

fn build_anthropic_body(request: &StreamRequest) -> Value {
    let mut messages = Vec::new();
    for msg in &request.messages {
        match msg {
            Message::System { .. } => {}
            Message::User { content, .. } => {
                messages.push(json!({
                    "role": "user",
                    "content": [{"type": "text", "text": flatten(content)}]
                }));
            }
            Message::Assistant(asst) => {
                let mut blocks = Vec::new();
                for b in &asst.content {
                    match b {
                        ContentBlock::Text { text } => {
                            blocks.push(json!({"type": "text", "text": text}));
                        }
                        ContentBlock::Thinking { thinking } => {
                            blocks.push(json!({"type": "thinking", "thinking": thinking}));
                        }
                        ContentBlock::ToolCall {
                            id,
                            name,
                            arguments,
                            ..
                        } => {
                            blocks.push(json!({
                                "type": "tool_use",
                                "id": id,
                                "name": name,
                                "input": arguments
                            }));
                        }
                        _ => {}
                    }
                }
                messages.push(json!({"role": "assistant", "content": blocks}));
            }
            Message::Tool {
                tool_call_id,
                content,
                is_error,
                ..
            } => {
                // Anthropic requires tool_result as a user message.
                let last_is_user = messages
                    .last()
                    .and_then(|m| m.get("role"))
                    .and_then(|r| r.as_str())
                    == Some("user");
                let result = json!({
                    "type": "tool_result",
                    "tool_use_id": tool_call_id,
                    "content": flatten(content),
                    "is_error": is_error
                });
                if last_is_user {
                    if let Some(Value::Object(obj)) = messages.last_mut() {
                        if let Some(Value::Array(arr)) = obj.get_mut("content") {
                            arr.push(result);
                            continue;
                        }
                    }
                }
                messages.push(json!({
                    "role": "user",
                    "content": [result]
                }));
            }
        }
    }

    let mut system = request.system.clone().unwrap_or_default();
    for msg in &request.messages {
        if let Message::System { content, .. } = msg {
            if !system.is_empty() {
                system.push_str("\n\n");
            }
            system.push_str(content);
        }
    }

    let mut body = json!({
        "model": request.model.id,
        "messages": messages,
        "max_tokens": request.options.max_tokens.unwrap_or(request.model.max_output.max(4096)),
        "stream": true,
    });
    if !system.is_empty() {
        body["system"] = json!(system);
    }
    if let Some(t) = request.options.temperature {
        body["temperature"] = json!(t);
    }
    if !request.tools.is_empty() {
        body["tools"] = Value::Array(request.tools.iter().map(tool_to_anthropic).collect());
    }
    body
}

fn tool_to_anthropic(tool: &ToolSpec) -> Value {
    json!({
        "name": tool.name,
        "description": tool.description,
        "input_schema": tool.parameters,
    })
}

fn flatten(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| b.as_text())
        .collect::<Vec<_>>()
        .join("\n")
}
