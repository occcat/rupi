//! rupi-llm: 统一 LLM API（对标 pi-ai）。
//! Provider 无关：上层只依赖 [`LlmProvider`] trait；会话可混用多家消息，provider 只做尽力兼容。

use async_trait::async_trait;
use rupi_core::{ContentBlock, Message, Role, ToolDefinition};
use serde::{Deserialize, Serialize};

pub mod anthropic;
pub use anthropic::AnthropicProvider;
pub mod gemini;
pub use gemini::GeminiProvider;
pub mod bedrock;
pub use bedrock::BedrockProvider;
pub mod vertex;
pub use vertex::VertexProvider;
pub mod catalog;
pub use catalog::{format_catalog, load_models, ModelEntry};
pub mod route;
pub use route::{parse_model_spec, provider_from_spec, ModelSpec, ProviderOptions};
pub mod overflow;
pub use overflow::is_overflow_error;
pub mod sse;
pub use sse::{SseEvent, SseParser};

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

/// 思考强度（对标上游 thinking levels）：off/low/medium/high/xhigh/max。
/// 各 provider 按自家参数名映射，语义统一为“推理预算逐档放大”。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    #[default]
    Off,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl std::str::FromStr for ThinkingLevel {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "off" | "none" | "disabled" => Ok(ThinkingLevel::Off),
            "low" | "minimal" | "min" => Ok(ThinkingLevel::Low),
            "medium" | "med" => Ok(ThinkingLevel::Medium),
            "high" => Ok(ThinkingLevel::High),
            "xhigh" | "x-high" | "extra" => Ok(ThinkingLevel::XHigh),
            "max" => Ok(ThinkingLevel::Max),
            other => anyhow::bail!(
                "invalid thinking level '{other}' (off|low|medium|high|xhigh|max)"
            ),
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
            ThinkingLevel::XHigh => Some("xhigh"),
            ThinkingLevel::Max => Some("xhigh"),
        }
    }

    /// Gemini `thinkingConfig.thinkingLevel` 取值；Off 返回 None（字段省略）。
    pub fn gemini_level(self) -> Option<&'static str> {
        match self {
            ThinkingLevel::Off => None,
            ThinkingLevel::Low => Some("LOW"),
            ThinkingLevel::Medium => Some("MEDIUM"),
            ThinkingLevel::High | ThinkingLevel::XHigh | ThinkingLevel::Max => Some("HIGH"),
        }
    }

    /// Anthropic extended-thinking 预算 token；Off 返回 None（字段省略）。
    pub fn anthropic_budget(self) -> Option<u32> {
        match self {
            ThinkingLevel::Off => None,
            ThinkingLevel::Low => Some(1024),
            ThinkingLevel::Medium => Some(4096),
            ThinkingLevel::High => Some(8192),
            ThinkingLevel::XHigh => Some(16384),
            ThinkingLevel::Max => Some(32768),
        }
    }

    /// 开启思考时 `max_tokens` 至少预算 + 输出预留，避免 medium/high 因默认 4096 静默失效。
    pub fn anthropic_min_max_tokens(self, requested: u32) -> u32 {
        match self.anthropic_budget() {
            Some(b) => requested.max(b.saturating_add(4096)),
            None => requested,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    pub message: Message,
    pub stop_reason: String,
}

/// 流式事件：文本增量即时透出；工具调用增量由各 provider 在内部累积，
/// 随最终 `ChatResponse` 一次返回（与 OpenAI `tool_calls` 流式语义对齐）。
/// `Usage` 在流末尾给出（OpenAI `stream_options.include_usage` / Anthropic
/// `message_delta.usage` / Gemini `usageMetadata`），多次给出以最后一次为准。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    TextDelta(String),
    Usage { input: u64, output: u64 },
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
    /// 会话亲和 id（OpenRouter `x-session-id` 荷载）：默认空实现（Mock 等无 HTTP
    /// 身份的不感知）。CLI 在 Box 唯一所有权阶段写入 sessions.db 的会话 id，
    /// `--resume` 同 id 即同一下游（对标上游 `sessionId`）；缺省为实例级随机 id，
    /// 同进程同轮请求照样粘滞，跨进程续聊则换域。
    fn set_session_id(&mut self, _id: String) {}
    /// 会话亲和显式开关（对标上游 compat opt-out）：默认空实现。
    fn set_session_affinity(&mut self, _on: bool) {}
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
            Role::User => out.push(serde_json::json!({
                "role":"user",
                "content": openai_user_content(m),
            })),
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
                let mut images = Vec::new();
                for b in &m.blocks {
                    match b {
                        ContentBlock::ToolResult {
                            tool_call_id,
                            content,
                            ..
                        } => {
                            out.push(serde_json::json!({"role":"tool","tool_call_id": tool_call_id, "content": content}));
                        }
                        ContentBlock::Image { media_type, data } => {
                            images.push(serde_json::json!({
                                "type": "image_url",
                                "image_url": {"url": rupi_core::image_data_url(media_type, data)},
                            }));
                        }
                        _ => {}
                    }
                }
                if !images.is_empty() {
                    let mut parts = vec![serde_json::json!({
                        "type": "text",
                        "text": "[image(s) from tool result]",
                    })];
                    parts.extend(images);
                    out.push(serde_json::json!({"role":"user","content": parts}));
                }
            }
        }
    }
    out
}

