use serde::{Deserialize, Serialize};

/// Thinking/reasoning level. `"xhigh"` and `"max"` are model-family specific
/// (same caveat as pi-ai 0.85.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    Off,
    Minimal,
    Low,
    #[default]
    Medium,
    High,
    Xhigh,
    Max,
}

impl ThinkingLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "minimal" => Some(Self::Minimal),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::Xhigh),
            "max" => Some(Self::Max),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApiKind {
    Openai,
    Anthropic,
    Google,
    Faux,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub provider: String,
    pub name: String,
    pub api: ApiKind,
    pub context_window: u32,
    pub max_tokens: u32,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

impl Model {
    pub fn new(
        provider: impl Into<String>,
        id: impl Into<String>,
        api: ApiKind,
        context_window: u32,
    ) -> Self {
        let id = id.into();
        let provider = provider.into();
        Self {
            name: id.clone(),
            id,
            provider,
            api,
            context_window,
            max_tokens: context_window.min(32_768),
            reasoning: false,
            base_url: None,
        }
    }
}

pub struct ModelCatalog {
    models: Vec<Model>,
}

impl ModelCatalog {
    pub fn builtin() -> Self {
        let mut models = Vec::new();
        models.push(openai("gpt-4o", 128_000));
        models.push(openai("gpt-4.1", 1_047_576));
        models.push(openai("gpt-4.1-mini", 1_047_576));
        models.push(openai("o4-mini", 200_000));
        models.push(openai("gpt-5", 400_000));
        models.push(anthropic("claude-sonnet-4-5", 200_000));
        models.push(anthropic("claude-opus-4-6", 200_000));
        models.push(anthropic("claude-haiku-4-5", 200_000));
        models.push(anthropic("claude-sonnet-4-6", 200_000));
        models.push(openrouter("openrouter", "anthropic/claude-sonnet-4.5", 200_000));
        models.push({
            let mut m = Model::new("faux", "faux", ApiKind::Faux, 128_000);
            m.name = "Faux (test)".into();
            m
        });
        Self { models }
    }

    pub fn get(&self, provider: &str, id: &str) -> Option<&Model> {
        self.models
            .iter()
            .find(|m| m.provider == provider && m.id == id)
    }

    pub fn list(&self) -> &[Model] {
        &self.models
    }

    pub fn resolve(&self, spec: &str) -> Option<Model> {
        if let Some((provider, id)) = spec.split_once('/') {
            return self.get(provider, id).cloned().or_else(|| {
                Some(Model::new(
                    provider,
                    id,
                    guess_api(provider),
                    128_000,
                ))
            });
        }
        self.models.iter().find(|m| m.id == spec).cloned()
    }
}

fn openai(id: &str, ctx: u32) -> Model {
    let mut m = Model::new("openai", id, ApiKind::Openai, ctx);
    m.base_url = Some("https://api.openai.com/v1".into());
    m.reasoning = id.starts_with('o') || id.starts_with("gpt-5");
    m
}

fn anthropic(id: &str, ctx: u32) -> Model {
    let mut m = Model::new("anthropic", id, ApiKind::Anthropic, ctx);
    m.base_url = Some("https://api.anthropic.com".into());
    m.reasoning = true;
    m
}

fn openrouter(provider: &str, id: &str, ctx: u32) -> Model {
    let mut m = Model::new(provider, id, ApiKind::Openai, ctx);
    m.base_url = Some("https://openrouter.ai/api/v1".into());
    m
}

fn guess_api(provider: &str) -> ApiKind {
    match provider {
        "anthropic" => ApiKind::Anthropic,
        "google" | "gemini" => ApiKind::Google,
        "faux" => ApiKind::Faux,
        _ => ApiKind::Openai,
    }
}

pub fn get_model(provider: &str, id: &str) -> Option<Model> {
    ModelCatalog::builtin().get(provider, id).cloned()
}

pub fn list_models() -> Vec<Model> {
    ModelCatalog::builtin().list().to_vec()
}

impl Default for ModelCatalog {
    fn default() -> Self {
        Self::builtin()
    }
}
