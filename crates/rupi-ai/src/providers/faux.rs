//! Scriptable provider for tests. Returns pre-seeded assistant turns in order.

use super::openai::MpscStream;
use super::{EventStream, Provider, StreamRequest};
use crate::error::AiError;
use crate::types::{
    AssistantMessage, ContentBlock, ProviderKind, StopReason, StreamEvent,
};
use async_trait::async_trait;
use chrono::Utc;
use serde_json::Value;
use std::sync::Mutex;
use tokio::sync::mpsc;

#[derive(Clone)]
pub struct ScriptedTurn {
    pub text: Option<String>,
    pub tool_calls: Vec<ScriptedToolCall>,
    pub stop: StopReason,
}

#[derive(Clone)]
pub struct ScriptedToolCall {
    pub name: String,
    pub arguments: Value,
}

impl ScriptedTurn {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: Some(text.into()),
            tool_calls: Vec::new(),
            stop: StopReason::EndTurn,
        }
    }

    pub fn tools(calls: Vec<ScriptedToolCall>) -> Self {
        Self {
            text: None,
            tool_calls: calls,
            stop: StopReason::ToolUse,
        }
    }

    pub fn tool(name: impl Into<String>, arguments: Value) -> Self {
        Self::tools(vec![ScriptedToolCall {
            name: name.into(),
            arguments,
        }])
    }
}

#[derive(Default)]
pub struct FauxProvider {
    turns: Mutex<Vec<ScriptedTurn>>,
    /// When the script is exhausted, return this (default: empty end_turn).
    fallback_text: Mutex<Option<String>>,
}

impl FauxProvider {
    pub fn new(turns: Vec<ScriptedTurn>) -> Self {
        Self {
            turns: Mutex::new(turns),
            fallback_text: Mutex::new(None),
        }
    }

    pub fn push(&self, turn: ScriptedTurn) {
        self.turns.lock().unwrap().push(turn);
    }

    pub fn set_fallback_text(&self, text: impl Into<String>) {
        *self.fallback_text.lock().unwrap() = Some(text.into());
    }

    fn next_turn(&self) -> ScriptedTurn {
        let mut turns = self.turns.lock().unwrap();
        if turns.is_empty() {
            let fb = self.fallback_text.lock().unwrap().clone();
            return ScriptedTurn::text(fb.unwrap_or_default());
        }
        turns.remove(0)
    }
}

#[async_trait]
impl Provider for FauxProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Faux
    }

    async fn stream(&self, request: StreamRequest) -> Result<EventStream, AiError> {
        let turn = self.next_turn();
        let (tx, rx) = mpsc::channel(32);
        let model_id = request.model.id.clone();
        tokio::spawn(async move {
            let mut content = Vec::new();
            if let Some(text) = &turn.text {
                if !text.is_empty() {
                    let _ = tx.send(Ok(StreamEvent::TextDelta(text.clone()))).await;
                    content.push(ContentBlock::text(text.clone()));
                }
            }
            for call in &turn.tool_calls {
                let id = uuid::Uuid::now_v7().to_string();
                let _ = tx
                    .send(Ok(StreamEvent::ToolCallStart {
                        id: id.clone(),
                        name: call.name.clone(),
                    }))
                    .await;
                content.push(ContentBlock::ToolCall {
                    id,
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                    arguments_json: call.arguments.to_string(),
                });
            }
            let _ = tx
                .send(Ok(StreamEvent::Done(AssistantMessage {
                    content,
                    stop_reason: turn.stop,
                    usage: Default::default(),
                    error_message: None,
                    timestamp: Some(Utc::now()),
                    model: Some(model_id),
                })))
                .await;
        });
        Ok(Box::pin(MpscStream::new(rx)))
    }
}
