use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::Value;

use crate::model::Model;
use crate::types::{
    AiError, AiResult, ContentBlock, Context, Message, StopReason, StreamOptions,
};
use crate::usage::Usage;

use super::ProviderClient;

/// Scripted assistant response for deterministic harness tests (pi `registerFauxProvider`).
#[derive(Debug, Clone)]
pub enum FauxScript {
    Text(String),
    ToolCalls(Vec<(String, String, Value)>),
    Error(String),
    Sequence(Vec<FauxScript>),
}

impl FauxScript {
    pub fn text(t: impl Into<String>) -> Self {
        Self::Text(t.into())
    }

    pub fn tool(name: impl Into<String>, arguments: Value) -> Self {
        Self::ToolCalls(vec![("call_1".into(), name.into(), arguments)])
    }
}

pub struct FauxProvider {
    scripts: Mutex<VecDeque<FauxScript>>,
}

impl FauxProvider {
    pub fn new(scripts: impl IntoIterator<Item = FauxScript>) -> Self {
        Self {
            scripts: Mutex::new(scripts.into_iter().collect()),
        }
    }

    pub fn push(&self, script: FauxScript) {
        self.scripts.lock().unwrap().push_back(script);
    }

    pub fn next_message(&self, model: &Model) -> Message {
        let script = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(FauxScript::Text(String::new()));
        script_to_message(&script, model)
    }
}

fn script_to_message(script: &FauxScript, model: &Model) -> Message {
    match script {
        FauxScript::Text(text) => Message::Assistant {
            content: vec![ContentBlock::text(text)],
            stop_reason: StopReason::Stop,
            usage: Usage {
                input: 10,
                output: estimate_output(text),
                ..Default::default()
            },
            error_message: None,
            timestamp: crate::types::now_ms(),
            model: Some(model.id.clone()),
            provider: Some(model.provider.clone()),
        },
        FauxScript::ToolCalls(calls) => {
            let content = calls
                .iter()
                .map(|(id, name, args)| ContentBlock::tool_call(id, name, args.clone()))
                .collect();
            Message::Assistant {
                content,
                stop_reason: StopReason::ToolUse,
                usage: Usage {
                    input: 10,
                    output: 20,
                    ..Default::default()
                },
                error_message: None,
                timestamp: crate::types::now_ms(),
                model: Some(model.id.clone()),
                provider: Some(model.provider.clone()),
            }
        }
        FauxScript::Error(msg) => Message::error(msg),
        FauxScript::Sequence(items) => items
            .first()
            .map(|s| script_to_message(s, model))
            .unwrap_or_else(|| Message::assistant_text("")),
    }
}

fn estimate_output(text: &str) -> u32 {
    (text.len() as u32 / 4).max(1)
}

#[async_trait]
impl ProviderClient for FauxProvider {
    async fn complete(
        &self,
        model: &Model,
        _context: &Context,
        _options: &StreamOptions,
    ) -> AiResult<Message> {
        Ok(self.next_message(model))
    }
}

pub fn inspect_last_user_text(context: &Context) -> Option<String> {
    context.messages.iter().rev().find_map(|m| match m {
        Message::User { content, .. } => Some(crate::content_text(content)),
        _ => None,
    })
}

/// Build a faux error encoded as a message rather than a thrown result,
/// matching pi StreamFn contract.
pub fn faux_failure(err: AiError) -> Message {
    match err {
        AiError::Aborted => Message::aborted("aborted"),
        other => Message::error(other.to_string()),
    }
}