/// 用户消息：有图片时走 content parts（`image_url` data URL），否则保持字符串。
fn openai_user_content(m: &Message) -> serde_json::Value {
    if !m.has_images() {
        return serde_json::Value::String(m.full_text());
    }
    let mut parts = Vec::new();
    for b in &m.blocks {
        match b {
            ContentBlock::Text { text } if !text.is_empty() => {
                parts.push(serde_json::json!({"type": "text", "text": text}));
            }
            ContentBlock::Image { media_type, data } => {
                parts.push(serde_json::json!({
                    "type": "image_url",
                    "image_url": {"url": rupi_core::image_data_url(media_type, data)},
                }));
            }
            _ => {}
        }
    }
    serde_json::Value::Array(parts)
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
        // 对标 pi-ai：让兼容网关在流末尾附带 usage（不支持的网关忽略该字段）
        m.insert(
            "stream_options".into(),
            serde_json::json!({"include_usage": true}),
        );
    }
    if let Some(effort) = req.thinking.and_then(|t| t.openai_effort()) {
        m.insert("reasoning_effort".into(), effort.into());
    }
    serde_json::Value::Object(m)
}

/// 是否走 OpenRouter 网关（对标上游 `model.provider === "openrouter" ||
/// baseUrl.includes("openrouter.ai")`；本仓库无 provider 名字段，只看 base_url）。
pub fn is_openrouter_base_url(url: &str) -> bool {
    url.contains("openrouter.ai")
}

/// `RUPI_SESSION_AFFINITY` 显式开关（`1`/`true` 强开，`0`/`false` 强关；
/// 未设或非法走 URL 自动判定）：对标上游 compat 显式 opt-out，CLI 构造与
/// TUI `/model` 切换共用此口径，避免两端漂移。
pub fn session_affinity_from_env() -> Option<bool> {
    match std::env::var("RUPI_SESSION_AFFINITY")
        .ok()
        .map(|s| s.trim().to_lowercase())
        .as_deref()
    {
        Some("1") | Some("true") | Some("yes") | Some("on") => Some(true),
        Some("0") | Some("false") | Some("no") | Some("off") => Some(false),
        _ => None,
    }
}

/// 会话装配收敛点：sid（sessions.db 会话 id）+ 环境显式开关一次性打到 provider。
/// 在 Box 唯一所有权阶段调用（进 Arc 分享前），无需 `Arc::get_mut` 抢可变借用。
pub fn apply_session_settings(p: &mut dyn LlmProvider, session_id: Option<&str>) {
    if let Some(sid) = session_id {
        p.set_session_id(sid.to_string());
    }
    if let Some(on) = session_affinity_from_env() {
        p.set_session_affinity(on);
    }
}

/// 按 `provider/model[:thinking]` 或模型名前缀选 provider。
/// `claude-*`→Anthropic，`gemini-*`→Gemini，其余→OpenAI-compatible；
/// 显式前缀 `openrouter/`/`azure/`/`bedrock/`/`vertex/` 走对应路由。
/// 缺 key 即 Err，由调用方决定回 mock 还是报错。CLI 与 TUI `/model` 共用。
pub fn provider_for_model(model: &str) -> anyhow::Result<Box<dyn LlmProvider>> {
    provider_from_spec(&parse_model_spec(model), &ProviderOptions::default())
}

