//! Google Gemini (generateContent) 原生 provider（Pi 三家之三）。
//!
//! 与 Anthropic 的三处关键差异：角色叫 `user`/`model`（system 独立 `system_instruction`）、
//! 工具调用是 `functionCall`/`functionResponse` 内容项、鉴权走 `x-goog-api-key` 请求头。

use async_trait::async_trait;
use rupi_core::{ContentBlock, Message, Role, ToolDefinition};

pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";

#[derive(Debug, Clone)]
pub struct GeminiProvider {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    client: reqwest::Client,
}

impl GeminiProvider {
    pub fn new(base_url: String, api_key: String, model: String) -> Self {
        Self {
            base_url,
            api_key,
            model,
            client: reqwest::Client::new(),
        }
    }

    pub fn from_env(model: String) -> anyhow::Result<Self> {
        let api_key = std::env::var("RUPI_GEMINI_KEY")
            .or_else(|_| std::env::var("GEMINI_API_KEY"))
            .or_else(|_| std::env::var("GOOGLE_API_KEY"))
            .map_err(|_| anyhow::anyhow!("set RUPI_GEMINI_KEY or GEMINI_API_KEY"))?;
        let base_url =
            std::env::var("RUPI_GEMINI_BASE").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        Ok(Self::new(base_url, api_key, model))
    }

    fn url(&self, stream: bool) -> String {
        let verb = if stream {
            "streamGenerateContent"
        } else {
            "generateContent"
        };
        format!(
            "{}/v1beta/models/{}:{verb}",
            self.base_url.trim_end_matches('/'),
            self.model
        )
    }

    fn body(&self, req: &super::ChatRequest) -> serde_json::Value {
        serde_json::json!({
            "system_instruction": {"parts": [{"text": req.system}]},
            "contents": to_gemini_contents(&req.messages),
            "tools": [{"functionDeclarations": to_gemini_tools(&req.tools)}],
            "generationConfig": {
                "temperature": req.temperature.unwrap_or(0.2),
                "maxOutputTokens": req.max_tokens.unwrap_or(4096),
            },
        })
    }
}

/// 内部消息 → Gemini contents（同角色相邻合并；assistant→model，tool 结果→user 的 functionResponse）。
pub fn to_gemini_contents(messages: &[Message]) -> Vec<serde_json::Value> {
    // functionResponse 配对要 name：先扫全历史建 tool_call_id → name 表。
    let mut names: std::collections::HashMap<&str, &str> = Default::default();
    for m in messages {
        for b in &m.blocks {
            if let ContentBlock::ToolCall { id, name, .. } = b {
                names.insert(id.as_str(), name.as_str());
            }
        }
    }
    let mut out: Vec<serde_json::Value> = vec![];
    let mut push = |role: &str, parts: Vec<serde_json::Value>| {
        if parts.is_empty() {
            return;
        }
        if let Some(last) = out.last_mut() {
            if last.get("role").and_then(|r| r.as_str()) == Some(role) {
                if let Some(a) = last.get_mut("parts").and_then(|p| p.as_array_mut()) {
                    a.extend(parts);
                    return;
                }
            }
        }
        out.push(serde_json::json!({"role": role, "parts": parts}));
    };
    for m in messages {
        match m.role {
            Role::System | Role::User => {
                let t = m.full_text();
                if !t.is_empty() {
                    push("user", vec![serde_json::json!({"text": t})]);
                }
            }
            Role::Assistant => {
                let mut parts = vec![];
                for b in &m.blocks {
                    match b {
                        ContentBlock::Text { text } => {
                            if !text.is_empty() {
                                parts.push(serde_json::json!({"text": text}));
                            }
                        }
                        ContentBlock::ToolCall {
                            id: _,
                            name,
                            arguments,
                        } => parts.push(serde_json::json!({
                            "functionCall": {"name": name, "args": arguments},
                        })),
                        _ => {}
                    }
                }
                push("model", parts);
            }
            Role::Tool => {
                let mut parts = vec![];
                for b in &m.blocks {
                    if let ContentBlock::ToolResult {
                        tool_call_id,
                        content,
                        is_error,
                    } = b
                    {
                        let name = names
                            .get(tool_call_id.as_str())
                            .copied()
                            .unwrap_or("unknown");
                        let mut resp = serde_json::json!({
                            "name": name,
                            "response": {"content": content},
                        });
                        if *is_error {
                            resp["response"]["is_error"] = true.into();
                        }
                        parts.push(serde_json::json!({"functionResponse": resp}));
                    }
                }
                push("user", parts);
            }
        }
    }
    out
}

pub fn to_gemini_tools(tools: &[ToolDefinition]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "name": t.name,
                "description": t.description,
                "parameters": t.input_schema,
            })
        })
        .collect()
}

/// 错误回包提消息（`{error: {message}}`）。
pub fn error_text(v: &serde_json::Value) -> Option<&str> {
    v.pointer("/error/message").and_then(|s| s.as_str())
}

