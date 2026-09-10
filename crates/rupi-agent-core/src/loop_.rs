//! Low-level agent loop matching `packages/agent/src/agent-loop.ts` (Pi 0.85.1).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use rupi_ai::{
    ContentBlock, Context, FauxProvider, Message, Model, ProviderClient, StreamOptions,
};

use crate::events::AgentEvent;
use crate::tools::{ToolExecutor, ToolSet};
use crate::types::{
    AfterToolCallContext, AfterToolCallResult, AgentContext, AgentError, AgentResult,
    BeforeToolCallContext, BeforeToolCallResult, ShouldStopAfterTurnContext, ToolExecutionMode,
    TurnUpdate, default_convert_to_llm,
};

pub type EventSink = Arc<dyn Fn(AgentEvent) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

pub fn sync_sink(log: std::sync::Arc<std::sync::Mutex<Vec<AgentEvent>>>) -> EventSink {
    Arc::new(move |event| {
        log.lock().unwrap().push(event);
        Box::pin(async {})
    })
}

pub struct AgentLoopConfig {
    pub model: Model,
    pub convert_to_llm: Box<dyn Fn(&[Message]) -> Vec<Message> + Send + Sync>,
    pub transform_context: Option<Box<dyn Fn(&[Message]) -> Vec<Message> + Send + Sync>>,
    pub tool_execution: ToolExecutionMode,
    pub stream_options: StreamOptions,
    pub provider: Arc<dyn ProviderClient>,
    pub tools: ToolSet,
    pub get_steering_messages: Option<Box<dyn Fn() -> Vec<Message> + Send + Sync>>,
    pub get_follow_up_messages: Option<Box<dyn Fn() -> Vec<Message> + Send + Sync>>,
    pub before_tool_call:
        Option<Box<dyn Fn(BeforeToolCallContext) -> BeforeToolCallResult + Send + Sync>>,
    pub after_tool_call:
        Option<Box<dyn Fn(AfterToolCallContext) -> AfterToolCallResult + Send + Sync>>,
    pub should_stop_after_turn:
        Option<Box<dyn Fn(ShouldStopAfterTurnContext) -> bool + Send + Sync>>,
    pub prepare_next_turn:
        Option<Box<dyn Fn(ShouldStopAfterTurnContext) -> Option<TurnUpdate> + Send + Sync>>,
}

impl AgentLoopConfig {
    pub fn faux(model: Model, provider: Arc<FauxProvider>, tools: ToolSet) -> Self {
        Self {
            model,
            convert_to_llm: Box::new(|m| default_convert_to_llm(m)),
            transform_context: None,
            tool_execution: ToolExecutionMode::Parallel,
            stream_options: StreamOptions::default(),
            provider: provider as Arc<dyn ProviderClient>,
            tools,
            get_steering_messages: None,
            get_follow_up_messages: None,
            before_tool_call: None,
            after_tool_call: None,
            should_stop_after_turn: None,
            prepare_next_turn: None,
        }
    }
}

pub async fn agent_loop(
    prompts: Vec<Message>,
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    emit: EventSink,
) -> AgentResult<Vec<Message>> {
    let mut new_messages = prompts.clone();
    context.messages.extend(prompts.iter().cloned());

    emit(AgentEvent::AgentStart).await;
    emit(AgentEvent::TurnStart).await;
    for prompt in &prompts {
        emit(AgentEvent::MessageStart {
            message: prompt.clone(),
        })
        .await;
        emit(AgentEvent::MessageEnd {
            message: prompt.clone(),
        })
        .await;
    }

    run_loop(context, &mut new_messages, config, emit).await?;
    Ok(new_messages)
}

pub async fn agent_loop_continue(
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    emit: EventSink,
) -> AgentResult<Vec<Message>> {
    if context.messages.is_empty() {
        return Err(AgentError::Continue("no messages in context".into()));
    }
    if context.messages.last().map(|m| m.is_assistant()).unwrap_or(false) {
        return Err(AgentError::Continue(
            "cannot continue from message role: assistant".into(),
        ));
    }
    let mut new_messages = Vec::new();
    emit(AgentEvent::AgentStart).await;
    emit(AgentEvent::TurnStart).await;
    run_loop(context, &mut new_messages, config, emit).await?;
    Ok(new_messages)
}

