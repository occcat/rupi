//! Agent loop matching Pi's `runLoop`: stream assistant → execute tools →
//! inject steering → repeat until no tool calls and no queued follow-ups.

use crate::compaction::{should_compact, CompactionSettings};
use crate::events::AgentEvent;
use crate::messages::convert_to_llm;
use crate::tools::{ToolContext, ToolRegistry, ToolResult};
use futures::StreamExt;
use rupi_ai::{
    estimate_messages_tokens, AssistantMessage, ContentBlock, Message, Model, Provider, StopReason,
    StreamEvent, StreamOptions, StreamRequest,
};
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolExecutionMode {
    Sequential,
    Parallel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueMode {
    All,
    OneAtATime,
}

pub struct AgentLoopConfig {
    pub model: Model,
    pub provider: Arc<dyn Provider>,
    pub tools: ToolRegistry,
    pub system: String,
    pub tool_ctx: ToolContext,
    pub tool_execution: ToolExecutionMode,
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    pub compaction: CompactionSettings,
    pub stream_options: StreamOptions,
    pub inbox: Arc<Mutex<Vec<Message>>>,
    pub abort: Option<tokio::sync::watch::Receiver<bool>>,
}

impl AgentLoopConfig {
    pub fn new(model: Model, provider: Arc<dyn Provider>, tools: ToolRegistry, system: String, cwd: impl Into<std::path::PathBuf>) -> Self {
        Self {
            model,
            provider,
            tools,
            system,
            tool_ctx: ToolContext::new(cwd),
            tool_execution: ToolExecutionMode::Parallel,
            steering_mode: QueueMode::All,
            follow_up_mode: QueueMode::All,
            compaction: CompactionSettings::default(),
            stream_options: StreamOptions::default(),
            inbox: Arc::new(Mutex::new(Vec::new())),
            abort: None,
        }
    }
}

/// Push a steering / follow-up message into a running loop.
    #[allow(dead_code)]
    pub fn queue_message(inbox: &Arc<Mutex<Vec<Message>>>, message: Message) {
    // called from other tasks
    let inbox = inbox.clone();
    tokio::spawn(async move {
        inbox.lock().await.push(message);
    });
}

pub async fn run_agent_loop(
    prompts: Vec<Message>,
    history: Vec<Message>,
    config: &AgentLoopConfig,
    mut emit: impl FnMut(AgentEvent),
) -> Vec<Message> {
    let mut new_messages = prompts.clone();
    let mut messages = history;
    messages.extend(prompts);

    emit(AgentEvent::AgentStart);
    emit(AgentEvent::TurnStart);
    for prompt in &new_messages {
        emit(AgentEvent::MessageStart {
            role: role_name(prompt).into(),
        });
        emit(AgentEvent::MessageEnd {
            message: prompt.clone(),
        });
    }

    let mut total_usage = rupi_ai::Usage::default();

    loop {
        if aborted(config) {
            emit(AgentEvent::Error {
                message: "aborted".into(),
            });
            emit(AgentEvent::AgentEnd {
                messages: new_messages.clone(),
                usage: total_usage.clone(),
            });
            return new_messages;
        }

        // Drain steering messages before the next model call.
        let pending = drain_inbox(&config.inbox, config.steering_mode).await;
        for msg in pending {
            emit(AgentEvent::MessageStart {
                role: role_name(&msg).into(),
            });
            emit(AgentEvent::MessageEnd { message: msg.clone() });
            messages.push(msg.clone());
            new_messages.push(msg);
        }

        if config.compaction.enabled {
            if should_compact(
                estimate_messages_tokens(&messages),
                config.model.context_window,
                &config.compaction,
            ) {
                match crate::compaction::compact_messages(
                    &messages,
                    &config.system,
                    config.provider.as_ref(),
                    &config.model,
                    &config.compaction,
                )
                .await
                {
                    Ok(result) => {
                        emit(AgentEvent::Compaction {
                            summary: result.summary.clone(),
                            tokens_before: result.tokens_before,
                        });
                        messages = result.replacement_messages;
                    }
                    Err(e) => {
                        tracing::warn!("compaction failed: {e}");
                    }
                }
            }
        }

        let request = StreamRequest {
            model: config.model.clone(),
            system: Some(config.system.clone()),
            messages: convert_to_llm(&messages),
            tools: config.tools.specs(),
            options: {
                let mut opts = config.stream_options.clone();
                opts.abort = config.abort.clone();
                opts
            },
        };

        let stream = match config.provider.stream(request).await {
            Ok(s) => s,
            Err(e) => {
                let msg = AssistantMessage::error(e.to_string());
                emit(AgentEvent::Error {
                    message: e.to_string(),
                });
                emit(AgentEvent::TurnEnd {
                    message: msg.clone(),
                });
                let wrapped = Message::Assistant(msg);
                messages.push(wrapped.clone());
                new_messages.push(wrapped);
                emit(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                    usage: total_usage.clone(),
                });
                return new_messages;
            }
        };

        futures::pin_mut!(stream);
        let mut assistant: Option<AssistantMessage> = None;
        while let Some(ev) = stream.next().await {
            match ev {
                Ok(StreamEvent::TextDelta(t)) => emit(AgentEvent::TextDelta { text: t }),
                Ok(StreamEvent::ThinkingDelta(t)) => emit(AgentEvent::ThinkingDelta { text: t }),
                Ok(StreamEvent::ToolCallStart { id, name }) => {
                    emit(AgentEvent::ToolCallStart { id, name })
                }
                Ok(StreamEvent::Usage(u)) => total_usage.add(&u),
                Ok(StreamEvent::Done(msg)) => {
                    total_usage.add(&msg.usage);
                    assistant = Some(msg);
                }
                Ok(StreamEvent::ToolCallDelta { .. }) => {}
                Err(e) => {
                    emit(AgentEvent::Error {
                        message: e.to_string(),
                    });
                    assistant = Some(AssistantMessage::error(e.to_string()));
                    break;
                }
            }
        }

        let mut assistant = assistant.unwrap_or_else(|| AssistantMessage::error("empty stream"));
        if aborted(config) {
            assistant.stop_reason = StopReason::Aborted;
        }

        emit(AgentEvent::MessageStart {
            role: "assistant".into(),
        });
        emit(AgentEvent::MessageEnd {
            message: Message::Assistant(assistant.clone()),
        });

        messages.push(Message::Assistant(assistant.clone()));
        new_messages.push(Message::Assistant(assistant.clone()));

        if matches!(
            assistant.stop_reason,
            StopReason::Error | StopReason::Aborted
        ) {
            emit(AgentEvent::TurnEnd {
                message: assistant.clone(),
            });
            emit(AgentEvent::AgentEnd {
                messages: new_messages.clone(),
                usage: total_usage.clone(),
            });
            return new_messages;
        }

        let tool_calls: Vec<(String, String, serde_json::Value)> = assistant
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolCall {
                    id,
                    name,
                    arguments,
                    arguments_json,
                } => {
                    let args = if arguments.is_null() && !arguments_json.is_empty() {
                        serde_json::from_str(arguments_json).unwrap_or(serde_json::json!({}))
                    } else {
                        arguments.clone()
                    };
                    Some((id.clone(), name.clone(), args))
                }
                _ => None,
            })
            .collect();

        if assistant.stop_reason == StopReason::Length && !tool_calls.is_empty() {
            for (id, name, _) in &tool_calls {
                let result = Message::tool_result(
                    id.clone(),
                    name.clone(),
                    "tool call aborted: assistant output was truncated (stop reason length)".into(),
                    true,
                );
                emit(AgentEvent::ToolExecutionEnd {
                    id: id.clone(),
                    name: name.clone(),
                    is_error: true,
                    preview: "truncated".into(),
                });
                messages.push(result.clone());
                new_messages.push(result);
            }
            emit(AgentEvent::TurnEnd {
                message: assistant.clone(),
            });
            continue;
        }

        if tool_calls.is_empty() {
            emit(AgentEvent::TurnEnd {
                message: assistant.clone(),
            });
            let follow = drain_inbox(&config.inbox, config.follow_up_mode).await;
            if follow.is_empty() {
                emit(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                    usage: total_usage.clone(),
                });
                return new_messages;
            }
            for msg in follow {
                messages.push(msg.clone());
                new_messages.push(msg);
            }
            emit(AgentEvent::TurnStart);
            continue;
        }

        let results = execute_tools(config, &tool_calls, &mut emit).await;
        for ((id, name, _), result) in tool_calls.iter().zip(results.into_iter()) {
            let preview: String = result.content.chars().take(200).collect();
            emit(AgentEvent::ToolExecutionEnd {
                id: id.clone(),
                name: name.clone(),
                is_error: result.is_error,
                preview,
            });
            let msg = Message::tool_result(id.clone(), name.clone(), result.content, result.is_error);
            messages.push(msg.clone());
            new_messages.push(msg);
        }

        emit(AgentEvent::TurnEnd {
            message: assistant.clone(),
        });
        emit(AgentEvent::TurnStart);
    }
}

