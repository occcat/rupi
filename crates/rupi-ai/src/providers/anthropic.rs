use reqwest::Client;
use serde_json::{json, Value};

use crate::model::Model;
use crate::types::{
    AiError, AiResult, ContentBlock, Context, Message, StopReason, StreamOptions, ToolDefinition,
};
use crate::usage::Usage;

const DEFAULT_ANTHROPIC_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";

pub async fn anthropic_complete(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
) -> AiResult<Message> {
    let api_key = options
        .api_key
        .clone()
        .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok());
    let Some(api_key) = api_key else {
        return Ok(Message::error("missing ANTHROPIC_API_KEY"));
    };

    let base = options
        .base_url
        .as_deref()
        .or(model.base_url.as_deref())
        .unwrap_or(DEFAULT_ANTHROPIC_URL)
        .trim_end_matches('/');

    let (system, messages) = to_anthropic_messages(&context.messages, &context.system_prompt);
    let mut body = json!({
        "model": model.id,
        "max_tokens": options.max_tokens.unwrap_or(model.max_tokens.min(16_384)),
        "messages": messages,
    });
    if !system.is_empty() {
        body["system"] = json!(system);
    }
    if let Some(t) = options.temperature {
        body["temperature"] = json!(t);
    }
    if !context.tools.is_empty() {
        body["tools"] = json!(context.tools.iter().map(anthropic_tool).collect::<Vec<_>>());
    }

    let client = Client::new();
    let mut req = client
        .post(format!("{base}/v1/messages"))
        .header("x-api-key", api_key)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .json(&body);
    for (k, v) in &options.headers {
        req = req.header(k, v);
    }

    let resp = req.send().await.map_err(|e| AiError::Http(e.to_string()))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Ok(Message::error(format!("HTTP {status}: {text}")));
    }
    let payload: Value = resp.json().await.map_err(|e| AiError::Http(e.to_string()))?;
    Ok(parse_anthropic_message(&payload, model))
}

fn anthropic_tool(tool: &ToolDefinition) -> Value {
    json!({
        "name": tool.name,
        "description": tool.description,
        "input_schema": tool.parameters,
    })
}

fn to_anthropic_messages(messages: &[Message], system: &str) -> (String, Vec<Value>) {
    let mut out = Vec::new();
    for msg in messages {
        match msg {
            Message::User { content, .. } => {
                out.push(json!({
                    "role": "user",
                    "content": [{"type": "text", "text": crate::content_text(content)}],
                }));
            }
            Message::Assistant { content, .. } => {
                let mut blocks = Vec::new();
                for c in content {
                    match c {
                        ContentBlock::Text { text } => {
                            blocks.push(json!({"type": "text", "text": text}));
                        }
                        ContentBlock::Thinking { thinking, signature } => {
                            let mut b = json!({"type": "thinking", "thinking": thinking});
                            if let Some(sig) = signature {
                                b["signature"] = json!(sig);
                            }
                            blocks.push(b);
                        }
                        ContentBlock::ToolCall { id, name, arguments } => {
                            blocks.push(json!({
                                "type": "tool_use",
                                "id": id,
                                "name": name,
                                "input": arguments,
                            }));
                        }
                        ContentBlock::Image { .. } => {}
                    }
                }
                out.push(json!({"role": "assistant", "content": blocks}));
            }
            Message::ToolResult {
                tool_call_id,
                content,
                is_error,
                ..
            } => {
                // Anthropic requires tool_result as a user message.
                if let Some(Value::Object(_)) = out.last() {
                    if out.last().and_then(|v| v["role"].as_str()) == Some("user")
                        && out.last().and_then(|v| v["content"].as_array()).is_some()
                    {
                        if let Some(arr) = out.last_mut().and_then(|v| v["content"].as_array_mut()) {
                            arr.push(json!({
                                "type": "tool_result",
                                "tool_use_id": tool_call_id,
                                "content": crate::content_text(content),
                                "is_error": is_error,
                            }));
                            continue;
                        }
                    }
                }
                out.push(json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": tool_call_id,
                        "content": crate::content_text(content),
                        "is_error": is_error,
                    }],
                }));
            }
        }
    }
    (system.to_string(), out)
}

fn parse_anthropic_message(payload: &Value, model: &Model) -> Message {
    let mut content = Vec::new();
    if let Some(blocks) = payload["content"].as_array() {
        for b in blocks {
            match b["type"].as_str().unwrap_or("") {
                "text" => {
                    if let Some(t) = b["text"].as_str() {
                        content.push(ContentBlock::text(t));
                    }
                }
                "thinking" => {
                    content.push(ContentBlock::Thinking {
                        thinking: b["thinking"].as_str().unwrap_or("").to_string(),
                        signature: b["signature"].as_str().map(|s| s.to_string()),
                    });
                }
                "tool_use" => {
                    content.push(ContentBlock::tool_call(
                        b["id"].as_str().unwrap_or("call"),
                        b["name"].as_str().unwrap_or("unknown"),
                        b["input"].clone(),
                    ));
                }
                _ => {}
            }
        }
    }
    let stop = match payload["stop_reason"].as_str().unwrap_or("end_turn") {
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::Length,
        _ => {
            if content.iter().any(|c| matches!(c, ContentBlock::ToolCall { .. })) {
                StopReason::ToolUse
            } else {
                StopReason::Stop
            }
        }
    };
    let usage = Usage {
        input: payload["usage"]["input_tokens"].as_u64().unwrap_or(0) as u32,
        output: payload["usage"]["output_tokens"].as_u64().unwrap_or(0) as u32,
        cache_read: payload["usage"]["cache_read_input_tokens"]
            .as_u64()
            .unwrap_or(0) as u32,
        cache_write: payload["usage"]["cache_creation_input_tokens"]
            .as_u64()
            .unwrap_or(0) as u32,
        total_cost: 0,
    };
    Message::Assistant {
        content,
        stop_reason: stop,
        usage,
        error_message: None,
        timestamp: crate::types::now_ms(),
        model: Some(model.id.clone()),
        provider: Some(model.provider.clone()),
    }
}
