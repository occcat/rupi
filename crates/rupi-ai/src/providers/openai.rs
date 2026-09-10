use reqwest::Client;
use serde_json::{json, Value};

use crate::model::Model;
use crate::types::{
    AiError, AiResult, ContentBlock, Context, Message, StopReason, StreamOptions, ToolDefinition,
};
use crate::usage::Usage;

const DEFAULT_OPENAI_URL: &str = "https://api.openai.com/v1";

pub async fn openai_complete(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
) -> AiResult<Message> {
    let api_key = options
        .api_key
        .clone()
        .or_else(|| std::env::var("OPENAI_API_KEY").ok())
        .or_else(|| std::env::var("RUPI_API_KEY").ok());
    let Some(api_key) = api_key else {
        return Ok(Message::error("missing API key for OpenAI-compatible provider"));
    };

    let base = options
        .base_url
        .as_deref()
        .or(model.base_url.as_deref())
        .unwrap_or(DEFAULT_OPENAI_URL)
        .trim_end_matches('/');

    let mut messages = Vec::new();
    if !context.system_prompt.is_empty() {
        messages.push(json!({"role": "system", "content": context.system_prompt}));
    }
    append_openai_messages(&mut messages, &context.messages);

    let mut body = json!({
        "model": model.id,
        "messages": messages,
    });
    if let Some(t) = options.temperature {
        body["temperature"] = json!(t);
    }
    if let Some(max) = options.max_tokens {
        body["max_tokens"] = json!(max);
    } else {
        body["max_tokens"] = json!(model.max_tokens.min(16_384));
    }
    if !context.tools.is_empty() {
        body["tools"] = json!(context
            .tools
            .iter()
            .map(openai_tool)
            .collect::<Vec<_>>());
        body["tool_choice"] = json!("auto");
    }

    let client = Client::new();
    let mut req = client
        .post(format!("{base}/chat/completions"))
        .bearer_auth(api_key)
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
    Ok(parse_openai_message(&payload, model))
}

fn openai_tool(tool: &ToolDefinition) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": tool.name,
            "description": tool.description,
            "parameters": tool.parameters,
        }
    })
}

fn append_openai_messages(out: &mut Vec<Value>, messages: &[Message]) {
    for msg in messages {
        match msg {
            Message::User { content, .. } => {
                out.push(json!({
                    "role": "user",
                    "content": crate::content_text(content),
                }));
            }
            Message::Assistant { content, .. } => {
                let text = crate::content_text(content);
                let tool_calls: Vec<Value> = content
                    .iter()
                    .filter_map(|c| match c {
                        ContentBlock::ToolCall { id, name, arguments } => Some(json!({
                            "id": id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": arguments.to_string(),
                            }
                        })),
                        _ => None,
                    })
                    .collect();
                let mut obj = json!({"role": "assistant", "content": if text.is_empty() { Value::Null } else { json!(text) }});
                if !tool_calls.is_empty() {
                    obj["tool_calls"] = json!(tool_calls);
                }
                out.push(obj);
            }
            Message::ToolResult {
                tool_call_id,
                content,
                ..
            } => {
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_call_id,
                    "content": crate::content_text(content),
                }));
            }
        }
    }
}

fn parse_openai_message(payload: &Value, model: &Model) -> Message {
    let choice = &payload["choices"][0];
    let msg = &choice["message"];
    let mut content = Vec::new();
    if let Some(text) = msg["content"].as_str() {
        if !text.is_empty() {
            content.push(ContentBlock::text(text));
        }
    }
    if let Some(thinking) = msg["reasoning_content"].as_str().filter(|s| !s.is_empty()) {
        content.insert(
            0,
            ContentBlock::Thinking {
                thinking: thinking.to_string(),
                signature: None,
            },
        );
    }
    let mut stop = StopReason::Stop;
    if let Some(calls) = msg["tool_calls"].as_array() {
        for call in calls {
            let id = call["id"].as_str().unwrap_or("call").to_string();
            let name = call["function"]["name"].as_str().unwrap_or("unknown").to_string();
            let args_raw = call["function"]["arguments"].as_str().unwrap_or("{}");
            let arguments = serde_json::from_str(args_raw).unwrap_or(json!({}));
            content.push(ContentBlock::tool_call(id, name, arguments));
        }
        if calls.iter().any(|c| c["function"]["name"].is_string()) {
            stop = StopReason::ToolUse;
        }
    }
    let finish = choice["finish_reason"].as_str().unwrap_or("stop");
    if finish == "length" {
        stop = StopReason::Length;
    }
    let usage = Usage {
        input: payload["usage"]["prompt_tokens"].as_u64().unwrap_or(0) as u32,
        output: payload["usage"]["completion_tokens"].as_u64().unwrap_or(0) as u32,
        ..Default::default()
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
