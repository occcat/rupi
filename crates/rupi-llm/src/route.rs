//! `provider/model[:thinking]` 解析与显式路由（Azure / Bedrock / Vertex / OpenRouter）。

use super::{
    catalog, AnthropicProvider, CompatKind, GeminiProvider, LlmProvider, MockProvider,
    OpenAiCompatProvider, ThinkingLevel,
};

/// `--api-key` / `--provider` 覆盖（环境变量仍是缺省）。
#[derive(Debug, Clone, Default)]
pub struct ProviderOptions {
    pub api_key: Option<String>,
    pub provider: Option<String>,
}

/// 解析后的模型引用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSpec {
    /// 显式 provider（`openai`/`anthropic`/…）；None 则按模型名前缀猜。
    pub provider: Option<String>,
    pub model: String,
    pub thinking: Option<ThinkingLevel>,
}

const KNOWN_PROVIDERS: &[&str] = &[
    "openai",
    "openai-compat",
    "anthropic",
    "gemini",
    "openrouter",
    "azure",
    "bedrock",
    "vertex",
];

/// 拆 `provider/model[:thinking]`。最后一段若是思考档才剥（Bedrock 的 `:0` 保留）。
pub fn parse_model_spec(input: &str) -> ModelSpec {
    let raw = input.trim();
    let (rest, thinking) = split_thinking_suffix(raw);
    let (provider, model) = split_provider_prefix(rest);
    ModelSpec {
        provider,
        model,
        thinking,
    }
}

fn split_thinking_suffix(s: &str) -> (&str, Option<ThinkingLevel>) {
    let Some((head, tail)) = s.rsplit_once(':') else {
        return (s, None);
    };
    if tail.is_empty() || head.is_empty() {
        return (s, None);
    }
    match tail.trim().to_lowercase().parse::<ThinkingLevel>() {
        Ok(t) => (head, Some(t)),
        Err(_) => (s, None),
    }
}

pub fn is_known_provider(name: &str) -> bool {
    KNOWN_PROVIDERS.iter().any(|p| p.eq_ignore_ascii_case(name))
        || catalog::find_extra_provider(name).is_some()
}

fn split_provider_prefix(s: &str) -> (Option<String>, String) {
    let Some((head, tail)) = s.split_once('/') else {
        return (None, s.to_string());
    };
    if is_known_provider(head) && !tail.is_empty() {
        return (Some(head.to_ascii_lowercase()), tail.to_string());
    }
    (None, s.to_string())
}

fn pick_key(override_key: Option<&str>, names: &[&str]) -> anyhow::Result<String> {
    if let Some(k) = override_key {
        if !k.is_empty() {
            return Ok(k.to_string());
        }
    }
    for n in names {
        if let Ok(v) = std::env::var(n) {
            if !v.is_empty() {
                return Ok(v);
            }
        }
    }
    anyhow::bail!("set {}", names.join(" or "))
}

fn infer_provider(model: &str) -> &'static str {
    if model.starts_with("claude-") {
        "anthropic"
    } else if model.starts_with("gemini-") {
        "gemini"
    } else {
        "openai"
    }
}