/// OpenAI-compat 路由形态：默认 Chat Completions；Azure 走 deployment URL + `api-key`；
/// OpenRouter 加 Referer/Title 与默认会话亲和。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompatKind {
    #[default]
    OpenAi,
    OpenRouter,
    Azure,
}

/// 进程内共享的 reqwest Client（rustls + webpki 根证书）。`clone` 只增 Arc；
/// `/model` 切换不再各建一个 Client。
pub(crate) fn shared_http_client() -> reqwest::Client {
    use std::sync::OnceLock;
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .build()
                .expect("build rustls HTTP client")
        })
        .clone()
}

/// OpenAI-compatible provider：覆盖 OpenAI / DeepSeek / Moonshot / 本地 Ollama 等。
/// 通过 `base_url + api_key + model` 配置，默认 `https://api.openai.com/v1`。
/// OpenRouter 会话亲和（对标上游 bbb61e3）：base_url 含 `openrouter.ai` 时默认
/// 带 `x-session-id` 头（同一实例 id，同轮请求落同一下游），显式开关可覆盖。
#[derive(Debug, Clone)]
pub struct OpenAiCompatProvider {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub session_id: String,
    pub session_affinity: Option<bool>,
    pub kind: CompatKind,
    pub api_version: Option<String>,
    client: reqwest::Client,
}

impl OpenAiCompatProvider {
    pub fn new(base_url: String, api_key: String, model: String) -> Self {
        Self {
            base_url,
            api_key,
            model,
            session_id: uuid::Uuid::new_v4().to_string(),
            session_affinity: None,
            kind: CompatKind::OpenAi,
            api_version: None,
            client: crate::shared_http_client(),
        }
    }

    pub fn with_kind(mut self, kind: CompatKind) -> Self {
        self.kind = kind;
        if kind == CompatKind::OpenRouter && self.session_affinity.is_none() {
            self.session_affinity = Some(true);
        }
        self
    }

    pub fn with_api_version(mut self, ver: impl Into<String>) -> Self {
        self.api_version = Some(ver.into());
        self
    }

