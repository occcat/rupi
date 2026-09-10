//! Anthropic Messages API 原生 provider（对标 Pi 的 Anthropic 接入）。
//!
//! 与 OpenAI 格式的三处关键差异：system 独立参数、Tool 结果走 user 角色的
//! `tool_result` 内容项、消息必须严格 user/assistant 交替（同角色相邻合并）。

use async_trait::async_trait;
use rupi_core::{ContentBlock, Message, Role, ToolDefinition};

pub const ANTHROPIC_VERSION: &str = "2023-06-01";
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    client: reqwest::Client,
}

impl AnthropicProvider {
    pub fn new(base_url: String, api_key: String, model: String) -> Self {
        Self {
            base_url,
            api_key,
            model,
            client: reqwest::Client::new(),
        }
    }

    pub fn from_env(model: String) -> anyhow::Result<Self> {
        let api_key = std::env::var("RUPI_ANTHROPIC_KEY")
            .or_else(|_| std::env::var("ANTHROPIC_API_KEY"))
            .map_err(|_| anyhow::anyhow!("set RUPI_ANTHROPIC_KEY or ANTHROPIC_API_KEY"))?;
        let base_url =
            std::env::var("RUPI_ANTHROPIC_BASE").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        Ok(Self::new(base_url, api_key, model))
    }

    fn headers(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        req.header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
    }

    fn body(&self, req: &super::ChatRequest, stream: bool) -> serde_json::Value {
        // prompt caching：system 与末工具挂 ephemeral 断点（Anthropic 按前缀缓存计费），
        // 无工具时省略 tools 字段（空数组会 400）。
        let mut tools = to_anthropic_tools(&req.tools);
        if let Some(last) = tools.last_mut() {
            last["cache_control"] = serde_json::json!({"type": "ephemeral"});
        }
        let mut m = serde_json::Map::new();
        m.insert("model".into(), self.model.clone().into());
        m.insert("max_tokens".into(), req.max_tokens.unwrap_or(4096).into());
        m.insert(
            "system".into(),
            serde_json::json!([{"type": "text", "text": req.system,
                "cache_control": {"type": "ephemeral"}}]),
        );
        m.insert(
            "messages".into(),
            serde_json::Value::Array(to_anthropic_messages(&req.messages)),
        );
        if !tools.is_empty() {
            m.insert("tools".into(), serde_json::Value::Array(tools));
        }
        // extended thinking 开启时 temperature 必须为 1（API 硬性要求），且
        // max_tokens 必须大于 budget；不满足任一条即省略 thinking（退化为普通请求，
        // 否则 400）。思考块签名由解析/累积器保留，多轮工具流原样回放。
        let budget = req.thinking.and_then(|t| t.anthropic_budget());
        let thinking_on = budget.is_some_and(|b| req.max_tokens.unwrap_or(4096) > b);
        if thinking_on {
            m.insert(
                "thinking".into(),
                serde_json::json!({"type": "enabled", "budget_tokens": budget.unwrap()}),
            );
        }
        m.insert(
            "temperature".into(),
            if thinking_on {
                1.0
            } else {
                req.temperature.unwrap_or(0.2)
            }
            .into(),
        );
        m.insert("stream".into(), stream.into());
        serde_json::Value::Object(m)
    }
}

