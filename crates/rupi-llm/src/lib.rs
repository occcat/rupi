//! rupi-llm: 统一 LLM API（对标 pi-ai）。
//! Provider 无关：上层只依赖 [`LlmProvider`] trait；会话可混用多家消息，provider 只做尽力兼容。

use async_trait::async_trait;
use rupi_core::{ContentBlock, Message, Role, ToolDefinition};
use serde::{Deserialize, Serialize};

pub mod anthropic;
pub use anthropic::AnthropicProvider;
pub mod gemini;
pub use gemini::GeminiProvider;
pub mod overflow;
pub use overflow::is_overflow_error;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    /// 思考强度（对标上游 `/thinking`）：None = 不干预（provider/模型默认）；
    /// Some(Off) = 显式关闭（能关的 provider 省 token），Low/Medium/High 逐档加码。
    /// 压缩摘要与后台 review 请求永远为 None（内部任务不需要烧推理 token）。
    /// Anthropic 开启后思考块（含 signature）随历史回放，多轮工具流不断签。
    #[serde(default)]
    pub thinking: Option<ThinkingLevel>,
}

/// 思考强度四档（对标上游 thinking levels）：各 provider 按自家参数名映射，
/// 语义统一为“推理预算逐档放大”。`FromStr` 供 CLI `--thinking` 解析。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    #[default]
    Off,
    Low,
    Medium,
    High,
}

impl std::str::FromStr for ThinkingLevel {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "off" | "none" | "disabled" => Ok(ThinkingLevel::Off),
            "low" | "minimal" | "min" => Ok(ThinkingLevel::Low),
            "medium" | "med" => Ok(ThinkingLevel::Medium),
            "high" | "max" => Ok(ThinkingLevel::High),
            other => anyhow::bail!("invalid thinking level '{other}' (off|low|medium|high)"),
        }
    }
}

