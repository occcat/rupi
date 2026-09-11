//! AWS Bedrock 显式路由：Anthropic Messages 形状 + `anthropic_version`，
//! 鉴权走 Bearer（`AWS_BEARER_TOKEN_BEDROCK` / `BEDROCK_API_KEY` / `--api-key`）。
//! 流式默认退化为非流（Bedrock 原生是 eventstream，不在本 PR 解析）。

use async_trait::async_trait;

const ANTHROPIC_BEDROCK_VERSION: &str = "bedrock-2023-05-31";

#[derive(Debug, Clone)]
pub struct BedrockProvider {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    client: reqwest::Client,
}

impl BedrockProvider {
    pub fn new(base_url: String, api_key: String, model: String) -> Self {
        Self {
            base_url,
            api_key,
            model,
            client: crate::shared_http_client(),
        }
    }

    pub fn from_options(model: String, api_key: Option<&str>) -> anyhow::Result<Self> {
        let key = api_key
            .map(str::to_string)
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var("AWS_BEARER_TOKEN_BEDROCK").ok())
            .or_else(|| std::env::var("BEDROCK_API_KEY").ok())
            .or_else(|| std::env::var("RUPI_BEDROCK_KEY").ok())
            .ok_or_else(|| {
                anyhow::anyhow!("set AWS_BEARER_TOKEN_BEDROCK or BEDROCK_API_KEY for bedrock")
            })?;
        let base = std::env::var("RUPI_BEDROCK_BASE")
            .or_else(|_| std::env::var("BEDROCK_BASE_URL"))
            .unwrap_or_else(|_| {
                let region = std::env::var("AWS_REGION")
                    .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
                    .unwrap_or_else(|_| "us-east-1".to_string());
                format!("https://bedrock-runtime.{region}.amazonaws.com")
            });
        Ok(Self::new(base, key, model))
    }

    fn invoke_url(&self) -> String {
        let model = urlencoding_lite(&self.model);
        format!(
            "{}/model/{model}/invoke",
            self.base_url.trim_end_matches('/')
        )
    }

    fn body(&self, req: &super::ChatRequest) -> serde_json::Value {
        // 复用 Anthropic 消息映射，但 Bedrock 要顶层 anthropic_version，system 用字符串更稳。
        let mut inner = serde_json::Map::new();
        inner.insert(
            "anthropic_version".into(),
            ANTHROPIC_BEDROCK_VERSION.into(),
        );
        let requested = req.max_tokens.unwrap_or(4096);
        let max_tokens = req
            .thinking
            .map(|t| t.anthropic_min_max_tokens(requested))
            .unwrap_or(requested);
        inner.insert("max_tokens".into(), max_tokens.into());
        inner.insert("system".into(), req.system.clone().into());
        inner.insert(
            "messages".into(),
            serde_json::Value::Array(crate::anthropic::to_anthropic_messages(&req.messages)),
        );
        let tools = crate::anthropic::to_anthropic_tools(&req.tools);
        if !tools.is_empty() {
            inner.insert("tools".into(), serde_json::Value::Array(tools));
        }
        if let Some(budget) = req.thinking.and_then(|t| t.anthropic_budget()) {
            if max_tokens > budget {
                inner.insert(
                    "thinking".into(),
                    serde_json::json!({"type": "enabled", "budget_tokens": budget}),
                );
                inner.insert("temperature".into(), 1.0.into());
            }
        } else {
            inner.insert("temperature".into(), req.temperature.unwrap_or(0.2).into());
        }
        serde_json::Value::Object(inner)
    }
}

/// 模型 id 里的 `:` `/` 要进路径，做最小百分号编码。
fn urlencoding_lite(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[async_trait]
impl super::LlmProvider for BedrockProvider {
    fn name(&self) -> &str {
        "bedrock"
    }

    fn model_id(&self) -> Option<&str> {
        Some(&self.model)
    }

    async fn complete(&self, req: super::ChatRequest) -> anyhow::Result<super::ChatResponse> {
        let url = self.invoke_url();
        let body = self.body(&req);
        let key = self.api_key.clone();
        let resp = super::post_json_with_retry(
            || {
                self.client
                    .post(url.clone())
                    .header("Authorization", format!("Bearer {key}"))
                    .header("Content-Type", "application/json")
            },
            &body,
            3,
        )
        .await?;
        let status = resp.status();
        let v: serde_json::Value = resp.json().await?;
        if !status.is_success() {
            let msg = crate::anthropic::error_text(&v).unwrap_or("bedrock request failed");
            anyhow::bail!("bedrock {status}: {msg}");
        }
        let mut parsed = crate::anthropic::parse_anthropic_response(v)?;
        parsed.message.provider = Some("bedrock".into());
        Ok(parsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LlmProvider;
    use rupi_core::{Message, Role};

    #[test]
    fn invoke_url_encodes_model_colon() {
        let p = BedrockProvider::new(
            "https://bedrock-runtime.us-east-1.amazonaws.com".into(),
            "k".into(),
            "anthropic.claude-3-5-sonnet-20241022-v2:0".into(),
        );
        assert!(p.invoke_url().contains("%3A0"), "{}", p.invoke_url());
        assert!(p.invoke_url().ends_with("/invoke"));
    }

    #[test]
    fn body_has_bedrock_anthropic_version() {
        let p = BedrockProvider::new("https://x".into(), "k".into(), "m".into());
        let b = p.body(&crate::ChatRequest {
            system: "sys".into(),
            messages: vec![Message::text(Role::User, "hi")],
            tools: vec![],
            max_tokens: None,
            temperature: None,
            thinking: Some(crate::ThinkingLevel::High),
        });
        assert_eq!(b["anthropic_version"], ANTHROPIC_BEDROCK_VERSION);
        assert_eq!(b["system"], "sys");
        assert!(b["max_tokens"].as_u64().unwrap() > 8192);
        assert_eq!(b["thinking"]["budget_tokens"], 8192);
    }

    #[tokio::test]
    async fn complete_posts_invoke() {
        let payload = serde_json::json!({
            "content": [{"type": "text", "text": "b-hi"}],
            "stop_reason": "end_turn",
        });
        let (base, seen) = crate::teststub::start(payload).await;
        let p = BedrockProvider::new(
            base,
            "bk".into(),
            "anthropic.claude-3-5-sonnet-20241022-v2:0".into(),
        );
        let resp = p
            .complete(crate::ChatRequest {
                system: "s".into(),
                messages: vec![],
                tools: vec![],
                max_tokens: None,
                temperature: None,
                thinking: None,
            })
            .await
            .unwrap();
        assert_eq!(resp.message.full_text(), "b-hi");
        assert_eq!(resp.message.provider.as_deref(), Some("bedrock"));
        assert!(seen.path.lock().unwrap().contains("/model/"));
        assert!(seen.path.lock().unwrap().contains("/invoke"));
        assert_eq!(
            seen.headers
                .lock()
                .unwrap()
                .get("authorization")
                .map(String::as_str),
            Some("Bearer bk")
        );
    }
}