async fn run_loop(
    current_context: &mut AgentContext,
    new_messages: &mut Vec<Message>,
    config: &AgentLoopConfig,
    emit: EventSink,
) -> AgentResult<()> {
    let mut first_turn = true;
    let mut pending_messages: Vec<Message> = config
        .get_steering_messages
        .as_ref()
        .map(|f| f())
        .unwrap_or_default();

    loop {
        let mut has_more_tool_calls = true;
        while has_more_tool_calls || !pending_messages.is_empty() {
            if !first_turn {
                emit(AgentEvent::TurnStart).await;
            } else {
                first_turn = false;
            }

            if !pending_messages.is_empty() {
                for message in pending_messages.drain(..) {
                    emit(AgentEvent::MessageStart {
                        message: message.clone(),
                    })
                    .await;
                    emit(AgentEvent::MessageEnd {
                        message: message.clone(),
                    })
                    .await;
                    current_context.messages.push(message.clone());
                    new_messages.push(message);
                }
            }

            let message = stream_assistant_response(current_context, config, &emit).await;
            current_context.messages.push(message.clone());
            new_messages.push(message.clone());

            if message
                .stop_reason()
                .map(|r| r.is_terminal_failure())
                .unwrap_or(false)
            {
                emit(AgentEvent::TurnEnd {
                    message: message.clone(),
                    tool_results: vec![],
                })
                .await;
                emit(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                })
                .await;
                return Ok(());
            }

            let tool_calls: Vec<ContentBlock> = message
                .tool_calls()
                .into_iter()
                .cloned()
                .collect();

            let mut tool_results = Vec::new();
            has_more_tool_calls = false;
            if !tool_calls.is_empty() {
                let (results, terminate) =
                    execute_tool_calls(current_context, &message, &tool_calls, config, &emit).await;
                tool_results = results;
                has_more_tool_calls = !terminate;
                for result in &tool_results {
                    current_context.messages.push(result.clone());
                    new_messages.push(result.clone());
                }
            }

            emit(AgentEvent::TurnEnd {
                message: message.clone(),
                tool_results: tool_results.clone(),
            })
            .await;

            let stop_ctx = ShouldStopAfterTurnContext {
                message: message.clone(),
                tool_results: tool_results.clone(),
                context: current_context.clone(),
                new_messages: new_messages.clone(),
            };

            if config
                .should_stop_after_turn
                .as_ref()
                .map(|f| f(stop_ctx.clone()))
                .unwrap_or(false)
            {
                emit(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                })
                .await;
                return Ok(());
            }

            if let Some(prep) = &config.prepare_next_turn {
                if let Some(update) = prep(stop_ctx) {
                    if let Some(ctx) = update.context {
                        *current_context = ctx;
                    }
                }
            }

            pending_messages = config
                .get_steering_messages
                .as_ref()
                .map(|f| f())
                .unwrap_or_default();
        }

        let follow_up = config
            .get_follow_up_messages
            .as_ref()
            .map(|f| f())
            .unwrap_or_default();
        if follow_up.is_empty() {
            emit(AgentEvent::AgentEnd {
                messages: new_messages.clone(),
            })
            .await;
            return Ok(());
        }
        pending_messages = follow_up;
        has_more_tool_calls = false;
        let _ = has_more_tool_calls;
    }
}

async fn stream_assistant_response(
    context: &AgentContext,
    config: &AgentLoopConfig,
    emit: &EventSink,
) -> Message {
    let mut messages = context.messages.clone();
    if let Some(transform) = &config.transform_context {
        messages = transform(&messages);
    }
    let llm_messages = (config.convert_to_llm)(&messages);
    let llm_context = Context {
        system_prompt: context.system_prompt.clone(),
        messages: llm_messages,
        tools: if context.tools.is_empty() {
            config.tools.definitions()
        } else {
            context.tools.clone()
        },
    };

    let message = match config
        .provider
        .complete(&config.model, &llm_context, &config.stream_options)
        .await
    {
        Ok(m) => m,
        Err(e) => Message::error(e.to_string()),
    };

    emit(AgentEvent::MessageStart {
        message: message.clone(),
    })
    .await;
    emit(AgentEvent::MessageEnd {
        message: message.clone(),
    })
    .await;
    message
}

