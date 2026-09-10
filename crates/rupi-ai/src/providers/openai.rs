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
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::mpsc;

pub struct OpenAiProvider {
    kind: ProviderKind,
    api_key: Option<String>,
    base_url: String,
    client: Client,
    retry: RetryPolicy,
}

impl OpenAiProvider {
    pub fn new(kind: ProviderKind, api_key: Option<String>, base_url: Option<String>) -> Self {
        let base_url = base_url
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| kind.default_base_url().to_string())
            .trim_end_matches('/')
            .to_string();
        Self {
            kind,
            api_key,
            base_url,
            client: Client::builder()
                .user_agent("rupi/0.1 (pi-compatible rust agent)")
                .build()
                .expect("reqwest client"),
            retry: RetryPolicy::default(),
        }
    }

    fn headers(&self) -> Result<reqwest::header::HeaderMap, AiError> {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            "application/json".parse().unwrap(),
        );
        if let Some(key) = &self.api_key {
            if !key.is_empty() {
                headers.insert(
                    reqwest::header::AUTHORIZATION,
                    format!("Bearer {key}").parse().unwrap(),
                );
            }
        } else if self.kind != ProviderKind::OpenAiCompat {
            return Err(AiError::MissingApiKey(self.kind.to_string()));
        }
        if self.kind == ProviderKind::OpenRouter {
            headers.insert(
                "HTTP-Referer",
                "https://github.com/occcat/rupi".parse().unwrap(),
            );
            headers.insert("X-Title", "rupi".parse().unwrap());
        }
        Ok(headers)
    }
}

pub(crate) struct MpscStream {
    rx: mpsc::Receiver<Result<StreamEvent, AiError>>,
}

impl MpscStream {
    pub fn new(rx: mpsc::Receiver<Result<StreamEvent, AiError>>) -> Self {
        Self { rx }
    }
}

impl futures::Stream for MpscStream {
    type Item = Result<StreamEvent, AiError>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    fn kind(&self) -> ProviderKind {
        self.kind
    }

