//! Unified multi-provider LLM types and streaming clients.
//!
//! Mirrors `@earendil-works/pi-ai`: a single `Message` / `AssistantMessage` shape
//! with OpenAI Chat Completions, Anthropic Messages, Google Gemini, and a
//! scriptable faux provider for tests.

mod error;
mod providers;
mod retry;
mod sse;
mod types;

pub use error::AiError;
pub use providers::{
    anthropic::AnthropicProvider,
    faux::{FauxProvider, ScriptedToolCall, ScriptedTurn},
    google::GoogleProvider,
    openai::OpenAiProvider,
    Provider, ProviderKind, StreamRequest,
};
pub use retry::{retry_with_backoff, RetryPolicy};
pub use types::*;

use std::sync::Arc;

/// Resolve a provider from a kind + credentials.
pub fn provider_for(kind: ProviderKind, api_key: Option<String>, base_url: Option<String>) -> Arc<dyn Provider> {
    match kind {
        ProviderKind::OpenAi | ProviderKind::OpenAiCompat | ProviderKind::OpenRouter => {
            Arc::new(OpenAiProvider::new(kind, api_key, base_url))
        }
        ProviderKind::Anthropic => Arc::new(AnthropicProvider::new(api_key, base_url)),
        ProviderKind::Google => Arc::new(GoogleProvider::new(api_key, base_url)),
        ProviderKind::Faux => Arc::new(FauxProvider::default()),
    }
}
