//! Google Vertex AI 显式路由：Gemini `generateContent` 形状，
//! URL 为 `{loc}-aiplatform.googleapis.com/.../publishers/google/models/{model}:verb`，
//! 鉴权 Bearer（`VERTEX_TOKEN` / `GOOGLE_OAUTH_ACCESS_TOKEN` / `--api-key`）。

use async_trait::async_trait;

#[derive(Debug, Clone)]
pub struct VertexProvider {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    client: reqwest::Client,
}

impl VertexProvider {
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
            .or_else(|| std::env::var("VERTEX_TOKEN").ok())
            .or_else(|| std::env::var("GOOGLE_OAUTH_ACCESS_TOKEN").ok())
            .or_else(|| std::env::var("RUPI_VERTEX_KEY").ok())
            .or_else(|| std::env::var("GOOGLE_API_KEY").ok())
            .ok_or_else(|| {
                anyhow::anyhow!("set VERTEX_TOKEN or GOOGLE_OAUTH_ACCESS_TOKEN for vertex")
            })?;
        let project = std::env::var("GOOGLE_CLOUD_PROJECT")
            .or_else(|_| std::env::var("VERTEX_PROJECT"))
            .or_else(|_| std::env::var("GCLOUD_PROJECT"))
            .unwrap_or_else(|_| "default".to_string());
        let location = std::env::var("VERTEX_LOCATION")
            .or_else(|_| std::env::var("GOOGLE_CLOUD_LOCATION"))
            .unwrap_or_else(|_| "us-central1".to_string());
        let base = std::env::var("RUPI_VERTEX_BASE").unwrap_or_else(|_| {
            format!("https://{location}-aiplatform.googleapis.com/v1/projects/{project}/locations/{location}/publishers/google")
        });
        Ok(Self::new(base, key, model))
    }

    fn url(&self, stream: bool) -> String {
        let verb = if stream {
            "streamGenerateContent"
        } else {
            "generateContent"
        };
        format!(
            "{}/models/{}:{verb}",
            self.base_url.trim_end_matches('/'),
            self.model
        )
    }

    fn body(&self, req: &super::ChatRequest) -> serde_json::Value {
        let mut m = serde_json::Map::new();
        m.insert(
            "system_instruction".into(),
            serde_json::json!({"parts": [{"text": req.system}]}),
        );
        m.insert(
            "contents".into(),
            serde_json::Value::Array(crate::gemini::to_gemini_contents(&req.messages)),
        );
        m.insert(
            "tools".into(),
            serde_json::json!([{"functionDeclarations": crate::gemini::to_gemini_tools(&req.tools)}]),
        );
        let mut gen = serde_json::Map::new();
        gen.insert("temperature".into(), req.temperature.unwrap_or(0.2).into());
        gen.insert(
            "maxOutputTokens".into(),
            req.max_tokens.unwrap_or(4096).into(),
        );
        if let Some(level) = req.thinking.and_then(|t| t.gemini_level()) {
            gen.insert(
                "thinkingConfig".into(),
                serde_json::json!({"thinkingLevel": level}),
            );
        }
        m.insert("generationConfig".into(), serde_json::Value::Object(gen));
        serde_json::Value::Object(m)
    }
}

#[async_trait]
impl super::LlmProvider for VertexProvider {
    fn name(&self) -> &str {
        "vertex"
    }

    fn model_id(&self) -> Option<&str> {
        Some(&self.model)
    }

    async fn complete(&self, req: super::ChatRequest) -> anyhow::Result<super::ChatResponse> {
        let url = self.url(false);
        let body = self.body(&req);
        let key = self.api_key.clone();
        let resp = super::post_json_with_retry(
            || {
                self.client
                    .post(url.clone())
                    .header("Authorization", format!("Bearer {key}"))
            },
            &body,
            3,
        )
        .await?;
        let status = resp.status();
        let v: serde_json::Value = resp.json().await?;
        if !status.is_success() {
            let msg = crate::gemini::error_text(&v).unwrap_or("vertex request failed");
            anyhow::bail!("vertex {status}: {msg}");
        }
        let mut parsed = crate::gemini::parse_gemini_response(v)?;
        parsed.message.provider = Some("vertex".into());
        Ok(parsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LlmProvider;

    #[test]
    fn url_uses_vertex_publishers_path() {
        let p = VertexProvider::new(
            "https://us-central1-aiplatform.googleapis.com/v1/projects/p/locations/us-central1/publishers/google".into(),
            "tok".into(),
            "gemini-2.5-pro".into(),
        );
        assert_eq!(
            p.url(false),
            "https://us-central1-aiplatform.googleapis.com/v1/projects/p/locations/us-central1/publishers/google/models/gemini-2.5-pro:generateContent"
        );
    }

    #[tokio::test]
    async fn complete_posts_generate_content() {
        let payload = serde_json::json!({
            "candidates": [{
                "content": {"parts": [{"text": "v-hi"}], "role": "model"},
                "finishReason": "STOP",
            }],
        });
        let (base, seen) = crate::teststub::start(payload).await;
        let p = VertexProvider::new(base, "vt".into(), "gemini-2.5-pro".into());
        let resp = p
            .complete(crate::ChatRequest {
                system: "sys".into(),
                messages: vec![],
                tools: vec![],
                max_tokens: None,
                temperature: None,
                thinking: Some(crate::ThinkingLevel::XHigh),
            })
            .await
            .unwrap();
        assert_eq!(resp.message.full_text(), "v-hi");
        assert_eq!(resp.message.provider.as_deref(), Some("vertex"));
        assert!(seen
            .path
            .lock()
            .unwrap()
            .contains("/models/gemini-2.5-pro:generateContent"));
        assert_eq!(
            seen.headers
                .lock()
                .unwrap()
                .get("authorization")
                .map(String::as_str),
            Some("Bearer vt")
        );
        let body = seen.body.lock().unwrap();
        assert_eq!(
            body.pointer("/generationConfig/thinkingConfig/thinkingLevel"),
            Some(&serde_json::json!("HIGH"))
        );
    }
}