    fn chat_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        match self.kind {
            CompatKind::Azure => {
                let ver = self.api_version.as_deref().unwrap_or("2024-10-21");
                format!("{base}/openai/deployments/{}/chat/completions?api-version={ver}", self.model)
            }
            _ => format!("{base}/chat/completions"),
        }
    }

    pub fn with_session_id(mut self, id: impl Into<String>) -> Self {
        self.session_id = id.into();
        self
    }

    pub fn with_session_affinity(mut self, on: bool) -> Self {
        self.session_affinity = Some(on);
        self
    }

    fn session_header(&self) -> Option<(&'static str, &str)> {
        let on = match self.session_affinity {
            Some(v) => v,
            None => self.kind == CompatKind::OpenRouter || is_openrouter_base_url(&self.base_url),
        };
        on.then_some(("x-session-id", self.session_id.as_str()))
    }

    fn authed(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let req = match self.kind {
            CompatKind::Azure => req.header("api-key", &self.api_key),
            _ => req.bearer_auth(&self.api_key),
        };
        let req = match self.kind {
            CompatKind::OpenRouter => req
                .header("HTTP-Referer", "https://github.com/occcat/rupi")
                .header("X-Title", "rupi"),
            _ => req,
        };
        match self.session_header() {
            Some((k, v)) => req.header(k, v),
            None => req,
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
        match self.kind {
            CompatKind::OpenAi => "openai-compat",
            CompatKind::OpenRouter => "openrouter",
            CompatKind::Azure => "azure",
        }
    }

    fn model_id(&self) -> Option<&str> {
        Some(&self.model)
    }

    fn set_session_id(&mut self, id: String) {
        self.session_id = id;
    }

    fn set_session_affinity(&mut self, on: bool) {
        self.session_affinity = Some(on);
    }

    async fn complete(&self, req: ChatRequest) -> anyhow::Result<ChatResponse> {
        let url = self.chat_url();
        let body = openai_body(&self.model, &req, false);
        let client = self.client.clone();
        let resp =
            post_json_with_retry(|| self.authed(client.post(url.clone())), &body, 3).await?;
        let resp = ensure_success("openai-compat", resp).await?;
        let v: serde_json::Value = resp.json().await?;
        parse_openai_response(v)
    }

    /// 真 SSE 流：`stream: true` + `stream_options.include_usage`，共用 [`SseParser`]
    /// （跨 chunk UTF-8、多行 `data:`），累积文本与 `tool_calls` 片段，末尾推 Usage。
    async fn complete_streaming(
        &self,
        req: ChatRequest,
        tx: tokio::sync::mpsc::Sender<StreamEvent>,
    ) -> anyhow::Result<ChatResponse> {
        use futures::StreamExt as _;
        let url = self.chat_url();
        let body = openai_body(&self.model, &req, true);
        let client = self.client.clone();
        let resp =
            post_json_with_retry(|| self.authed(client.post(url.clone())), &body, 3).await?;
        let resp = ensure_success("openai-compat", resp).await?;
        let mut stream = resp.bytes_stream();
        let mut parser = SseParser::default();
        let mut acc = SseAccumulator::default();
        let mut stop_reason = "stop".to_string();
        'stream: while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            for ev in parser.push_bytes(&chunk) {
                // [DONE] 是流终结符：必须跳出外层字节循环，否则
                // 连接复用的网关会让 next() 永远等待，整轮卡死。
                if ev.data.trim() == "[DONE]" {
                    break 'stream;
                }
                let v: serde_json::Value = match serde_json::from_str(&ev.data) {
                    Ok(v) => v,
                    Err(e) => {
                        // 非 JSON 的 data（网关心跳/注释）跳过，不再让整轮失败
                        tracing::debug!("openai-compat: skip non-JSON sse data ({e})");
                        continue;
                    }
                };
                // 流中错误对象（网关限流/上游故障常以 data 形式下发）：转错误而非静默空回
                if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
                    let msg = err
                        .get("message")
                        .and_then(|m| m.as_str())
                        .map(str::to_owned)
                        .unwrap_or_else(|| err.to_string());
                    anyhow::bail!("openai-compat stream error: {msg}");
                }
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
        // usage 块（include_usage 时最后一个 chunk，choices 为空）：先于 delta 判定处理
        if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
            let input = u.get("prompt_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
            let output = u
                .get("completion_tokens")
                .and_then(|x| x.as_u64())
                .unwrap_or(0);
            if input > 0 || output > 0 {
                let _ = tx.send(StreamEvent::Usage { input, output }).await;
            }
        }
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

/// 非 2xx 回包转错误：状态码 + 响应体里的错误文案（JSON `error.message` / `error` /
/// `message`，否则原文，截 2000 字）。此前走 `error_for_status()` 只留状态码：用户
/// 看不到网关原因（如“无权访问该模型”），`is_overflow_error` 也永远匹配不到 body 里的
/// 溢出文案，溢出恢复对 OpenAI-compat 形同失效。
pub async fn ensure_success(
    provider: &str,
    resp: reqwest::Response,
) -> anyhow::Result<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    let detail = extract_error_message(&body);
    if detail.is_empty() {
        anyhow::bail!("{provider} HTTP {status} (no body)");
    }
    anyhow::bail!("{provider} HTTP {status}: {detail}");
}

/// 从错误回包里挑可读文案：JSON 的 `error.message` / `error`（字符串）/ `message`，否则原文。
pub fn extract_error_message(body: &str) -> String {
    let trimmed = body.trim();
    let picked = serde_json::from_str::<serde_json::Value>(trimmed)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .and_then(|m| m.as_str())
                .map(str::to_owned)
                .or_else(|| v.get("error").and_then(|e| e.as_str()).map(str::to_owned))
                .or_else(|| v.get("message").and_then(|m| m.as_str()).map(str::to_owned))
        })
        .unwrap_or_else(|| trimmed.to_string());
    picked.chars().take(2000).collect()
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
    /// 每轮请求的 system 提示（断言压实自定义指令等 system 拼装用）。
    pub seen_systems: std::sync::Mutex<Vec<String>>,
}

