//! Unified LLM types matching `@earendil-works/pi-ai` 0.85.1.
//!
//! Wire protocols: OpenAI Chat Completions, Anthropic Messages, and a
//! deterministic faux provider used by harness tests.

mod model;
mod providers;
mod types;
mod usage;

pub use model::{get_model, list_models, ApiKind, Model, ModelCatalog, ThinkingLevel};
pub use providers::{
    anthropic_complete, complete, faux_failure, inspect_last_user_text, openai_complete, FauxProvider,
    FauxScript, ProviderClient, StreamFn,
};
pub use types::*;
pub use usage::{add_usage, estimate_tokens, Usage};

pub const UPSTREAM_PI_VERSION: &str = "0.85.1";
pub const UPSTREAM_PI_PACKAGE: &str = "@earendil-works/pi-coding-agent";

#[cfg(test)]
mod tests;