/// 按 spec + 选项构造 provider。缺 key 返回 Err（调用方回 mock）。
pub fn provider_from_spec(
    spec: &ModelSpec,
    opts: &ProviderOptions,
) -> anyhow::Result<Box<dyn LlmProvider>> {
    let forced = opts
        .provider
        .as_deref()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty());
    let provider = forced
        .or_else(|| spec.provider.clone())
        .unwrap_or_else(|| infer_provider(&spec.model).to_string());
    let key = opts.api_key.as_deref();
    if let Some(extra) = catalog::find_extra_provider(&provider) {
        return provider_from_extra(&extra, spec, opts);
    }
    match provider.as_str() {
        "anthropic" => {
            let api_key = pick_key(key, &["RUPI_ANTHROPIC_KEY", "ANTHROPIC_API_KEY"])?;
            let base = std::env::var("RUPI_ANTHROPIC_BASE")
                .unwrap_or_else(|_| crate::anthropic::DEFAULT_BASE_URL.to_string());
            Ok(Box::new(AnthropicProvider::new(
                base,
                api_key,
                spec.model.clone(),
            )))
        }
        "gemini" => {
            let api_key = pick_key(
                key,
                &["RUPI_GEMINI_KEY", "GEMINI_API_KEY", "GOOGLE_API_KEY"],
            )?;
            let base = std::env::var("RUPI_GEMINI_BASE")
                .unwrap_or_else(|_| crate::gemini::DEFAULT_BASE_URL.to_string());
            Ok(Box::new(GeminiProvider::new(
                base,
                api_key,
                spec.model.clone(),
            )))
        }
        "openrouter" => {
            let api_key = pick_key(
                key,
                &[
                    "RUPI_OPENROUTER_KEY",
                    "OPENROUTER_API_KEY",
                    "RUPI_API_KEY",
                    "OPENAI_API_KEY",
                ],
            )?;
            let base = std::env::var("RUPI_OPENROUTER_BASE")
                .or_else(|_| std::env::var("OPENROUTER_BASE_URL"))
                .unwrap_or_else(|_| "https://openrouter.ai/api/v1".to_string());
            Ok(Box::new(
                OpenAiCompatProvider::new(base, api_key, spec.model.clone())
                    .with_kind(CompatKind::OpenRouter)
                    .with_session_affinity(true),
            ))
        }
        "azure" => {
            let api_key = pick_key(
                key,
                &[
                    "AZURE_OPENAI_API_KEY",
                    "RUPI_AZURE_KEY",
                    "RUPI_API_KEY",
                    "OPENAI_API_KEY",
                ],
            )?;
            let base = std::env::var("AZURE_OPENAI_ENDPOINT")
                .or_else(|_| std::env::var("RUPI_AZURE_BASE"))
                .or_else(|_| std::env::var("RUPI_BASE_URL"))
                .map_err(|_| {
                    anyhow::anyhow!("set AZURE_OPENAI_ENDPOINT or RUPI_AZURE_BASE for azure")
                })?;
            let ver = std::env::var("AZURE_OPENAI_API_VERSION")
                .or_else(|_| std::env::var("RUPI_AZURE_API_VERSION"))
                .unwrap_or_else(|_| "2024-10-21".to_string());
            Ok(Box::new(
                OpenAiCompatProvider::new(base, api_key, spec.model.clone())
                    .with_kind(CompatKind::Azure)
                    .with_api_version(ver),
            ))
        }
        "bedrock" => Ok(Box::new(crate::bedrock::BedrockProvider::from_options(
            spec.model.clone(),
            key,
        )?)),
        "vertex" => Ok(Box::new(crate::vertex::VertexProvider::from_options(
            spec.model.clone(),
            key,
        )?)),
        "openai" | "openai-compat" => {
            let api_key = pick_key(key, &["RUPI_API_KEY", "OPENAI_API_KEY"])?;
            let base = std::env::var("RUPI_BASE_URL")
                .or_else(|_| std::env::var("OPENAI_BASE_URL"))
                .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
            Ok(Box::new(OpenAiCompatProvider::new(
                base,
                api_key,
                spec.model.clone(),
            )))
        }
        other => anyhow::bail!(
            "unknown provider '{other}' (openai|anthropic|gemini|openrouter|azure|bedrock|vertex)"
        ),
    }
}

fn provider_from_extra(
    extra: &catalog::ExtraProvider,
    spec: &ModelSpec,
    opts: &ProviderOptions,
) -> anyhow::Result<Box<dyn LlmProvider>> {
    let key = opts.api_key.as_deref();
    match extra.protocol.as_str() {
        "anthropic" => {
            let mut names = vec!["RUPI_ANTHROPIC_KEY", "ANTHROPIC_API_KEY"];
            if let Some(e) = extra.api_key_env.as_deref() {
                names.insert(0, e);
            }
            let api_key = pick_key(key, &names)?;
            Ok(Box::new(AnthropicProvider::new(
                extra.base_url.clone(),
                api_key,
                spec.model.clone(),
            )))
        }
        "gemini" => {
            let mut names = vec!["RUPI_GEMINI_KEY", "GEMINI_API_KEY", "GOOGLE_API_KEY"];
            if let Some(e) = extra.api_key_env.as_deref() {
                names.insert(0, e);
            }
            let api_key = pick_key(key, &names)?;
            Ok(Box::new(GeminiProvider::new(
                extra.base_url.clone(),
                api_key,
                spec.model.clone(),
            )))
        }
        _ => {
            let mut names = vec!["RUPI_API_KEY", "OPENAI_API_KEY"];
            if let Some(e) = extra.api_key_env.as_deref() {
                names.insert(0, e);
            }
            let api_key = pick_key(key, &names)?;
            Ok(Box::new(OpenAiCompatProvider::new(
                extra.base_url.clone(),
                api_key,
                spec.model.clone(),
            )))
        }
    }
}