impl MockProvider {
    pub fn new(script: Vec<ChatResponse>) -> Self {
        Self {
            script: std::sync::Mutex::new(script),
            seen_tools: std::sync::Mutex::new(vec![]),
            seen_systems: std::sync::Mutex::new(vec![]),
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
        self.seen_systems.lock().unwrap().push(req.system.clone());
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

/// HTTP stub（`#[tokio::test]` 端到端用）：手写 HTTP/1.1 成帧，每连接读一个请求、
/// 回固定 JSON，对标 `rupi-mcp/tests/mcp_bridge.rs` 的 stub 写法。无 key 也能覆盖
/// 真模型请求路径（path/头/体形状），key-gated 代码不再只有纯逻辑单测。
#[cfg(test)]
mod teststub {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Default)]
    pub struct Seen {
        pub path: Mutex<String>,
        pub headers: Mutex<HashMap<String, String>>,
        pub body: Mutex<serde_json::Value>,
        pub count: Mutex<usize>,
    }

    pub async fn start(payload: serde_json::Value) -> (String, Arc<Seen>) {
        start_with_status(payload, 200).await
    }

    pub async fn start_with_status(
        payload: serde_json::Value,
        status: u16,
    ) -> (String, Arc<Seen>) {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
        let seen = Arc::new(Seen::default());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen_clone = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    break;
                };
                let seen = seen_clone.clone();
                let payload = payload.clone();
                let status = status;
                tokio::spawn(async move {
                    let status_line = match status {
                        200 => "200 OK",
                        400 => "400 Bad Request",
                        401 => "401 Unauthorized",
                        429 => "429 Too Many Requests",
                        500 => "500 Internal Server Error",
                        _ => "200 OK",
                    };
                    let (rh, mut wh) = sock.into_split();
                    let mut reader = tokio::io::BufReader::new(rh);
                    let mut request_line = String::new();
                    if reader.read_line(&mut request_line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let path = request_line
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("")
                        .to_string();
                    let mut headers = HashMap::new();
                    let mut content_len = 0usize;
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let t = line.trim();
                        if t.is_empty() {
                            break;
                        }
                        if let Some((k, v)) = t.split_once(':') {
                            headers.insert(k.trim().to_lowercase(), v.trim().to_string());
                        }
                        if let Some(v) = t.to_lowercase().strip_prefix("content-length:") {
                            content_len = v.trim().parse().unwrap_or(0);
                        }
                    }
                    let mut raw = vec![0u8; content_len];
                    if content_len > 0 && reader.read_exact(&mut raw).await.is_err() {
                        return;
                    }
                    *seen.path.lock().unwrap() = path;
                    *seen.headers.lock().unwrap() = headers;
                    *seen.body.lock().unwrap() =
                        serde_json::from_slice(&raw).unwrap_or(serde_json::Value::Null);
                    *seen.count.lock().unwrap() += 1;
                    let body = payload.to_string().into_bytes();
                    let head = format!(
                        "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = wh.write_all(head.as_bytes()).await;
                    let _ = wh.write_all(&body).await;
                });
            }
        });
        (base, seen)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rustls_shared_client_builds() {
        let _ = shared_http_client();
    }

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
        assert_eq!(ThinkingLevel::from_str("xhigh").unwrap(), ThinkingLevel::XHigh);
        assert_eq!(ThinkingLevel::from_str("max").unwrap(), ThinkingLevel::Max);
        assert!(ThinkingLevel::from_str("ultra").is_err());
        assert_eq!(ThinkingLevel::High.openai_effort(), Some("high"));
        assert_eq!(ThinkingLevel::XHigh.openai_effort(), Some("xhigh"));
        assert_eq!(ThinkingLevel::Off.openai_effort(), None);
        assert_eq!(ThinkingLevel::Low.gemini_level(), Some("LOW"));
        assert_eq!(ThinkingLevel::Max.gemini_level(), Some("HIGH"));
        assert_eq!(ThinkingLevel::Medium.anthropic_budget(), Some(4096));
        assert_eq!(ThinkingLevel::XHigh.anthropic_budget(), Some(16384));
        assert!(ThinkingLevel::High.anthropic_min_max_tokens(4096) > 8192);
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
    fn openai_message_mapping_keeps_images() {
        let m = Message::from_blocks(
            Role::User,
            vec![
                ContentBlock::Text {
                    text: "see".into(),
                },
                ContentBlock::Image {
                    media_type: "image/png".into(),
                    data: "AAA".into(),
                },
            ],
        );
        let msgs = to_openai_messages("sys", &[m]);
        assert_eq!(msgs[1]["content"][0]["type"], "text");
        assert_eq!(msgs[1]["content"][1]["type"], "image_url");
        assert!(msgs[1]["content"][1]["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,"));
    }

    #[test]
    fn azure_chat_url_uses_deployments() {
        let p = OpenAiCompatProvider::new("https://oai.example".into(), "k".into(), "gpt4".into())
            .with_kind(CompatKind::Azure)
            .with_api_version("2024-10-21");
        assert_eq!(
            p.chat_url(),
            "https://oai.example/openai/deployments/gpt4/chat/completions?api-version=2024-10-21"
        );
        assert_eq!(p.name(), "azure");
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

    #[test]
    fn openrouter_detection_matches_upstream_rule() {
        assert!(is_openrouter_base_url("https://openrouter.ai/api/v1"));
        assert!(is_openrouter_base_url(
            "https://openrouter.ai/api/v1/chat/completions"
        ));
        assert!(!is_openrouter_base_url("https://api.openai.com/v1"));
        assert!(!is_openrouter_base_url("http://127.0.0.1:8080/v1"));
    }

    #[test]
    fn dyn_setters_match_builder_semantics() {
        let mut p =
            OpenAiCompatProvider::new("https://api.openai.com/v1".into(), "k".into(), "m".into());
        // 经 dyn 分发写入（CLI build_provider 即此路径，非具体类型调用）
        let d: &mut dyn LlmProvider = &mut p;
        d.set_session_id("dyn-sess".into());
        d.set_session_affinity(true);
        assert_eq!(p.session_header(), Some(("x-session-id", "dyn-sess")));
        // Mock 走默认空实现：不断言行为，只验不断链
        let mut m: Box<dyn LlmProvider> =
            Box::new(MockProvider::new(vec![MockProvider::text_response("hi")]));
        m.set_session_id("x".into());
        m.set_session_affinity(true);
    }

    #[test]
    fn session_affinity_env_parses_and_applies() {
        let saved = std::env::var("RUPI_SESSION_AFFINITY").ok();
        unsafe { std::env::set_var("RUPI_SESSION_AFFINITY", "0") };
        assert_eq!(session_affinity_from_env(), Some(false));
        // 强关压过 URL 自动判定，sid 照写
        let mut q = OpenAiCompatProvider::new(
            "https://openrouter.ai/api/v1".into(),
            "k".into(),
            "m".into(),
        );
        apply_session_settings(&mut q, Some("s1"));
        assert_eq!(q.session_id, "s1");
        assert!(q.session_header().is_none());
        unsafe { std::env::set_var("RUPI_SESSION_AFFINITY", "yes") };
        assert_eq!(session_affinity_from_env(), Some(true));
        unsafe { std::env::set_var("RUPI_SESSION_AFFINITY", "whatever") };
        assert_eq!(session_affinity_from_env(), None);
        unsafe { std::env::remove_var("RUPI_SESSION_AFFINITY") };
        assert_eq!(session_affinity_from_env(), None);
        if let Some(v) = saved {
            unsafe { std::env::set_var("RUPI_SESSION_AFFINITY", v) };
        }
    }
    #[test]
    fn session_header_auto_by_url_with_explicit_override() {
        let auto = OpenAiCompatProvider::new(
            "https://openrouter.ai/api/v1".into(),
            "k".into(),
            "m".into(),
        );
        let (k, v) = auto.session_header().expect("openrouter auto affinity");
        assert_eq!(k, "x-session-id");
        assert_eq!(v, auto.session_id.as_str());
        let plain =
            OpenAiCompatProvider::new("https://api.openai.com/v1".into(), "k".into(), "m".into());
        assert!(plain.session_header().is_none());
        // 显式开关优先于 URL 判定（对标上游 explicit opt-out）
        let forced =
            OpenAiCompatProvider::new("https://api.openai.com/v1".into(), "k".into(), "m".into())
                .with_session_affinity(true)
                .with_session_id("sess-1");
        assert_eq!(
            forced.session_header(),
            Some(("x-session-id", "sess-1"))
        );
        let opted_out = OpenAiCompatProvider::new(
            "https://openrouter.ai/api/v1".into(),
            "k".into(),
            "m".into(),
        )
        .with_session_affinity(false);
        assert!(opted_out.session_header().is_none());
    }

    fn stub_request() -> ChatRequest {
        ChatRequest {
            system: "sys".into(),
            messages: vec![],
            tools: vec![],
            max_tokens: None,
            temperature: None,
            thinking: None,
        }
    }

    #[tokio::test]
    async fn openai_compat_complete_posts_json_without_affinity_by_default() {
        let payload = serde_json::json!({
            "choices": [{"message": {"content": "stub-hi"}, "finish_reason": "stop"}]
        });
        let (base, seen) = super::teststub::start(payload).await;
        let p = OpenAiCompatProvider::new(base, "k".into(), "stub-model".into());
        let resp = p.complete(stub_request()).await.unwrap();
        assert_eq!(resp.message.full_text(), "stub-hi");
        assert_eq!(resp.stop_reason, "stop");
        assert_eq!(*seen.path.lock().unwrap(), "/chat/completions");
        let headers = seen.headers.lock().unwrap();
        assert_eq!(
            headers.get("authorization").map(String::as_str),
            Some("Bearer k")
        );
        assert!(headers.get("x-session-id").is_none());
        let body = seen.body.lock().unwrap();
        assert_eq!(body["model"], "stub-model");
        // 非流式不带 stream 字段（只在 true 时插入）
        assert!(body.get("stream").is_none());
        assert!(body.get("reasoning_effort").is_none());
    }

    #[tokio::test]
    async fn openai_compat_complete_sends_session_id_when_affinity_on() {
        let payload = serde_json::json!({
            "choices": [{"message": {"content": "ok"}, "finish_reason": "stop"}]
        });
        let (base, seen) = super::teststub::start(payload).await;
        let p = OpenAiCompatProvider::new(base, "k".into(), "m".into())
            .with_session_affinity(true)
            .with_session_id("sess-9");
        p.complete(stub_request()).await.unwrap();
        let headers = seen.headers.lock().unwrap();
        assert_eq!(
            headers.get("x-session-id").map(String::as_str),
            Some("sess-9")
        );
    }
}