impl ThinkingLevel {
    /// OpenAI chat-completions `reasoning_effort` 取值；Off 返回 None（字段省略）。
    pub fn openai_effort(self) -> Option<&'static str> {
        match self {
            ThinkingLevel::Off => None,
            ThinkingLevel::Low => Some("low"),
            ThinkingLevel::Medium => Some("medium"),
            ThinkingLevel::High => Some("high"),
        }
    }

    /// Gemini `thinkingConfig.thinkingLevel` 取值；Off 返回 None（字段省略）。
    pub fn gemini_level(self) -> Option<&'static str> {
        match self {
            ThinkingLevel::Off => None,
            ThinkingLevel::Low => Some("LOW"),
            ThinkingLevel::Medium => Some("MEDIUM"),
            ThinkingLevel::High => Some("HIGH"),
        }
    }

    /// Anthropic extended-thinking 预算 token；Off 返回 None（字段省略）。
    pub fn anthropic_budget(self) -> Option<u32> {
        match self {
            ThinkingLevel::Off => None,
            ThinkingLevel::Low => Some(1024),
            ThinkingLevel::Medium => Some(4096),
            ThinkingLevel::High => Some(8192),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    pub message: Message,
    pub stop_reason: String,
}

/// 流式事件：目前只透文本增量；工具调用增量由各 provider 在内部累积，
/// 随最终 `ChatResponse` 一次返回（与 OpenAI `tool_calls` 流式语义对齐）。
#[derive(Debug, Clone)]
pub enum StreamEvent {
    TextDelta(String),
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn name(&self) -> &str;
    /// 当前模型 id（压实按模型覆盖的 key 材料：`name/model_id`，对标上游
    /// `compaction.modelOverrides` 的 `"provider/modelId"` 键）。无固定模型
    ///（如聚合网关动态路由）返回 None，覆盖查找回退全局默认。
    fn model_id(&self) -> Option<&str> {
        None
    }
    async fn complete(&self, req: ChatRequest) -> anyhow::Result<ChatResponse>;

    /// 流式补全：边收边推 `TextDelta`，最终仍返回完整 `ChatResponse`。
    /// 默认实现退化为非流式（整体推一次），SSE 真流由各 provider 按需覆盖。
    async fn complete_streaming(
        &self,
        req: ChatRequest,
        tx: tokio::sync::mpsc::Sender<StreamEvent>,
    ) -> anyhow::Result<ChatResponse> {
        let resp = self.complete(req).await?;
        for b in &resp.message.blocks {
            if let ContentBlock::Text { text } = b {
                if !text.is_empty() {
                    let _ = tx.send(StreamEvent::TextDelta(text.clone())).await;
                }
            }
        }
        Ok(resp)
    }
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

/// OpenAI chat-completions 请求体：thinking 有档位时加 `reasoning_effort`，
/// 无/Off 时字段省略（显式传空值部分网关会 400）。
fn openai_body(model: &str, req: &ChatRequest, stream: bool) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    m.insert("model".into(), model.to_string().into());
    m.insert(
        "messages".into(),
        serde_json::Value::Array(to_openai_messages(&req.system, &req.messages)),
    );
    m.insert(
        "tools".into(),
        serde_json::Value::Array(to_openai_tools(&req.tools)),
    );
    m.insert("temperature".into(), req.temperature.unwrap_or(0.2).into());
    if stream {
        m.insert("stream".into(), true.into());
    }
    if let Some(effort) = req.thinking.and_then(|t| t.openai_effort()) {
        m.insert("reasoning_effort".into(), effort.into());
    }
    serde_json::Value::Object(m)
}

/// 按模型名前缀选原生 provider（`claude-*`→Anthropic，`gemini-*`→Gemini，
/// 其余→OpenAI-compatible）：缺 key 即 Err，由调用方决定回 mock 还是报错。
/// CLI 与 TUI 的 `/model` 共用此路由，避免两端漂移。
pub fn provider_for_model(model: &str) -> anyhow::Result<Box<dyn LlmProvider>> {
    if model.starts_with("claude-") {
        return Ok(Box::new(AnthropicProvider::from_env(model.to_string())?));
    }
    if model.starts_with("gemini-") {
        return Ok(Box::new(GeminiProvider::from_env(model.to_string())?));
    }
    Ok(Box::new(OpenAiCompatProvider::from_env(model.to_string())?))
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
        let api_key = std::env::var("RUPI_API_KEY")
            .or_else(|_| std::env::var("OPENAI_API_KEY"))
            .map_err(|_| anyhow::anyhow!("set RUPI_API_KEY or OPENAI_API_KEY"))?;
        Ok(Self::new(base_url, api_key, model))
    }
}

#[async_trait]
impl LlmProvider for OpenAiCompatProvider {
    fn name(&self) -> &str {
        "openai-compat"
    }

    fn model_id(&self) -> Option<&str> {
        Some(&self.model)
    }

    async fn complete(&self, req: ChatRequest) -> anyhow::Result<ChatResponse> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let body = openai_body(&self.model, &req, false);
        let api_key = self.api_key.clone();
        let client = self.client.clone();
        let resp =
            post_json_with_retry(|| client.post(url.clone()).bearer_auth(&api_key), &body, 3)
                .await?
                .error_for_status()?;
        let v: serde_json::Value = resp.json().await?;
        parse_openai_response(v)
    }

    /// 真 SSE 流：`stream: true`，逐 `data:` 行累积文本与 `tool_calls` 片段。
    async fn complete_streaming(
        &self,
        req: ChatRequest,
        tx: tokio::sync::mpsc::Sender<StreamEvent>,
    ) -> anyhow::Result<ChatResponse> {
        use futures::StreamExt as _;
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let body = openai_body(&self.model, &req, true);
        let api_key = self.api_key.clone();
        let client = self.client.clone();
        let resp =
            post_json_with_retry(|| client.post(url.clone()).bearer_auth(&api_key), &body, 3)
                .await?
                .error_for_status()?;
        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        let mut acc = SseAccumulator::default();
        let mut stop_reason = "stop".to_string();
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
                // [DONE] 是流终结符：必须跳出外层字节循环，否则
                // 连接复用的网关会让 next() 永远等待，整轮卡死。
                if data == "[DONE]" {
                    break 'stream;
                }
                let v: serde_json::Value = serde_json::from_str(data)?;
                acc.apply_chunk(&v, &tx).await;
                if let Some(fr) = v
                    .pointer("/choices/0/finish_reason")
                    .and_then(|s| s.as_str())
                {
                    stop_reason = fr.to_string();
                }
            }
        }
        Ok(acc.finish(stop_reason))
    }
}