    async fn stream(&self, request: StreamRequest) -> Result<EventStream, AiError> {
        let body = build_openai_body(&request);
        let url = format!("{}/chat/completions", self.base_url);
        let headers = self.headers()?;
        let client = self.client.clone();
        let retry = self.retry.clone();
        let model_id = request.model.id.clone();

        let response = retry_with_backoff(&retry, || {
            let client = client.clone();
            let url = url.clone();
            let headers = headers.clone();
            let body = body.clone();
            async move {
                let resp = client
                    .post(&url)
                    .headers(headers)
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
            let mut text = String::new();
            let mut tool_acc: Vec<ToolAcc> = Vec::new();
            let mut usage = Usage::default();
            let mut finish: Option<String> = None;

            while let Some(chunk) = byte_stream.next().await {
                let bytes = match chunk {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = tx.send(Err(AiError::Network(e.to_string()))).await;
                        return;
                    }
                };
                for ev in parser.push(&String::from_utf8_lossy(&bytes)) {
                    if ev.data == "[DONE]" {
                        continue;
                    }
                    let v: Value = match serde_json::from_str(&ev.data) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    if let Some(err) = v.get("error") {
                        let _ = tx
                            .send(Ok(StreamEvent::Done(AssistantMessage::error(err.to_string()))))
                            .await;
                        return;
                    }
                    if let Some(u) = v.get("usage") {
                        usage.input =
                            u.get("prompt_tokens").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
                        usage.output = u
                            .get("completion_tokens")
                            .and_then(|x| x.as_u64())
                            .unwrap_or(0) as u32;
                        let _ = tx.send(Ok(StreamEvent::Usage(usage.clone()))).await;
                    }
                    let Some(choice) = v.get("choices").and_then(|c| c.get(0)) else {
                        continue;
                    };
                    if let Some(fr) = choice.get("finish_reason").and_then(|x| x.as_str()) {
                        if fr != "null" && !fr.is_empty() {
                            finish = Some(fr.to_string());
                        }
                    }
                    let Some(delta) = choice.get("delta") else {
                        continue;
                    };
                    if let Some(t) = delta.get("content").and_then(|x| x.as_str()) {
                        if !t.is_empty() {
                            text.push_str(t);
                            let _ = tx.send(Ok(StreamEvent::TextDelta(t.to_string()))).await;
                        }
                    }
                    if let Some(reasoning) = delta.get("reasoning_content").and_then(|x| x.as_str())
                    {
                        if !reasoning.is_empty() {
                            let _ = tx
                                .send(Ok(StreamEvent::ThinkingDelta(reasoning.to_string())))
                                .await;
                        }
                    }
                    if let Some(calls) = delta.get("tool_calls").and_then(|x| x.as_array()) {
                        for call in calls {
                            let idx =
                                call.get("index").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
                            while tool_acc.len() <= idx {
                                tool_acc.push(ToolAcc::default());
                            }
                            let acc = &mut tool_acc[idx];
                            if let Some(id) = call.get("id").and_then(|x| x.as_str()) {
                                if acc.id.is_empty() {
                                    acc.id = id.to_string();
                                    let name = call
                                        .pointer("/function/name")
                                        .and_then(|x| x.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    acc.name = name.clone();
                                    let _ = tx
                                        .send(Ok(StreamEvent::ToolCallStart {
                                            id: acc.id.clone(),
                                            name,
                                        }))
                                        .await;
                                }
                            }
                            if let Some(name) =
                                call.pointer("/function/name").and_then(|x| x.as_str())
                            {
                                if acc.name.is_empty() {
                                    acc.name = name.to_string();
                                }
                            }
                            if let Some(args) =
                                call.pointer("/function/arguments").and_then(|x| x.as_str())
                            {
                                acc.arguments.push_str(args);
                                let _ = tx
                                    .send(Ok(StreamEvent::ToolCallDelta {
                                        id: acc.id.clone(),
                                        arguments_delta: args.to_string(),
                                    }))
                                    .await;
                            }
                        }
                    }
                }
            }

            let mut content = Vec::new();
            if !text.is_empty() {
                content.push(ContentBlock::text(text));
            }
            for acc in tool_acc {
                if acc.name.is_empty() {
                    continue;
                }
                let parsed: Value = serde_json::from_str(&acc.arguments).unwrap_or(json!({}));
                content.push(ContentBlock::ToolCall {
                    id: if acc.id.is_empty() {
                        uuid::Uuid::now_v7().to_string()
                    } else {
                        acc.id
                    },
                    name: acc.name,
                    arguments: parsed,
                    arguments_json: acc.arguments,
                });
            }
            let has_tools = content.iter().any(|c| matches!(c, ContentBlock::ToolCall { .. }));
            let stop = match finish.as_deref() {
                Some("tool_calls") | Some("function_call") => StopReason::ToolUse,
                Some("length") => StopReason::Length,
                _ if has_tools => StopReason::ToolUse,
                _ => StopReason::EndTurn,
            };
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

fn build_openai_body(request: &StreamRequest) -> Value {
    let mut messages = Vec::new();
    if let Some(sys) = &request.system {
        messages.push(json!({"role": "system", "content": sys}));
    }
    for msg in &request.messages {
        match msg {
            Message::System { content, .. } => {
                messages.push(json!({"role": "system", "content": content}));
            }
            Message::User { content, .. } => {
                messages.push(json!({"role": "user", "content": flatten_text(content)}));
            }
            Message::Assistant(asst) => {
                let mut tool_calls = Vec::new();
                let mut text = String::new();
                for block in &asst.content {
                    match block {
                        ContentBlock::Text { text: t } => text.push_str(t),
                        ContentBlock::ToolCall {
                            id,
                            name,
                            arguments,
                            arguments_json,
                        } => {
                            let args = if arguments_json.is_empty() {
                                arguments.to_string()
                            } else {
                                arguments_json.clone()
                            };
                            tool_calls.push(json!({
                                "id": id,
                                "type": "function",
                                "function": {"name": name, "arguments": args}
                            }));
                        }
                        _ => {}
                    }
                }
                let mut obj = serde_json::Map::new();
                obj.insert("role".into(), json!("assistant"));
                if !text.is_empty() {
                    obj.insert("content".into(), json!(text));
                } else {
                    obj.insert("content".into(), Value::Null);
                }
                if !tool_calls.is_empty() {
                    obj.insert("tool_calls".into(), Value::Array(tool_calls));
                }
                messages.push(Value::Object(obj));
            }
            Message::Tool {
                tool_call_id,
                content,
                ..
            } => {
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_call_id,
                    "content": flatten_text(content)
                }));
            }
        }
    }

    let mut body = json!({
        "model": request.model.id,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    if let Some(t) = request.options.temperature {
        body["temperature"] = json!(t);
    }
    if let Some(m) = request.options.max_tokens {
        body["max_tokens"] = json!(m);
    } else if request.model.max_output > 0 {
        body["max_tokens"] = json!(request.model.max_output.min(16_384));
    }
    if !request.tools.is_empty() {
        body["tools"] = Value::Array(request.tools.iter().map(tool_to_openai).collect());
    }
    body
}

fn tool_to_openai(tool: &ToolSpec) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": tool.name,
            "description": tool.description,
            "parameters": tool.parameters,
        }
    })
}

fn flatten_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| b.as_text())
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Default)]
struct ToolAcc {
    id: String,
    name: String,
    arguments: String,
}
