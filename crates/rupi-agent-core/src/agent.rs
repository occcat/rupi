use std::sync::Arc;

use rupi_ai::{FauxProvider, Message, Model, ThinkingLevel};

use crate::events::AgentEvent;
use crate::loop_::{agent_loop, agent_loop_continue, noop_sink, sync_sink, AgentLoopConfig, EventSink};
use crate::permissions::{PermissionDecision, PermissionGate};
use crate::queue::{MessageQueue, QueueMode};
use crate::tools::ToolSet;
use crate::types::{AgentContext, AgentError, AgentResult, BeforeToolCallResult};

/// High-level Agent with state, steering, and follow-up queues.
/// Matches pi-agent-core `Agent`.
pub struct Agent {
    pub context: AgentContext,
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    pub tools: ToolSet,
    pub streaming: bool,
    steering: Arc<MessageQueue>,
    follow_up: Arc<MessageQueue>,
    events: Arc<std::sync::Mutex<Vec<AgentEvent>>>,
    provider: Arc<FauxProvider>,
    use_faux: bool,
    live_provider: Option<Arc<dyn rupi_ai::ProviderClient>>,
    gate: Option<PermissionGate>,
}

impl Agent {
    pub fn new(system_prompt: impl Into<String>, model: Model) -> Self {
        Self {
            context: AgentContext::new(system_prompt),
            model,
            thinking_level: ThinkingLevel::Medium,
            tools: ToolSet::new(),
            streaming: false,
            steering: Arc::new(MessageQueue::new(QueueMode::All)),
            follow_up: Arc::new(MessageQueue::new(QueueMode::All)),
            events: Arc::new(std::sync::Mutex::new(Vec::new())),
            provider: Arc::new(FauxProvider::new([])),
            use_faux: true,
            live_provider: None,
            gate: None,
        }
    }

    pub fn with_faux(mut self, provider: Arc<FauxProvider>) -> Self {
        self.provider = provider;
        self.use_faux = true;
        self
    }

    pub fn with_provider(mut self, provider: Arc<dyn rupi_ai::ProviderClient>) -> Self {
        self.live_provider = Some(provider);
        self.use_faux = false;
        self
    }

    pub fn with_gate(mut self, gate: PermissionGate) -> Self {
        self.gate = Some(gate);
        self
    }

    pub fn provider(&self) -> Arc<dyn rupi_ai::ProviderClient> {
        if self.use_faux {
            self.provider.clone() as Arc<dyn rupi_ai::ProviderClient>
        } else if let Some(p) = &self.live_provider {
            p.clone()
        } else {
            self.provider.clone() as Arc<dyn rupi_ai::ProviderClient>
        }
    }

    pub fn set_tools(&mut self, tools: ToolSet) {
        self.context.tools = tools.definitions();
        self.tools = tools;
    }

    pub fn steer(&self, message: Message) {
        self.steering.push(message);
    }

    pub fn follow_up(&self, message: Message) {
        self.follow_up.push(message);
    }

    pub fn events(&self) -> Vec<AgentEvent> {
        self.events.lock().unwrap().clone()
    }

    pub fn messages(&self) -> &[Message] {
        &self.context.messages
    }

    fn config(&self) -> AgentLoopConfig {
        let steering = Arc::clone(&self.steering);
        let follow_up = Arc::clone(&self.follow_up);
        let provider: Arc<dyn rupi_ai::ProviderClient> = if self.use_faux {
            self.provider.clone() as Arc<dyn rupi_ai::ProviderClient>
        } else if let Some(p) = &self.live_provider {
            p.clone()
        } else {
            self.provider.clone() as Arc<dyn rupi_ai::ProviderClient>
        };
        AgentLoopConfig {
            model: self.model.clone(),
            convert_to_llm: Box::new(|m| crate::default_convert_to_llm(m)),
            transform_context: None,
            tool_execution: crate::ToolExecutionMode::Parallel,
            stream_options: rupi_ai::StreamOptions {
                thinking_level: Some(self.thinking_level),
                ..Default::default()
            },
            provider,
            tools: self.tools.clone(),
            get_steering_messages: Some(Box::new(move || steering.drain())),
            get_follow_up_messages: Some(Box::new(move || follow_up.drain())),
            before_tool_call: self.gate.clone().map(|gate| {
                Box::new(move |ctx: crate::BeforeToolCallContext| {
                    let hint = ctx.args.get("command")
                        .and_then(|v| v.as_str())
                        .or_else(|| ctx.args.get("path").and_then(|v| v.as_str()))
                        .unwrap_or("");
                    match gate.decide(&ctx.tool_name, hint) {
                        PermissionDecision::Allow => BeforeToolCallResult::default(),
                        PermissionDecision::Deny => BeforeToolCallResult {
                            block: true,
                            reason: Some(format!("permission denied for tool `{}`", ctx.tool_name)),
                            terminate: false,
                        },
                        PermissionDecision::Ask => {
                            if std::env::var("RUPI_ALLOW_DESTRUCTIVE").ok().as_deref() == Some("1") {
                                BeforeToolCallResult::default()
                            } else {
                                BeforeToolCallResult {
                                    block: true,
                                    reason: Some(format!(
                                        "destructive `{}` requires approval (set RUPI_ALLOW_DESTRUCTIVE=1)",
                                        ctx.tool_name
                                    )),
                                    terminate: false,
                                }
                            }
                        }
                    }
                }) as Box<dyn Fn(crate::BeforeToolCallContext) -> BeforeToolCallResult + Send + Sync>
            }),
            after_tool_call: None,
            should_stop_after_turn: None,
            prepare_next_turn: None,
        }
    }

    pub async fn prompt(&mut self, text: impl Into<String>) -> AgentResult<Vec<Message>> {
        self.prompt_message(Message::user(text)).await
    }

    pub async fn prompt_message(&mut self, message: Message) -> AgentResult<Vec<Message>> {
        if self.streaming {
            return Err(AgentError::Busy);
        }
        self.streaming = true;
        self.events.lock().unwrap().clear();
        let emit = sync_sink(self.events.clone());
        let cfg = self.config();
        let result = agent_loop(vec![message], &mut self.context, &cfg, emit).await;
        self.streaming = false;
        result
    }

    pub async fn continue_run(&mut self) -> AgentResult<Vec<Message>> {
        if self.streaming {
            return Err(AgentError::Busy);
        }
        self.streaming = true;
        let emit = sync_sink(self.events.clone());
        let cfg = self.config();
        let result = agent_loop_continue(&mut self.context, &cfg, emit).await;
        self.streaming = false;
        result
    }

    pub fn event_sink(&self) -> EventSink {
        sync_sink(self.events.clone())
    }

    pub fn silent_sink() -> EventSink {
        noop_sink()
    }
}