/// SSE 增量累积器（纯逻辑，可单测）：文本直推 `TextDelta`，`tool_calls` 按 index 拼 `arguments`。
#[derive(Debug, Default)]
pub struct SseAccumulator {
    text: String,
    calls: Vec<ToolCallFrag>,
}

#[derive(Debug, Default, Clone)]
struct ToolCallFrag {
    id: String,
    name: String,
    args: String,
}

impl SseAccumulator {
    pub async fn apply_chunk(
        &mut self,
        v: &serde_json::Value,
        tx: &tokio::sync::mpsc::Sender<StreamEvent>,
    ) {
        let delta = match v.pointer("/choices/0/delta") {
            Some(d) => d,
            None => return,
        };
        if let Some(content) = delta.get("content") {
            let t = extract_text(content);
            if !t.is_empty() {
                self.text.push_str(&t);
                let _ = tx.send(StreamEvent::TextDelta(t)).await;
            }
        }
        if let Some(frags) = delta.get("tool_calls").and_then(|c| c.as_array()) {
            for f in frags {
                let idx = f.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                while self.calls.len() <= idx {
                    self.calls.push(ToolCallFrag::default());
                }
                let slot = &mut self.calls[idx];
                if let Some(id) = f.get("id").and_then(|s| s.as_str()) {
                    slot.id = id.to_string();
                }
                if let Some(name) = f.pointer("/function/name").and_then(|s| s.as_str()) {
                    slot.name = name.to_string();
                }
                if let Some(args) = f.pointer("/function/arguments").and_then(|s| s.as_str()) {
                    slot.args.push_str(args);
                }
            }
        }
    }

    pub fn finish(self, stop_reason: String) -> ChatResponse {
        let mut blocks = vec![];
        if !self.text.is_empty() {
            blocks.push(ContentBlock::Text { text: self.text });
        }
        for (i, c) in self.calls.into_iter().enumerate() {
            if c.name.is_empty() && c.args.is_empty() {
                continue;
            }
            blocks.push(ContentBlock::ToolCall {
                id: if c.id.is_empty() {
                    format!("call-{i}")
                } else {
                    c.id
                },
                name: if c.name.is_empty() {
                    "unknown".into()
                } else {
                    c.name
                },
                arguments: serde_json::from_str(&c.args).unwrap_or(serde_json::json!({})),
            });
        }
        if blocks.is_empty() {
            blocks.push(ContentBlock::Text {
                text: String::new(),
            });
        }
        ChatResponse {
            message: Message {
                id: uuid::Uuid::new_v4().to_string(),
                role: Role::Assistant,
                blocks,
                provider: Some("openai-compat".into()),
                created_at: chrono::Utc::now(),
            },
            stop_reason,
        }
    }
}