/// 非流回包解析：首 candidate；functionCall 无 id，用序号合成（执行按名路由）。
pub fn parse_gemini_response(v: serde_json::Value) -> anyhow::Result<super::ChatResponse> {
    if error_text(&v).is_some() {
        anyhow::bail!("gemini error: {}", v.pointer("/error/message").unwrap());
    }
    let cand = v
        .pointer("/candidates/0")
        .ok_or_else(|| anyhow::anyhow!("bad gemini response: {v}"))?;
    let parts = cand
        .pointer("/content/parts")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();
    let mut blocks = vec![];
    let mut call_idx = 0usize;
    for p in &parts {
        if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
            if !t.is_empty() {
                blocks.push(ContentBlock::Text { text: t.to_owned() });
            }
        }
        if let Some(fc) = p.get("functionCall") {
            blocks.push(ContentBlock::ToolCall {
                id: fc
                    .get("id")
                    .and_then(|s| s.as_str())
                    .map(str::to_owned)
                    .unwrap_or_else(|| {
                        call_idx += 1;
                        format!("call-{call_idx}")
                    }),
                name: fc
                    .get("name")
                    .and_then(|s| s.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
                arguments: fc.get("args").cloned().unwrap_or(serde_json::json!({})),
            });
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
            provider: Some("gemini".into()),
            created_at: chrono::Utc::now(),
        },
        stop_reason: cand
            .get("finishReason")
            .and_then(|s| s.as_str())
            .unwrap_or("STOP")
            .to_string(),
    })
}

/// Gemini 流累积器（纯逻辑，可单测）：`:streamGenerateContent` 逐 JSON 对象，
/// 文本直推，functionCall 按名累积 args（名唯一假设，见上）。
#[derive(Debug, Default)]
pub struct GeminiAccumulator {
    text: String,
    calls: Vec<CallFrag>,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Default, Clone)]
struct CallFrag {
    name: String,
    args: serde_json::Value,
    merged: bool,
}

impl GeminiAccumulator {
    pub async fn apply_response(
        &mut self,
        v: &serde_json::Value,
        tx: &tokio::sync::mpsc::Sender<super::StreamEvent>,
    ) {
        let cand = match v.pointer("/candidates/0") {
            Some(c) => c,
            None => return,
        };
        if let Some(fr) = cand.get("finishReason").and_then(|s| s.as_str()) {
            if fr != "STOP" || self.finish_reason.is_none() {
                self.finish_reason = Some(fr.to_string());
            }
        }
        let parts = cand
            .pointer("/content/parts")
            .and_then(|p| p.as_array())
            .cloned()
            .unwrap_or_default();
        for p in &parts {
            if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                if !t.is_empty() {
                    self.text.push_str(t);
                    let _ = tx.send(super::StreamEvent::TextDelta(t.to_string())).await;
                }
            }
            if let Some(fc) = p.get("functionCall") {
                let name = fc.get("name").and_then(|s| s.as_str()).unwrap_or("");
                let args = fc.get("args").cloned().unwrap_or(serde_json::json!({}));
                match self.calls.iter_mut().find(|c| c.name == name && !c.merged) {
                    Some(slot) => merge_args(&mut slot.args, &args),
                    None => self.calls.push(CallFrag {
                        name: name.to_string(),
                        args,
                        merged: false,
                    }),
                }
            }
        }
    }

    pub fn finish(mut self, default_stop: &str) -> super::ChatResponse {
        let mut blocks = vec![];
        if !self.text.is_empty() {
            blocks.push(ContentBlock::Text { text: self.text });
        }
        for (i, c) in self.calls.iter().enumerate() {
            let name = if c.name.is_empty() {
                "unknown".into()
            } else {
                c.name.clone()
            };
            blocks.push(ContentBlock::ToolCall {
                id: format!("call-{i}"),
                name,
                arguments: c.args.clone(),
            });
        }
        self.calls.iter_mut().for_each(|c| c.merged = true);
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
                provider: Some("gemini".into()),
                created_at: chrono::Utc::now(),
            },
            stop_reason: self
                .finish_reason
                .unwrap_or_else(|| default_stop.to_string()),
        }
    }
}

/// 流式分片 args 合并：对象逐键覆盖并集，非对象以后到为准。
fn merge_args(into: &mut serde_json::Value, add: &serde_json::Value) {
    match (into.as_object_mut(), add.as_object()) {
        (Some(a), Some(b)) => {
            for (k, v) in b {
                a.insert(k.clone(), v.clone());
            }
        }
        _ => *into = add.clone(),
    }
}

#[async_trait]
impl super::LlmProvider for GeminiProvider {
    fn name(&self) -> &str {
        "gemini"
    }

    async fn complete(&self, req: super::ChatRequest) -> anyhow::Result<super::ChatResponse> {
        let body = self.body(&req);
        let url = self.url(false);
        let api_key = self.api_key.clone();
        let resp = super::post_json_with_retry(
            || {
                self.client
                    .post(url.clone())
                    .header("x-goog-api-key", &api_key)
            },
            &body,
            3,
        )
        .await?;
        let status = resp.status();
        let v: serde_json::Value = resp.json().await?;
        if !status.is_success() {
            let msg = error_text(&v).unwrap_or("gemini request failed");
            anyhow::bail!("gemini {status}: {msg}");
        }
        parse_gemini_response(v)
    }