#[cfg(test)]
mod error_body_tests {
    use super::*;

    fn req() -> ChatRequest {
        ChatRequest {
            system: "s".into(),
            messages: vec![Message::text(Role::User, "hi")],
            tools: vec![],
            max_tokens: None,
            temperature: None,
            thinking: None,
        }
    }

    #[tokio::test]
    async fn non_2xx_error_surfaces_body_message_in_both_paths() {
        let (base, _seen) = teststub::start_with_status(
            serde_json::json!({"error": {"message": "This token has no access to model x", "type": "new_api_error"}}),
            400,
        )
        .await;
        let p = OpenAiCompatProvider::new(base, "k".into(), "x".into());
        let err = p.complete(req()).await.unwrap_err();
        assert!(format!("{err:#}").contains("no access to model x"), "{err:#}");
        assert!(format!("{err:#}").contains("400"), "{err:#}");
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let err = p.complete_streaming(req(), tx).await.unwrap_err();
        assert!(format!("{err:#}").contains("no access to model x"), "{err:#}");
    }

    #[tokio::test]
    async fn overflow_body_is_detected_by_is_overflow_error() {
        let (base, _seen) = teststub::start_with_status(
            serde_json::json!({"error": {"message": "This model's maximum context length is 8192 tokens. However, you requested 9000 tokens."}}),
            400,
        )
        .await;
        let p = OpenAiCompatProvider::new(base, "k".into(), "x".into());
        let err = p.complete(req()).await.unwrap_err();
        assert!(is_overflow_error(&format!("{err:#}")), "{err:#}");
    }

    #[test]
    fn extract_error_message_prefers_json_error_message() {
        assert_eq!(extract_error_message(r#"{"error":{"message":"boom"}}"#), "boom");
        assert_eq!(extract_error_message(r#"{"error":"plain"}"#), "plain");
        assert_eq!(extract_error_message(r#"{"message":"m"}"#), "m");
        assert_eq!(extract_error_message("  raw text "), "raw text");
    }

    #[tokio::test]
    async fn usage_chunk_emits_usage_event() {
        let mut acc = SseAccumulator::default();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        acc.apply_chunk(
            &serde_json::json!({"choices": [], "usage": {"prompt_tokens": 12, "completion_tokens": 3}}),
            &tx,
        )
        .await;
        assert_eq!(
            rx.try_recv().unwrap(),
            StreamEvent::Usage { input: 12, output: 3 }
        );
    }
}