/// 提取消息文本：兼容 string / content-parts 数组（OpenAI `[{type:text}]`、
/// 经 OpenRouter 等网关的 Anthropic 风格）/ null。数组外未知形状忽略，不丢整段。
pub fn extract_text(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter_map(|p| {
                if let Some(s) = p.as_str() {
                    return Some(s.to_string());
                }
                let t = p.get("type").and_then(|t| t.as_str()).unwrap_or("");
                if t == "text" {
                    p.get("text")
                        .and_then(|x| x.as_str())
                        .map(|s| s.to_string())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// 解析工具参数：标准为 JSON 字符串；部分兼容服务直接给对象，原样采用；
/// 字符串解析失败回 `{}`（调用方报未知参数而非崩溃）。
pub fn parse_arguments(raw: &serde_json::Value) -> serde_json::Value {
    match raw {
        serde_json::Value::String(s) => serde_json::from_str(s).unwrap_or(serde_json::json!({})),
        serde_json::Value::Object(_) => raw.clone(),
        _ => serde_json::json!({}),
    }
}

fn parse_openai_response(v: serde_json::Value) -> anyhow::Result<ChatResponse> {
    let choice = v
        .pointer("/choices/0/message")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("bad openai response: {v}"))?;
    let mut blocks = vec![];
    let text = choice.get("content").map(extract_text).unwrap_or_default();
    if !text.is_empty() {
        blocks.push(ContentBlock::Text { text });
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
            let arguments = c
                .pointer("/function/arguments")
                .map(parse_arguments)
                .unwrap_or(serde_json::json!({}));
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

/// 可重试状态码：429 限流与 5xx 服务端故障（对标 Pi 指数退避重试）。
pub fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// 第 `attempt` 次（0 起）重试前等待毫秒数：500·2^attempt，上限 8000。
pub fn backoff_ms(attempt: u32) -> u64 {
    500u64.saturating_mul(1 << attempt.min(4)).min(8_000)
}

/// 解析 `Retry-After` 秒数（HTTP-date 形状交由指数退避兜底）。
pub fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let v = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    v.trim().parse::<u64>().ok().map(|s| s.saturating_mul(1000))
}

/// 带重试的 JSON POST：传输错误与 429/5xx 按 `Retry-After`（无则指数退避）
/// 重试 `max_retries` 次；非重试状态直接返回 Response 由调用方解析错误回包。
pub async fn post_json_with_retry(
    make: impl Fn() -> reqwest::RequestBuilder,
    body: &serde_json::Value,
    max_retries: u32,
) -> anyhow::Result<reqwest::Response> {
    let mut attempt = 0u32;
    loop {
        match make().json(body).send().await {
            Err(e) if attempt < max_retries => {
                tokio::time::sleep(std::time::Duration::from_millis(backoff_ms(attempt))).await;
                attempt += 1;
                let _ = e;
            }
            Err(e) => return Err(e.into()),
            Ok(resp) => {
                if is_retryable_status(resp.status()) && attempt < max_retries {
                    let wait =
                        parse_retry_after(resp.headers()).unwrap_or_else(|| backoff_ms(attempt));
                    // 消费 body 释放连接后等待重试
                    let _ = resp.bytes().await;
                    tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
                    attempt += 1;
                    continue;
                }
                return Ok(resp);
            }
        }
    }
}
/// Mock provider：单测 / 无 Key 演示用，按预设剧本返回。
pub struct MockProvider {
    pub script: std::sync::Mutex<Vec<ChatResponse>>,
    /// 每轮请求携带的工具数（断言工具可见性变化用，如渐进式发现）。
    pub seen_tools: std::sync::Mutex<Vec<usize>>,
}

impl MockProvider {
    pub fn new(script: Vec<ChatResponse>) -> Self {
        Self {
            script: std::sync::Mutex::new(script),
            seen_tools: std::sync::Mutex::new(vec![]),
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
    async fn complete(&self, req: ChatRequest) -> anyhow::Result<ChatResponse> {
        self.seen_tools.lock().unwrap().push(req.tools.len());
        let mut g = self.script.lock().unwrap();
        if g.is_empty() {
            Ok(Self::text_response("done"))
        } else {
            Ok(g.remove(0))
        }
    }

    /// 演示用分块推送：把剧本全文按 ~8 字切块，验证主循环逐 delta 渲染路径。
    async fn complete_streaming(
        &self,
        req: ChatRequest,
        tx: tokio::sync::mpsc::Sender<StreamEvent>,
    ) -> anyhow::Result<ChatResponse> {
        let resp = self.complete(req).await?;
        for b in &resp.message.blocks {
            if let ContentBlock::Text { text } = b {
                let chars: Vec<char> = text.chars().collect();
                for chunk in chars.chunks(8) {
                    let s: String = chunk.iter().collect();
                    let _ = tx.send(StreamEvent::TextDelta(s)).await;
                }
            }
        }
        Ok(resp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_status_covers_429_and_5xx() {
        use reqwest::StatusCode;
        assert!(is_retryable_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable_status(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(is_retryable_status(StatusCode::SERVICE_UNAVAILABLE));
        assert!(!is_retryable_status(StatusCode::OK));
        assert!(!is_retryable_status(StatusCode::BAD_REQUEST));
        assert!(!is_retryable_status(StatusCode::UNAUTHORIZED));
    }

    #[test]
    fn backoff_is_exponential_and_capped() {
        assert_eq!(backoff_ms(0), 500);
        assert_eq!(backoff_ms(1), 1000);
        assert_eq!(backoff_ms(2), 2000);
        assert_eq!(backoff_ms(10), 8_000);
    }

    #[test]
    fn retry_after_seconds_parsed_to_ms() {
        let mut h = reqwest::header::HeaderMap::new();
        assert_eq!(parse_retry_after(&h), None);
        h.insert(
            reqwest::header::RETRY_AFTER,
            reqwest::header::HeaderValue::from_static("2"),
        );
        assert_eq!(parse_retry_after(&h), Some(2000));
    }

    #[test]
    fn thinking_level_parses_and_maps() {
        use std::str::FromStr as _;
        assert_eq!(
            ThinkingLevel::from_str("high").unwrap(),
            ThinkingLevel::High
        );
        assert_eq!(
            ThinkingLevel::from_str("MED").unwrap(),
            ThinkingLevel::Medium
        );
        assert_eq!(ThinkingLevel::from_str("none").unwrap(), ThinkingLevel::Off);
        assert!(ThinkingLevel::from_str("ultra").is_err());
        assert_eq!(ThinkingLevel::High.openai_effort(), Some("high"));
        assert_eq!(ThinkingLevel::Off.openai_effort(), None);
        assert_eq!(ThinkingLevel::Low.gemini_level(), Some("LOW"));
        assert_eq!(ThinkingLevel::Medium.anthropic_budget(), Some(4096));
    }

    #[test]
    fn openai_body_carries_reasoning_effort_only_when_set() {
        let base = ChatRequest {
            system: "s".into(),
            messages: vec![],
            tools: vec![],
            max_tokens: None,
            temperature: None,
            thinking: None,
        };
        assert!(openai_body("m", &base, false)
            .get("reasoning_effort")
            .is_none());
        let high = ChatRequest {
            thinking: Some(ThinkingLevel::High),
            ..base
        };
        let b = openai_body("m", &high, true);
        assert_eq!(b["reasoning_effort"], "high");
        assert_eq!(b["stream"], true);
    }

    #[test]
    fn provider_for_model_routes_by_prefix() {
        //  hermetic：暂存并清空三家 key，断言路由选型（错误信息带各家 key 名即选对分支）
        let saved: Vec<(&str, Option<String>)> = [
            "RUPI_ANTHROPIC_KEY",
            "ANTHROPIC_API_KEY",
            "RUPI_GEMINI_KEY",
            "GEMINI_API_KEY",
            "GOOGLE_API_KEY",
            "RUPI_API_KEY",
            "OPENAI_API_KEY",
        ]
        .iter()
        .map(|k| (*k, std::env::var(k).ok()))
        .collect();
        for (k, _) in &saved {
            unsafe { std::env::remove_var(k) };
        }
        // 补回 base 系变量干扰？from_env 只读 key 与 *_BASE，不动 BASE 即走默认网关
        let ea = match provider_for_model("claude-sonnet-4-5") {
            Ok(_) => panic!("expected missing-key error without env"),
            Err(e) => e.to_string(),
        };
        assert!(ea.contains("ANTHROPIC_API_KEY"), "anthropic branch: {ea}");
        let eb = match provider_for_model("gemini-2.5-flash") {
            Ok(_) => panic!("expected missing-key error without env"),
            Err(e) => e.to_string(),
        };
        assert!(eb.contains("GEMINI_API_KEY"), "gemini branch: {eb}");
        let ec = match provider_for_model("gpt-4o-mini") {
            Ok(_) => panic!("expected missing-key error without env"),
            Err(e) => e.to_string(),
        };
        assert!(ec.contains("RUPI_API_KEY"), "openai branch: {ec}");
        for (k, v) in saved {
            if let Some(val) = v {
                unsafe { std::env::set_var(k, val) };
            }
        }
    }

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

    #[tokio::test]
    async fn sse_accumulator_rebuilds_text_and_fragmented_tool_call() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let mut acc = SseAccumulator::default();
        // 文本分两片
        acc.apply_chunk(
            &serde_json::json!({"choices":[{"delta":{"content":"hel"}}]}),
            &tx,
        )
        .await;
        acc.apply_chunk(
            &serde_json::json!({"choices":[{"delta":{"content":"lo"}}]}),
            &tx,
        )
        .await;
        // 工具调用 id/name 与 arguments 分三片到达
        acc.apply_chunk(
            &serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"read"}}]}}]}),
            &tx,
        )
        .await;
        acc.apply_chunk(
            &serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"pa"}}]}}]}),
            &tx,
        )
        .await;
        acc.apply_chunk(
            &serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a\"}"}}]}}]}),
            &tx,
        )
        .await;
        drop(tx);
        let mut deltas = vec![];
        while let Some(StreamEvent::TextDelta(d)) = rx.recv().await {
            deltas.push(d);
        }
        assert_eq!(deltas.concat(), "hello");
        let resp = acc.finish("tool_calls".into());
        let call = resp.message.blocks.iter().find_map(|b| match b {
            ContentBlock::ToolCall {
                name, arguments, ..
            } => Some((name, arguments)),
            _ => None,
        });
        let (name, args) = call.expect("tool call rebuilt");
        assert_eq!(name, "read");
        assert_eq!(args["path"], serde_json::json!("a"));
    }

    #[tokio::test]
    async fn mock_streams_in_chunks() {
        let p = MockProvider::new(vec![MockProvider::text_response("abcdefghij12345")]);
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let req = ChatRequest {
            system: "s".into(),
            messages: vec![],
            tools: vec![],
            max_tokens: None,
            temperature: None,
            thinking: None,
        };
        let resp = p.complete_streaming(req, tx).await.unwrap();
        drop(resp);
        let mut n = 0;
        let mut all = String::new();
        while let Some(StreamEvent::TextDelta(d)) = rx.recv().await {
            n += 1;
            all.push_str(&d);
        }
        assert!(n >= 2);
        assert_eq!(all, "abcdefghij12345");
    }

    #[test]
    fn extract_text_handles_parts_array_and_null() {
        assert_eq!(extract_text(&serde_json::json!("hi")), "hi");
        assert_eq!(
            extract_text(&serde_json::json!([
                {"type": "text", "text": "a"},
                {"type": "image_url", "image_url": {}},
                "b"
            ])),
            "ab"
        );
        assert_eq!(extract_text(&serde_json::Value::Null), "");
    }

    #[test]
    fn parse_openai_response_keeps_array_content_and_object_args() {
        let v = serde_json::json!({
            "choices": [{
                "message": {
                    "content": [{"type": "text", "text": "hello "}, {"type": "text", "text": "world"}],
                    "tool_calls": [{
                        "id": "c1", "type": "function",
                        "function": {"name": "read", "arguments": {"path": "a"}}
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        let r = parse_openai_response(v).unwrap();
        let texts: Vec<&str> = r
            .message
            .blocks
            .iter()
            .filter_map(|b| match b {
                rupi_core::ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["hello world"]);
        let (_, args) = r
            .message
            .blocks
            .iter()
            .find_map(|b| match b {
                rupi_core::ContentBlock::ToolCall {
                    name, arguments, ..
                } => Some((name.clone(), arguments.clone())),
                _ => None,
            })
            .expect("tool call rebuilt");
        assert_eq!(args["path"], serde_json::json!("a"));
    }
}