fn aborted(config: &AgentLoopConfig) -> bool {
    config.abort.as_ref().map(|r| *r.borrow()).unwrap_or(false)
}

fn role_name(msg: &Message) -> &'static str {
    match msg {
        Message::System { .. } => "system",
        Message::User { .. } => "user",
        Message::Assistant(_) => "assistant",
        Message::Tool { .. } => "tool",
    }
}

async fn drain_inbox(inbox: &Arc<Mutex<Vec<Message>>>, mode: QueueMode) -> Vec<Message> {
    let mut guard = inbox.lock().await;
    if guard.is_empty() {
        return Vec::new();
    }
    match mode {
        QueueMode::All => std::mem::take(&mut *guard),
        QueueMode::OneAtATime => vec![guard.remove(0)],
    }
}

async fn execute_tools(
    config: &AgentLoopConfig,
    calls: &[(String, String, serde_json::Value)],
    emit: &mut impl FnMut(AgentEvent),
) -> Vec<ToolResult> {
    match config.tool_execution {
        ToolExecutionMode::Sequential => {
            let mut out = Vec::new();
            for (id, name, args) in calls {
                emit(AgentEvent::ToolExecutionStart {
                    id: id.clone(),
                    name: name.clone(),
                    args: args.clone(),
                });
                out.push(run_one(config, name, args.clone()).await);
            }
            out
        }
        ToolExecutionMode::Parallel => {
            for (id, name, args) in calls {
                emit(AgentEvent::ToolExecutionStart {
                    id: id.clone(),
                    name: name.clone(),
                    args: args.clone(),
                });
            }
            let futs: Vec<_> = calls
                .iter()
                .map(|(_, name, args)| run_one(config, name, args.clone()))
                .collect();
            futures::future::join_all(futs).await
        }
    }
}

async fn run_one(config: &AgentLoopConfig, name: &str, args: serde_json::Value) -> ToolResult {
    match config.tools.get(name) {
        Some(tool) => tool.execute(args, &config.tool_ctx).await,
        None => ToolResult::err(format!("unknown tool: {name}")),
    }
}

/// Wait helper so callers can block until a Notify fires (steering).
    #[allow(dead_code)]
    pub fn new_wake() -> Arc<Notify> {
    Arc::new(Notify::new())
}
