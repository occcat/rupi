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

pub struct GoogleProvider {
    api_key: Option<String>,
    base_url: String,
    client: Client,
    retry: RetryPolicy,
}

impl GoogleProvider {
    pub fn new(api_key: Option<String>, base_url: Option<String>) -> Self {
        let base_url = base_url
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| ProviderKind::Google.default_base_url().to_string())
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
impl Provider for GoogleProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Google
    }

    async fn stream(&self, request: StreamRequest) -> Result<EventStream, AiError> {
        let api_key = self
            .api_key
            .clone()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| AiError::MissingApiKey("google".into()))?;
        let body = build_google_body(&request);
        let url = format!(
            "{}/models/{}:streamGenerateContent?alt=sse&key={}",
            self.base_url, request.model.id, api_key
        );
        let client = self.client.clone();
        let retry = self.retry.clone();
        let model_id = request.model.id.clone();

        let response = retry_with_backoff(&retry, || {
            let client = client.clone();
            let url = url.clone();
            let body = body.clone();
            async move {
                let resp = client
                    .post(&url)
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
            let mut text = String::new();
            let mut tools: Vec<ContentBlock> = Vec::new();
            let mut usage = Usage::default();
            let mut finish = String::new();

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
                    if let Some(u) = v.get("usageMetadata") {
                        usage.input = u
                            .get("promptTokenCount")
                            .and_then(|x| x.as_u64())
                            .unwrap_or(0) as u32;
                        usage.output = u
                            .get("candidatesTokenCount")
                            .and_then(|x| x.as_u64())
                            .unwrap_or(0) as u32;
                    }
                    let Some(cand) = v.get("candidates").and_then(|c| c.get(0)) else {
                        continue;
                    };
                    if let Some(fr) = cand.get("finishReason").and_then(|x| x.as_str()) {
                        finish = fr.to_string();
                    }
                    let Some(parts) = cand.pointer("/content/parts").and_then(|x| x.as_array())
                    else {
                        continue;
                    };
                    for part in parts {
                        if let Some(t) = part.get("text").and_then(|x| x.as_str()) {
                            if !t.is_empty() {
                                text.push_str(t);
                                let _ = tx.send(Ok(StreamEvent::TextDelta(t.to_string()))).await;
                            }
                        }
                        if let Some(fc) = part.get("functionCall") {
                            let name = fc
                                .get("name")
                                .and_then(|x| x.as_str())
                                .unwrap_or("")
                                .to_string();
                            let args = fc.get("args").cloned().unwrap_or(json!({}));
                            let id = uuid::Uuid::now_v7().to_string();
                            let _ = tx
                                .send(Ok(StreamEvent::ToolCallStart {
                                    id: id.clone(),
                                    name: name.clone(),
                                }))
                                .await;
                            tools.push(ContentBlock::ToolCall {
                                id,
                                name,
                                arguments_json: args.to_string(),
                                arguments: args,
                            });
                        }
                    }
                }
            }

            let mut content = Vec::new();
            if !text.is_empty() {
                content.push(ContentBlock::text(text));
            }
            content.extend(tools);
            let has_tools = content.iter().any(|c| matches!(c, ContentBlock::ToolCall { .. }));
            let stop = if finish == "MAX_TOKENS" {
                StopReason::Length
            } else if has_tools {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
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

fn build_google_body(request: &StreamRequest) -> Value {
    let mut contents = Vec::new();
    let mut system = request.system.clone().unwrap_or_default();
    for msg in &request.messages {
        match msg {
            Message::System { content, .. } => {
                if !system.is_empty() {
                    system.push_str("\n\n");
                }
                system.push_str(content);
            }
            Message::User { content, .. } => {
                contents.push(json!({
                    "role": "user",
                    "parts": [{"text": flatten(content)}]
                }));
            }
            Message::Assistant(asst) => {
                let mut parts = Vec::new();
                for b in &asst.content {
                    match b {
                        ContentBlock::Text { text } => parts.push(json!({"text": text})),
                        ContentBlock::ToolCall {
                            name, arguments, ..
                        } => {
                            parts.push(json!({
                                "functionCall": {"name": name, "args": arguments}
                            }));
                        }
                        _ => {}
                    }
                }
                contents.push(json!({"role": "model", "parts": parts}));
            }
            Message::Tool {
                tool_name, content, ..
            } => {
                contents.push(json!({
                    "role": "user",
                    "parts": [{
                        "functionResponse": {
                            "name": tool_name,
                            "response": {"result": flatten(content)}
                        }
                    }]
                }));
            }
        }
    }

    let mut body = json!({
        "contents": contents,
        "generationConfig": {
            "maxOutputTokens": request.options.max_tokens.unwrap_or(8192),
        }
    });
    if !system.is_empty() {
        body["systemInstruction"] = json!({"parts": [{"text": system}]});
    }
    if !request.tools.is_empty() {
        body["tools"] = json!([{
            "functionDeclarations": request.tools.iter().map(tool_to_google).collect::<Vec<_>>()
        }]);
    }
    body
}

fn tool_to_google(tool: &ToolSpec) -> Value {
    json!({
        "name": tool.name,
        "description": tool.description,
        "parameters": tool.parameters,
    })
}

fn flatten(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| b.as_text())
        .collect::<Vec<_>>()
        .join("\n")
}