/// 内部消息 → Anthropic messages（同角色相邻合并，保证严格交替）。
pub fn to_anthropic_messages(messages: &[Message]) -> Vec<serde_json::Value> {
    let mut out: Vec<serde_json::Value> = vec![];
    let mut push = |role: &str, items: Vec<serde_json::Value>| {
        if items.is_empty() {
            return;
        }
        if let Some(last) = out.last_mut() {
            if last.get("role").and_then(|r| r.as_str()) == Some(role) {
                if let Some(a) = last.get_mut("content").and_then(|c| c.as_array_mut()) {
                    a.extend(items);
                    return;
                }
            }
        }
        out.push(serde_json::json!({"role": role, "content": items}));
    };
    for m in messages {
        match m.role {
            // system 会话消息极少见，降级为 user 文本（Anthropic 只认独立 system 参数）
            Role::System | Role::User => {
                let t = m.full_text();
                if !t.is_empty() {
                    push("user", vec![serde_json::json!({"type": "text", "text": t})]);
                }
            }
            Role::Assistant => {
                let mut items = vec![];
                for b in &m.blocks {
                    match b {
                        ContentBlock::Text { text } => {
                            if !text.is_empty() {
                                items.push(serde_json::json!({"type": "text", "text": text}));
                            }
                        }
                        ContentBlock::ToolCall {
                            id,
                            name,
                            arguments,
                        } => items.push(serde_json::json!({
                            "type": "tool_use", "id": id, "name": name, "input": arguments,
                        })),
                        // 思考块按原序回放：thinking 必须排在同消息 tool_use 之前
                        // （调用方 finish() 已保证顺序），signature 缺失即便如此也照发，
                        // 丢签名的降级由服务端判，本地不静默吞块。
                        ContentBlock::Thinking { text, signature } => {
                            let mut item = serde_json::json!({
                                "type": "thinking", "thinking": text,
                            });
                            if let Some(sig) = signature {
                                item["signature"] = sig.clone().into();
                            }
                            items.push(item);
                        }
                        ContentBlock::RedactedThinking { data } => {
                            items.push(serde_json::json!({
                                "type": "redacted_thinking", "data": data,
                            }));
                        }
                        _ => {}
                    }
                }
                push("assistant", items);
            }
            Role::Tool => {
                let mut items = vec![];
                for b in &m.blocks {
                    if let ContentBlock::ToolResult {
                        tool_call_id,
                        content,
                        is_error,
                    } = b
                    {
                        let mut item = serde_json::json!({
                            "type": "tool_result",
                            "tool_use_id": tool_call_id,
                            "content": [{"type": "text", "text": content}],
                        });
                        if *is_error {
                            item["is_error"] = true.into();
                        }
                        items.push(item);
                    }
                }
                push("user", items);
            }
        }
    }
    out
}

pub fn to_anthropic_tools(tools: &[ToolDefinition]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.input_schema,
            })
        })
        .collect()
}

/// 错误回包提消息（`{error: {message}}`），调用方拼状态码。
pub fn error_text(v: &serde_json::Value) -> Option<&str> {
    v.pointer("/error/message").and_then(|s| s.as_str())
}

pub fn parse_anthropic_response(v: serde_json::Value) -> anyhow::Result<super::ChatResponse> {
    if error_text(&v).is_some() {
        anyhow::bail!("anthropic error: {}", v.pointer("/error/message").unwrap());
    }
    let content = v
        .get("content")
        .and_then(|c| c.as_array())
        .ok_or_else(|| anyhow::anyhow!("bad anthropic response: {v}"))?;
    let mut blocks = vec![];
    for item in content {
        match item.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                let text = item.get("text").and_then(|t| t.as_str()).unwrap_or("");
                if !text.is_empty() {
                    blocks.push(ContentBlock::Text {
                        text: text.to_owned(),
                    });
                }
            }
            Some("tool_use") => blocks.push(ContentBlock::ToolCall {
                id: item
                    .get("id")
                    .and_then(|s| s.as_str())
                    .unwrap_or("call-0")
                    .to_string(),
                name: item
                    .get("name")
                    .and_then(|s| s.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
                arguments: item.get("input").cloned().unwrap_or(serde_json::json!({})),
            }),
            // extended-thinking：明文块留 signature 做回放凭证，加密块留 data 原样回放
            Some("thinking") => blocks.push(ContentBlock::Thinking {
                text: item
                    .get("thinking")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string(),
                signature: item
                    .get("signature")
                    .and_then(|s| s.as_str())
                    .map(str::to_owned),
            }),
            Some("redacted_thinking") => blocks.push(ContentBlock::RedactedThinking {
                data: item
                    .get("data")
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .to_string(),
            }),
            _ => {}
        }
    }
    if blocks.is_empty() {
        blocks.push(ContentBlock::Text {
            text: String::new(),
        });
    }
    Ok(super::ChatResponse {
        message: Message {
            id: uuid::Uuid::new_v4().to_string(),
            role: Role::Assistant,
            blocks,
            provider: Some("anthropic".into()),
            created_at: chrono::Utc::now(),
        },
        stop_reason: v
            .get("stop_reason")
            .and_then(|s| s.as_str())
            .unwrap_or("end_turn")
            .to_string(),
    })
}