    /// 真流：`streamGenerateContent?alt=sse` 逐 `data:` JSON（无 event 名）。
    async fn complete_streaming(
        &self,
        req: super::ChatRequest,
        tx: tokio::sync::mpsc::Sender<super::StreamEvent>,
    ) -> anyhow::Result<super::ChatResponse> {
        use futures::StreamExt as _;
        let url = format!("{}?alt=sse", self.url(true));
        let body = self.body(&req);
        let api_key = self.api_key.clone();
        let resp = super::post_json_with_retry(
            || {
                self.client
                    .post(url.clone())
                    .header("x-goog-api-key", &api_key)
            },
            &body,
            3,
        )
        .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let v: serde_json::Value = resp.json().await.unwrap_or(serde_json::json!({}));
            let msg = error_text(&v).unwrap_or("gemini request failed");
            anyhow::bail!("gemini {status}: {msg}");
        }
        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        let mut acc = GeminiAccumulator::default();
        'stream: while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buf.find('\n') {
                let line = buf[..pos].trim().to_string();
                buf = buf[pos + 1..].to_string();
                if line.is_empty() || line.starts_with(':') {
                    continue;
                }
                let Some(data) = line.strip_prefix("data:").map(str::trim) else {
                    continue;
                };
                if data == "[DONE]" {
                    break 'stream;
                }
                let v: serde_json::Value = serde_json::from_str(data)?;
                acc.apply_response(&v, &tx).await;
            }
        }
        Ok(acc.finish("STOP"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rupi_core::Message;

    #[test]
    fn maps_roles_and_function_call() {
        let msgs = vec![
            Message::text(Role::User, "list files"),
            Message {
                id: "a".into(),
                role: Role::Assistant,
                blocks: vec![ContentBlock::ToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command": "ls"}),
                }],
                provider: None,
                created_at: chrono::Utc::now(),
            },
            Message {
                id: "t".into(),
                role: Role::Tool,
                blocks: vec![ContentBlock::ToolResult {
                    tool_call_id: "c1".into(),
                    content: "a.rs".into(),
                    is_error: false,
                }],
                provider: None,
                created_at: chrono::Utc::now(),
            },
        ];
        let out = to_gemini_contents(&msgs);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0]["role"], "user");
        assert_eq!(out[1]["role"], "model");
        assert_eq!(out[1]["parts"][0]["functionCall"]["name"], "bash");
        assert_eq!(out[1]["parts"][0]["functionCall"]["args"]["command"], "ls");
        assert_eq!(out[2]["role"], "user");
        assert!(out[2]["parts"][0].get("functionResponse").is_some());
        // functionResponse 按历史 ToolCall 回填 name
        assert_eq!(out[2]["parts"][0]["functionResponse"]["name"], "bash");
    }

    #[test]
    fn parses_text_and_function_call() {
        let v = serde_json::json!({
            "candidates": [{
                "content": {"parts": [
                    {"text": "running"},
                    {"functionCall": {"name": "glob", "args": {"pattern": "**/*.rs"}}},
                ], "role": "model"},
                "finishReason": "STOP",
            }],
        });
        let r = parse_gemini_response(v).unwrap();
        assert_eq!(r.stop_reason, "STOP");
        assert_eq!(r.message.blocks.len(), 2);
        match &r.message.blocks[1] {
            ContentBlock::ToolCall {
                name, arguments, ..
            } => {
                assert_eq!(name, "glob");
                assert_eq!(arguments["pattern"], "**/*.rs");
            }
            _ => panic!("expected tool call"),
        }
    }

    #[test]
    fn error_payload_is_surfaced() {
        let v = serde_json::json!({"error": {"code": 400, "message": "bad key"}});
        assert_eq!(error_text(&v), Some("bad key"));
        assert!(parse_gemini_response(v).is_err());
    }

    #[tokio::test]
    async fn accumulator_merges_fragmented_function_call() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let mut acc = GeminiAccumulator::default();
        acc.apply_response(
            &serde_json::json!({"candidates": [{"content": {"parts": [
                {"text": "ok"},
                {"functionCall": {"name": "bash", "args": {"command": "ls"}}},
            ]}, "finishReason": "STOP"}]}),
            &tx,
        )
        .await;
        // 同名后续分片合并 args（流式常把大参数拆开展示）
        acc.apply_response(
            &serde_json::json!({"candidates": [{"content": {"parts": [
                {"functionCall": {"name": "bash", "args": {"extra": 1}}},
            ]}}]}),
            &tx,
        )
        .await;
        drop(tx);
        let mut deltas = vec![];
        while let Some(d) = rx.recv().await {
            deltas.push(d);
        }
        assert_eq!(deltas.len(), 1);
        let r = acc.finish("STOP");
        assert_eq!(r.message.blocks.len(), 2);
        match &r.message.blocks[1] {
            ContentBlock::ToolCall { arguments, .. } => {
                assert_eq!(arguments["command"], "ls");
                assert_eq!(arguments["extra"], 1);
            }
            _ => panic!("expected tool call"),
        }
    }
}
