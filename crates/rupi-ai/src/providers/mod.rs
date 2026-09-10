pub mod anthropic;
pub mod faux;
pub mod google;
pub mod openai;

use crate::error::AiError;
use crate::types::{AssistantMessage, Message, Model, StreamEvent, StreamOptions, ToolSpec};
use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;

pub use crate::types::ProviderKind;

#[derive(Clone)]
pub struct StreamRequest {
    pub model: Model,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub options: StreamOptions,
}

pub type EventStream = Pin<Box<dyn Stream<Item = Result<StreamEvent, AiError>> + Send>>;

#[async_trait]
pub trait Provider: Send + Sync {
    fn kind(&self) -> ProviderKind;

    async fn stream(&self, request: StreamRequest) -> Result<EventStream, AiError>;

    /// Convenience: drain the stream into a final assistant message.
    async fn complete(&self, request: StreamRequest) -> Result<AssistantMessage, AiError> {
        let mut stream = self.stream(request).await?;
        use futures::StreamExt;
        let mut last: Option<AssistantMessage> = None;
        while let Some(ev) = stream.next().await {
            match ev? {
                StreamEvent::Done(msg) => last = Some(msg),
                StreamEvent::TextDelta(_)
                | StreamEvent::ThinkingDelta(_)
                | StreamEvent::ToolCallStart { .. }
                | StreamEvent::ToolCallDelta { .. }
                | StreamEvent::Usage(_) => {}
            }
        }
        last.ok_or_else(|| AiError::Stream("provider stream ended without a Done event".into()))
    }
}