/// Anthropic SSE 累积器（纯逻辑，可单测）：text 增量直推，tool_use 按 index 拼 input_json，
/// thinking 按 index 攒明文 + signature（静默累积，不进 TextDelta 通道）。
#[derive(Debug, Default)]
pub struct AnthropicAccumulator {
    text: String,
    frags: Vec<Frag>,
    pub stop_reason: Option<String>,
}

#[derive(Debug, Default, Clone)]
struct Frag {
    is_tool: bool,
    is_thinking: bool,
    redacted_data: Option<String>,
    signature: String,
    id: String,
    name: String,
    buf: String,
}

impl AnthropicAccumulator {
    fn slot(&mut self, index: usize) -> &mut Frag {
        while self.frags.len() <= index {
            self.frags.push(Frag::default());
        }
        &mut self.frags[index]
    }

    pub async fn apply_event(
        &mut self,
        event: &str,
        data: &serde_json::Value,
        tx: &tokio::sync::mpsc::Sender<super::StreamEvent>,
    ) {
        match event {
            "content_block_start" => {
                let index = data.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                let block = data.get("content_block");
                let kind = block
                    .and_then(|b| b.get("type"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("text");
                let slot = self.slot(index);
                if kind == "tool_use" {
                    slot.is_tool = true;
                    slot.id = block
                        .and_then(|b| b.get("id"))
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string();
                    slot.name = block
                        .and_then(|b| b.get("name"))
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string();
                } else if kind == "thinking" {
                    slot.is_thinking = true;
                } else if kind == "redacted_thinking" {
                    // 加密块流式只在 start 里给全量 data，无后续 delta
                    slot.is_thinking = true;
                    slot.redacted_data = block
                        .and_then(|b| b.get("data"))
                        .and_then(|d| d.as_str())
                        .map(str::to_owned);
                }
            }
            "content_block_delta" => {
                let index = data.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                let delta = match data.get("delta") {
                    Some(d) => d,
                    None => return,
                };
                match delta.get("type").and_then(|t| t.as_str()) {
                    Some("text_delta") => {
                        let t = delta.get("text").and_then(|s| s.as_str()).unwrap_or("");
                        if !t.is_empty() {
                            self.text.push_str(t);
                            self.slot(index).buf.push_str(t);
                            let _ = tx.send(super::StreamEvent::TextDelta(t.to_string())).await;
                        }
                    }
                    Some("input_json_delta") => {
                        let p = delta
                            .get("partial_json")
                            .and_then(|s| s.as_str())
                            .unwrap_or("");
                        self.slot(index).buf.push_str(p);
                    }
                    // thinking 明文静默攒块（不进回答文本通道），signature 另攒做回放凭证
                    Some("thinking_delta") => {
                        let t = delta.get("thinking").and_then(|s| s.as_str()).unwrap_or("");
                        let slot = self.slot(index);
                        slot.is_thinking = true;
                        slot.buf.push_str(t);
                    }
                    Some("signature_delta") => {
                        let s = delta
                            .get("signature")
                            .and_then(|s| s.as_str())
                            .unwrap_or("");
                        let slot = self.slot(index);
                        slot.is_thinking = true;
                        slot.signature.push_str(s);
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(s) = data.pointer("/delta/stop_reason").and_then(|s| s.as_str()) {
                    self.stop_reason = Some(s.to_string());
                }
            }
            _ => {}
        }
    }

    pub fn finish(self, default_stop: &str) -> super::ChatResponse {
        let mut blocks = vec![];
        // 思考块排最前：同消息内 thinking 必须先于 tool_use（API 硬性顺序），文本位置自由
        for f in &self.frags {
            if !f.is_thinking {
                continue;
            }
            if let Some(data) = &f.redacted_data {
                blocks.push(ContentBlock::RedactedThinking { data: data.clone() });
            } else if !f.buf.is_empty() || !f.signature.is_empty() {
                blocks.push(ContentBlock::Thinking {
                    text: f.buf.clone(),
                    signature: if f.signature.is_empty() {
                        None
                    } else {
                        Some(f.signature.clone())
                    },
                });
            }
        }
        if !self.text.is_empty() {
            blocks.push(ContentBlock::Text { text: self.text });
        }
        for (i, f) in self.frags.into_iter().enumerate() {
            if !f.is_tool {
                continue;
            }
            blocks.push(ContentBlock::ToolCall {
                id: if f.id.is_empty() {
                    format!("call-{i}")
                } else {
                    f.id
                },
                name: if f.name.is_empty() {
                    "unknown".into()
                } else {
                    f.name
                },
                arguments: serde_json::from_str(&f.buf).unwrap_or(serde_json::json!({})),
            });
        }
        if blocks.is_empty() {
            blocks.push(ContentBlock::Text {
                text: String::new(),
            });
        }
        super::ChatResponse {
            message: Message {
                id: uuid::Uuid::new_v4().to_string(),
                role: Role::Assistant,
                blocks,
                provider: Some("anthropic".into()),
                created_at: chrono::Utc::now(),
            },
            stop_reason: self.stop_reason.unwrap_or_else(|| default_stop.to_string()),
        }
    }
}

#[async_trait]
impl super::LlmProvider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    async fn complete(&self, req: super::ChatRequest) -> anyhow::Result<super::ChatResponse> {
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let body = self.body(&req, false);
        let resp =
            super::post_json_with_retry(|| self.headers(self.client.post(&url)), &body, 3).await?;
        let status = resp.status();
        let v: serde_json::Value = resp.json().await?;
        if !status.is_success() {
            let msg = error_text(&v).unwrap_or("anthropic request failed");
            anyhow::bail!("anthropic {status}: {msg}");
        }
        parse_anthropic_response(v)
    }

    /// 真 SSE 流：`event:` + `data:` 配对，文本直推、tool_use 内部累积。
    async fn complete_streaming(
        &self,
        req: super::ChatRequest,
        tx: tokio::sync::mpsc::Sender<super::StreamEvent>,
    ) -> anyhow::Result<super::ChatResponse> {
        use futures::StreamExt as _;
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let body = self.body(&req, true);
        let resp =
            super::post_json_with_retry(|| self.headers(self.client.post(&url)), &body, 3).await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let v: serde_json::Value = resp.json().await.unwrap_or(serde_json::json!({}));
            let msg = error_text(&v).unwrap_or("anthropic request failed");
            anyhow::bail!("anthropic {status}: {msg}");
        }
        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        let mut event = String::new();
        let mut acc = AnthropicAccumulator::default();
        'stream: while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buf.find('\n') {
                let line = buf[..pos].trim().to_string();
                buf = buf[pos + 1..].to_string();
                if line.is_empty() || line.starts_with(':') {
                    continue;
                }
                if let Some(name) = line.strip_prefix("event:").map(str::trim) {
                    event = name.to_string();
                    if event == "message_stop" {
                        break 'stream;
                    }
                    continue;
                }
                let Some(data) = line.strip_prefix("data:").map(str::trim) else {
                    continue;
                };
                if event.is_empty() {
                    continue;
                }
                let v: serde_json::Value = serde_json::from_str(data)?;
                acc.apply_event(&event, &v, &tx).await;
            }
        }
        Ok(acc.finish("end_turn"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rupi_core::Message;

    fn assistant_tool_msg() -> Message {
        Message {
            id: "a".into(),
            role: Role::Assistant,
            blocks: vec![
                ContentBlock::Text {
                    text: "let me check".into(),
                },
                ContentBlock::ToolCall {
                    id: "tu1".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path": "a.rs"}),
                },
            ],
            provider: None,
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn body_sets_cache_breakpoints_and_omits_empty_tools() {
        use rupi_core::ToolDefinition;
        let p = AnthropicProvider::new("https://x".into(), "k".into(), "m".into());
        let tool = |n: &str| ToolDefinition {
            name: n.into(),
            description: "d".into(),
            input_schema: serde_json::json!({"type": "object"}),
            prompt_snippet: None,
        };
        let req = super::super::ChatRequest {
            system: "sys".into(),
            messages: vec![],
            tools: vec![tool("a"), tool("b")],
            max_tokens: None,
            temperature: None,
            thinking: None,
        };
        let b = p.body(&req, false);
        // system 数组挂 ephemeral
        assert_eq!(
            b.pointer("/system/0/cache_control/type"),
            Some(&serde_json::json!("ephemeral"))
        );
        assert_eq!(b.pointer("/system/0/text"), Some(&serde_json::json!("sys")));
        // 仅末工具挂断点
        assert_eq!(
            b.pointer("/tools/1/cache_control/type"),
            Some(&serde_json::json!("ephemeral"))
        );
        assert!(b.pointer("/tools/0/cache_control").is_none());
        // 无工具时省略字段（空数组会 400）
        let empty = super::super::ChatRequest {
            tools: vec![],
            ..req
        };
        assert!(p.body(&empty, false).get("tools").is_none());
    }

    #[test]
    fn body_maps_thinking_budget_and_forces_temperature() {
        let p = AnthropicProvider::new("https://x".into(), "k".into(), "m".into());
        let base = || super::super::ChatRequest {
            system: "sys".into(),
            messages: vec![],
            tools: vec![],
            max_tokens: None,
            temperature: Some(0.2),
            thinking: None,
        };
        // 默认关闭：无 thinking 字段，温度保持原值
        let b = p.body(&base(), false);
        assert!(b.get("thinking").is_none());
        let temp = b["temperature"].as_f64().unwrap();
        assert!((temp - 0.2).abs() < 1e-6, "temperature passthrough");
        // Medium + 足够 max_tokens：thinking 启用，温度强制 1
        let mut med = base();
        med.thinking = Some(super::super::ThinkingLevel::Medium);
        med.max_tokens = Some(8192);
        let b = p.body(&med, false);
        assert_eq!(b["thinking"]["budget_tokens"], 4096);
        assert_eq!(b["temperature"], 1.0);
        // max_tokens 不大于 budget：省略 thinking（否则 API 400），温度不变
        let mut tight = base();
        tight.thinking = Some(super::super::ThinkingLevel::High);
        tight.max_tokens = Some(4096);
        let b = p.body(&tight, false);
        assert!(b.get("thinking").is_none());
        let temp = b["temperature"].as_f64().unwrap();
        assert!((temp - 0.2).abs() < 1e-6, "temperature untouched");
    }

    #[test]
    fn maps_roles_and_merges_adjacent_users() {
        let tool_msg = Message {
            id: "t".into(),
            role: Role::Tool,
            blocks: vec![ContentBlock::ToolResult {
                tool_call_id: "tu1".into(),
                content: "fn main() {}".into(),
                is_error: false,
            }],
            provider: None,
            created_at: chrono::Utc::now(),
        };
        let msgs = vec![
            Message::text(Role::User, "read a.rs"),
            assistant_tool_msg(),
            tool_msg,
        ];
        let out = to_anthropic_messages(&msgs);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0]["role"], "user");
        assert_eq!(out[1]["role"], "assistant");
        assert_eq!(out[1]["content"][1]["type"], "tool_use");
        assert_eq!(out[1]["content"][1]["input"]["path"], "a.rs");
        // tool 结果并入 user 角色
        assert_eq!(out[2]["role"], "user");
        assert_eq!(out[2]["content"][0]["type"], "tool_result");
        assert_eq!(out[2]["content"][0]["tool_use_id"], "tu1");
    }

    #[test]
    fn merges_consecutive_same_role() {
        let msgs = vec![
            Message::text(Role::User, "one"),
            Message {
                id: "t".into(),
                role: Role::Tool,
                blocks: vec![ContentBlock::ToolResult {
                    tool_call_id: "x".into(),
                    content: "two".into(),
                    is_error: true,
                }],
                provider: None,
                created_at: chrono::Utc::now(),
            },
        ];
        let out = to_anthropic_messages(&msgs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["content"].as_array().unwrap().len(), 2);
        assert_eq!(out[0]["content"][1]["is_error"], true);
    }

    #[test]
    fn parses_thinking_and_redacted_blocks() {
        let v = serde_json::json!({
            "content": [
                {"type": "thinking", "thinking": "let me reason", "signature": "sig1"},
                {"type": "redacted_thinking", "data": "enc9"},
                {"type": "text", "text": "checking"},
                {"type": "tool_use", "id": "tu9", "name": "bash",
                 "input": {"command": "ls"}},
            ],
            "stop_reason": "tool_use",
        });
        let r = parse_anthropic_response(v).unwrap();
        assert_eq!(r.message.blocks.len(), 4);
        match &r.message.blocks[0] {
            ContentBlock::Thinking { text, signature } => {
                assert_eq!(text, "let me reason");
                assert_eq!(signature.as_deref(), Some("sig1"));
            }
            _ => panic!("expected thinking"),
        }
        match &r.message.blocks[1] {
            ContentBlock::RedactedThinking { data } => assert_eq!(data, "enc9"),
            _ => panic!("expected redacted"),
        }
    }

    #[test]
    fn replays_thinking_before_tool_use() {
        let msgs = vec![Message {
            id: "a".into(),
            role: Role::Assistant,
            blocks: vec![
                ContentBlock::Thinking {
                    text: "hmm".into(),
                    signature: Some("sig1".into()),
                },
                ContentBlock::RedactedThinking {
                    data: "enc9".into(),
                },
                ContentBlock::ToolCall {
                    id: "tu1".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({}),
                },
            ],
            provider: None,
            created_at: chrono::Utc::now(),
        }];
        let out = to_anthropic_messages(&msgs);
        assert_eq!(out.len(), 1);
        let content = out[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 3);
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["signature"], "sig1");
        assert_eq!(content[1]["type"], "redacted_thinking");
        assert_eq!(content[1]["data"], "enc9");
        assert_eq!(content[2]["type"], "tool_use");
    }

    #[test]
    fn parses_text_and_tool_use() {
        let v = serde_json::json!({
            "id": "msg_1",
            "content": [
                {"type": "text", "text": "checking"},
                {"type": "tool_use", "id": "tu9", "name": "bash",
                 "input": {"command": "ls"}},
            ],
            "stop_reason": "tool_use",
        });
        let r = parse_anthropic_response(v).unwrap();
        assert_eq!(r.stop_reason, "tool_use");
        assert_eq!(r.message.blocks.len(), 2);
        match &r.message.blocks[1] {
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
            } => {
                assert_eq!(id, "tu9");
                assert_eq!(name, "bash");
                assert_eq!(arguments["command"], "ls");
            }
            _ => panic!("expected tool call"),
        }
    }

    #[test]
    fn error_payload_is_surfaced() {
        let v = serde_json::json!({"type": "error",
            "error": {"type": "invalid_request", "message": "bad key"}});
        assert_eq!(error_text(&v), Some("bad key"));
        assert!(parse_anthropic_response(v).is_err());
    }

    #[tokio::test]
    async fn accumulator_assembles_text_and_tool_use() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let mut acc = AnthropicAccumulator::default();
        async fn feed(
            acc: &mut AnthropicAccumulator,
            tx: &tokio::sync::mpsc::Sender<super::super::StreamEvent>,
            e: &str,
            d: serde_json::Value,
        ) {
            acc.apply_event(e, &d, tx).await;
        }
        feed(
            &mut acc,
            &tx,
            "content_block_start",
            serde_json::json!({"index": 0,
            "content_block": {"type": "text"}}),
        )
        .await;
        feed(
            &mut acc,
            &tx,
            "content_block_delta",
            serde_json::json!({"index": 0,
            "delta": {"type": "text_delta", "text": "hi"}}),
        )
        .await;
        feed(
            &mut acc,
            &tx,
            "content_block_start",
            serde_json::json!({"index": 1,
            "content_block": {"type": "tool_use", "id": "tu1", "name": "read"}}),
        )
        .await;
        feed(
            &mut acc,
            &tx,
            "content_block_delta",
            serde_json::json!({"index": 1,
            "delta": {"type": "input_json_delta", "partial_json": "{\"path\":"}}),
        )
        .await;
        feed(
            &mut acc,
            &tx,
            "content_block_delta",
            serde_json::json!({"index": 1,
            "delta": {"type": "input_json_delta", "partial_json": "\"a.rs\"}"}}),
        )
        .await;
        feed(
            &mut acc,
            &tx,
            "message_delta",
            serde_json::json!({"delta": {"stop_reason": "tool_use"}}),
        )
        .await;
        drop(tx);
        let mut deltas = vec![];
        while let Some(d) = rx.recv().await {
            deltas.push(d);
        }
        // 只有文本进 delta 通道
        assert_eq!(deltas.len(), 1);
        let r = acc.finish("end_turn");
        assert_eq!(r.stop_reason, "tool_use");
        assert_eq!(r.message.blocks.len(), 2);
        match &r.message.blocks[1] {
            ContentBlock::ToolCall { arguments, .. } => {
                assert_eq!(arguments["path"], "a.rs");
            }
            _ => panic!("expected tool call"),
        }
    }