async fn execute_tool_calls(
    _context: &AgentContext,
    assistant: &Message,
    tool_calls: &[ContentBlock],
    config: &AgentLoopConfig,
    emit: &EventSink,
) -> (Vec<Message>, bool) {
    let mut results = Vec::new();
    let mut terminate_flags = Vec::new();

    let run_one = |call: ContentBlock| async move {
        let ContentBlock::ToolCall { id, name, arguments } = &call else {
            return (Message::tool_result("", "unknown", "invalid", true), true);
        };

        emit(AgentEvent::ToolExecutionStart {
            tool_call_id: id.clone(),
            tool_name: name.clone(),
            args: arguments.clone(),
        })
        .await;

        if let Some(before) = &config.before_tool_call {
            let decision = before(BeforeToolCallContext {
                assistant_message: assistant.clone(),
                tool_call_id: id.clone(),
                tool_name: name.clone(),
                args: arguments.clone(),
            });
            if decision.block {
                let reason = decision
                    .reason
                    .unwrap_or_else(|| format!("tool `{name}` blocked by permission gate"));
                let msg = Message::tool_result(id, name, &reason, true);
                emit(AgentEvent::ToolExecutionEnd {
                    tool_call_id: id.clone(),
                    tool_name: name.clone(),
                    result: reason,
                    is_error: true,
                })
                .await;
                emit(AgentEvent::MessageStart {
                    message: msg.clone(),
                })
                .await;
                emit(AgentEvent::MessageEnd {
                    message: msg.clone(),
                })
                .await;
                return (msg, decision.terminate);
            }
        }

        let mut msg = ToolExecutor::execute(&config.tools, &call).await;
        let mut is_error = matches!(
            &msg,
            Message::ToolResult { is_error: true, .. }
        );
        let mut terminate = matches!(
            &msg,
            Message::ToolResult {
                terminate: Some(true),
                ..
            }
        );
        let mut text = match &msg {
            Message::ToolResult { content, .. } => rupi_ai::content_text(content),
            _ => String::new(),
        };

        if let Some(after) = &config.after_tool_call {
            let over = after(AfterToolCallContext {
                assistant_message: assistant.clone(),
                tool_call_id: id.clone(),
                tool_name: name.clone(),
                args: arguments.clone(),
                is_error,
                result_text: text.clone(),
            });
            if let Some(c) = over.content {
                text = c;
            }
            if let Some(e) = over.is_error {
                is_error = e;
            }
            if let Some(t) = over.terminate {
                terminate = t;
            }
            msg = Message::tool_result(id, name, &text, is_error);
            if let (Some(details), Message::ToolResult { details: d, .. }) = (over.details, &mut msg)
            {
                *d = Some(details);
            }
        }

        emit(AgentEvent::ToolExecutionEnd {
            tool_call_id: id.clone(),
            tool_name: name.clone(),
            result: text.clone(),
            is_error,
        })
        .await;
        emit(AgentEvent::MessageStart {
            message: msg.clone(),
        })
        .await;
        emit(AgentEvent::MessageEnd {
            message: msg.clone(),
        })
        .await;
        (msg, terminate)
    };

    match config.tool_execution {
        ToolExecutionMode::Sequential => {
            for call in tool_calls {
                let (msg, term) = run_one(call.clone()).await;
                terminate_flags.push(term);
                results.push(msg);
            }
        }
        ToolExecutionMode::Parallel => {
            let mut futs = Vec::new();
            for call in tool_calls {
                futs.push(run_one(call.clone()));
            }
            // Preserve assistant source order for tool-result artifacts
            // (pi: tool_execution_end may complete out of order, results emit in source order).
            for fut in futs {
                let (msg, term) = fut.await;
                terminate_flags.push(term);
                results.push(msg);
            }
        }
    }

    let terminate = !terminate_flags.is_empty() && terminate_flags.iter().all(|t| *t);
    (results, terminate)
}

pub fn noop_sink() -> EventSink {
    Arc::new(|_| Box::pin(async {}))
}
