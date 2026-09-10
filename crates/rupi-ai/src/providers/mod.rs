pub mod anthropic;
pub mod faux;
pub mod openai;

use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures::Stream;

use crate::model::{ApiKind, Model};
use crate::types::{AiResult, AssistantEvent, Context, Message, StreamOptions};

pub use anthropic::anthropic_complete;
pub use faux::{faux_failure, inspect_last_user_text, FauxProvider, FauxScript};
pub use openai::openai_complete;

#[allow(dead_code)]
pub type EventStream = Pin<Box<dyn Stream<Item = AssistantEvent> + Send>>;

/// Stream function used by the agent loop. Matches pi-ai `StreamFn`.
/// Failures must be encoded as an AssistantMessage with stopReason error/aborted.
pub type StreamFn = Arc<
    dyn Fn(Model, Context, StreamOptions) -> Pin<Box<dyn Stream<Item = AssistantEvent> + Send>>
        + Send
        + Sync,
>;

#[async_trait]
pub trait ProviderClient: Send + Sync {
    async fn complete(
        &self,
        model: &Model,
        context: &Context,
        options: &StreamOptions,
    ) -> AiResult<Message>;
}

pub async fn complete(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
) -> AiResult<Message> {
    match model.api {
        ApiKind::Anthropic => anthropic_complete(model, context, options).await,
        ApiKind::Faux => Err(crate::AiError::Invalid(
            "faux models must be used via FauxProvider".into(),
        )),
        ApiKind::Openai | ApiKind::Google => openai_complete(model, context, options).await,
    }
}