    #[tokio::test]
    async fn accumulator_assembles_thinking_with_signature_first() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let mut acc = AnthropicAccumulator::default();
        async fn feed(
            acc: &mut AnthropicAccumulator,
            tx: &tokio::sync::mpsc::Sender<super::super::StreamEvent>,
            e: &str,
            d: serde_json::Value,
        ) {
            acc.apply_event(e, &d, tx).await;
        }
        // 加密块 start 自带全量 data
        feed(
            &mut acc,
            &tx,
            "content_block_start",
            serde_json::json!({"index": 0,
            "content_block": {"type": "redacted_thinking", "data": "enc0"}}),
        )
        .await;
        feed(
            &mut acc,
            &tx,
            "content_block_start",
            serde_json::json!({"index": 1,
            "content_block": {"type": "thinking"}}),
        )
        .await;
        feed(
            &mut acc,
            &tx,
            "content_block_delta",
            serde_json::json!({"index": 1,
            "delta": {"type": "thinking_delta", "thinking": "reason "}}),
        )
        .await;
        feed(
            &mut acc,
            &tx,
            "content_block_delta",
            serde_json::json!({"index": 1,
            "delta": {"type": "thinking_delta", "thinking": "more"}}),
        )
        .await;
        feed(
            &mut acc,
            &tx,
            "content_block_delta",
            serde_json::json!({"index": 1,
            "delta": {"type": "signature_delta", "signature": "sigX"}}),
        )
        .await;
        // 回答文本照常走 delta 通道，思考文本不进
        feed(
            &mut acc,
            &tx,
            "content_block_delta",
            serde_json::json!({"index": 2,
            "delta": {"type": "text_delta", "text": "hi"}}),
        )
        .await;
        drop(tx);
        let mut deltas = vec![];
        while let Some(d) = rx.recv().await {
            deltas.push(d);
        }
        assert_eq!(deltas.len(), 1);
        let r = acc.finish("end_turn");
        assert_eq!(r.message.blocks.len(), 3);
        match &r.message.blocks[0] {
            ContentBlock::RedactedThinking { data } => assert_eq!(data, "enc0"),
            _ => panic!("expected redacted first"),
        }
        match &r.message.blocks[1] {
            ContentBlock::Thinking { text, signature } => {
                assert_eq!(text, "reason more");
                assert_eq!(signature.as_deref(), Some("sigX"));
            }
            _ => panic!("expected thinking"),
        }
        // 思考文本不污染回答与 transcript 文本口径
        assert!(r.message.full_text().contains("[thinking] reason more"));
    }
}