/// 缺 key 时回 Mock（RPC/TUI 切模型不因环境缺钥失败）。
pub fn provider_or_mock(model: &str, opts: &ProviderOptions) -> Box<dyn LlmProvider> {
    match provider_from_spec(&parse_model_spec(model), opts) {
        Ok(p) => p,
        Err(_) => Box::new(MockProvider::new(vec![MockProvider::text_response(
            &format!("demo mode: missing key for {model}"),
        )])),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_provider_model_thinking() {
        let a = parse_model_spec("anthropic/claude-sonnet-4-5:high");
        assert_eq!(a.provider.as_deref(), Some("anthropic"));
        assert_eq!(a.model, "claude-sonnet-4-5");
        assert_eq!(a.thinking, Some(ThinkingLevel::High));

        let b = parse_model_spec("openrouter/anthropic/claude-3.5-sonnet:xhigh");
        assert_eq!(b.provider.as_deref(), Some("openrouter"));
        assert_eq!(b.model, "anthropic/claude-3.5-sonnet");
        assert_eq!(b.thinking, Some(ThinkingLevel::XHigh));

        let c = parse_model_spec("bedrock/anthropic.claude-3-5-sonnet-20241022-v2:0");
        assert_eq!(c.provider.as_deref(), Some("bedrock"));
        assert_eq!(c.model, "anthropic.claude-3-5-sonnet-20241022-v2:0");
        assert!(c.thinking.is_none());

        let d = parse_model_spec("gpt-4o-mini");
        assert!(d.provider.is_none());
        assert_eq!(d.model, "gpt-4o-mini");

        let e = parse_model_spec("claude-sonnet-4-5:max");
        assert!(e.provider.is_none());
        assert_eq!(e.model, "claude-sonnet-4-5");
        assert_eq!(e.thinking, Some(ThinkingLevel::Max));
    }

    #[test]
    fn provider_from_spec_routes_explicit() {
        let saved: Vec<(&str, Option<String>)> = [
            "RUPI_ANTHROPIC_KEY",
            "ANTHROPIC_API_KEY",
            "RUPI_GEMINI_KEY",
            "GEMINI_API_KEY",
            "GOOGLE_API_KEY",
            "RUPI_API_KEY",
            "OPENAI_API_KEY",
            "OPENROUTER_API_KEY",
            "RUPI_OPENROUTER_KEY",
            "AZURE_OPENAI_API_KEY",
            "RUPI_AZURE_KEY",
            "AZURE_OPENAI_ENDPOINT",
            "RUPI_AZURE_BASE",
            "AWS_BEARER_TOKEN_BEDROCK",
            "BEDROCK_API_KEY",
            "VERTEX_TOKEN",
            "GOOGLE_OAUTH_ACCESS_TOKEN",
        ]
        .iter()
        .map(|k| (*k, std::env::var(k).ok()))
        .collect();
        for (k, _) in &saved {
            unsafe { std::env::remove_var(k) };
        }

        let expect_err = |spec: &str| match provider_from_spec(
            &parse_model_spec(spec),
            &ProviderOptions::default(),
        ) {
            Ok(_) => panic!("{spec}: expected missing-key error"),
            Err(e) => e.to_string(),
        };
        let e = expect_err("anthropic/claude-x");
        assert!(e.contains("ANTHROPIC_API_KEY"), "{e}");
        let e = expect_err("openrouter/openai/gpt-4o");
        assert!(
            e.contains("OPENROUTER") || e.contains("RUPI_API_KEY"),
            "{e}"
        );
        let e = expect_err("azure/gpt-4o");
        assert!(e.contains("AZURE") || e.contains("RUPI_API_KEY"), "{e}");
        let e = expect_err("bedrock/x");
        assert!(e.contains("BEDROCK") || e.contains("AWS_BEARER"), "{e}");
        let e = expect_err("vertex/gemini-2.5-pro");
        assert!(
            e.contains("VERTEX") || e.contains("GOOGLE") || e.contains("GEMINI"),
            "{e}"
        );

        // --api-key 压过缺 env
        let p = provider_from_spec(
            &parse_model_spec("openai/gpt-4o-mini"),
            &ProviderOptions {
                api_key: Some("k".into()),
                provider: None,
            },
        )
        .unwrap();
        assert_eq!(p.name(), "openai-compat");

        for (k, v) in saved {
            if let Some(val) = v {
                unsafe { std::env::set_var(k, val) };
            }
        }
    }
}
