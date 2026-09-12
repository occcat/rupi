//! AG-UI：官方 `RunAgentInput` / `BaseEvent`。禁止 CUSTOM/RAW 当 RPC 隧道。

use rupi_core::{AgentEvent, ContentBlock, Message, Role, SessionTree};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunAgentInput {
    pub thread_id: String,
    pub run_id: String,
    #[serde(default)]
    pub parent_run_id: Option<String>,
    #[serde(default)]
    pub state: Option<Value>,
    #[serde(default)]
    pub messages: Vec<AguiMessage>,
    #[serde(default)]
    pub tools: Vec<Value>,
    #[serde(default)]
    pub context: Vec<AguiContext>,
    #[serde(default)]
    pub forwarded_props: Option<Value>,
    #[serde(default)]
    pub resume: Vec<ResumeEntry>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeEntry {
    pub interrupt_id: String,
    pub status: String,
    #[serde(default)]
    pub payload: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AguiContext {
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub value: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AguiMessage {
    #[serde(default)]
    pub id: Option<String>,
    pub role: String,
    #[serde(default)]
    pub content: Option<Value>,
    #[serde(default)]
    pub tool_calls: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AguiEvent {
    #[serde(rename = "type")]
    pub typ: String,
    #[serde(flatten)]
    pub fields: Value,
}

impl AguiEvent {
    pub fn new(typ: &str, fields: Value) -> Self {
        Self {
            typ: typ.into(),
            fields,
        }
    }

    pub fn to_sse_data(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".into())
    }
}

pub fn run_started(thread: &str, run: &str, parent: Option<&str>) -> AguiEvent {
    let mut f = json!({"threadId": thread, "runId": run});
    if let Some(p) = parent {
        f["parentRunId"] = json!(p);
    }
    AguiEvent::new("RUN_STARTED", f)
}

pub fn run_finished_success(thread: &str, run: &str) -> AguiEvent {
    AguiEvent::new(
        "RUN_FINISHED",
        json!({
            "threadId": thread,
            "runId": run,
            "outcome": { "type": "success" }
        }),
    )
}

pub fn run_finished_interrupt(
    thread: &str,
    run: &str,
    interrupts: Vec<Value>,
) -> AguiEvent {
    AguiEvent::new(
        "RUN_FINISHED",
        json!({
            "threadId": thread,
            "runId": run,
            "outcome": { "type": "interrupt", "interrupts": interrupts }
        }),
    )
}

pub fn run_error(message: &str, code: Option<&str>) -> AguiEvent {
    let mut f = json!({"message": message});
    if let Some(c) = code {
        f["code"] = json!(c);
    }
    AguiEvent::new("RUN_ERROR", f)
}

pub fn state_snapshot(state: Value) -> AguiEvent {
    AguiEvent::new("STATE_SNAPSHOT", json!({"snapshot": state}))
}

pub fn messages_snapshot(messages: Vec<Value>) -> AguiEvent {
    AguiEvent::new("MESSAGES_SNAPSHOT", json!({"messages": messages}))
}

/// 把本机 [`AgentEvent`] 映射为官方事件。不发射 CUSTOM/RAW。
pub struct EventMapper {
    thread_id: String,
    run_id: String,
    text_id: Option<String>,
    reasoning_id: Option<String>,
}

impl EventMapper {
    pub fn new(thread_id: String, run_id: String) -> Self {
        Self {
            thread_id,
            run_id,
            text_id: None,
            reasoning_id: None,
        }
    }

    fn close_text(&mut self, out: &mut Vec<AguiEvent>) {
        if let Some(id) = self.text_id.take() {
            out.push(AguiEvent::new(
                "TEXT_MESSAGE_END",
                json!({"messageId": id}),
            ));
        }
    }

    fn close_reason(&mut self, out: &mut Vec<AguiEvent>) {
        if let Some(id) = self.reasoning_id.take() {
            out.push(AguiEvent::new("REASONING_END", json!({"messageId": id})));
        }
    }

    pub fn map(&mut self, ev: &AgentEvent) -> Vec<AguiEvent> {
        let mut out = Vec::new();
        match ev {
            AgentEvent::TextDelta { delta } if !delta.is_empty() => {
                self.close_reason(&mut out);
                if self.text_id.is_none() {
                    let id = uuid::Uuid::new_v4().to_string();
                    out.push(AguiEvent::new(
                        "TEXT_MESSAGE_START",
                        json!({"messageId": id, "role": "assistant"}),
                    ));
                    self.text_id = Some(id);
                }
                out.push(AguiEvent::new(
                    "TEXT_MESSAGE_CONTENT",
                    json!({"messageId": self.text_id.clone(), "delta": delta}),
                ));
            }
            AgentEvent::ToolStart {
                tool_call_id,
                name,
                arguments,
            } => {
                self.close_text(&mut out);
                self.close_reason(&mut out);
                out.push(AguiEvent::new(
                    "TOOL_CALL_START",
                    json!({
                        "toolCallId": tool_call_id,
                        "toolCallName": name
                    }),
                ));
                out.push(AguiEvent::new(
                    "TOOL_CALL_ARGS",
                    json!({
                        "toolCallId": tool_call_id,
                        "delta": arguments.to_string()
                    }),
                ));
                out.push(AguiEvent::new(
                    "TOOL_CALL_END",
                    json!({"toolCallId": tool_call_id}),
                ));
            }
            AgentEvent::ToolEnd {
                tool_call_id,
                content,
                is_error,
                ..
            } => {
                self.close_text(&mut out);
                let mut f = json!({
                    "messageId": uuid::Uuid::new_v4().to_string(),
                    "toolCallId": tool_call_id,
                    "content": content,
                    "role": "tool"
                });
                if *is_error {
                    f["error"] = json!(content);
                }
                out.push(AguiEvent::new("TOOL_CALL_RESULT", f));
            }
            AgentEvent::Thinking { text } if !text.is_empty() => {
                self.close_text(&mut out);
                let id = uuid::Uuid::new_v4().to_string();
                out.push(AguiEvent::new(
                    "REASONING_START",
                    json!({"messageId": id}),
                ));
                out.push(AguiEvent::new(
                    "REASONING_MESSAGE_CONTENT",
                    json!({"messageId": id, "delta": text}),
                ));
                out.push(AguiEvent::new("REASONING_END", json!({"messageId": id})));
            }
            AgentEvent::TurnStart { turn } => {
                out.push(AguiEvent::new(
                    "STEP_STARTED",
                    json!({"stepName": format!("turn-{turn}")}),
                ));
            }
            AgentEvent::TurnEnd { turn, .. } => {
                self.close_text(&mut out);
                self.close_reason(&mut out);
                out.push(AguiEvent::new(
                    "STEP_FINISHED",
                    json!({"stepName": format!("turn-{turn}")}),
                ));
            }
            AgentEvent::CompactionStart => {
                out.push(AguiEvent::new(
                    "STEP_STARTED",
                    json!({"stepName": "compaction"}),
                ));
            }
            AgentEvent::CompactionEnd { summarized, kept } => {
                out.push(AguiEvent::new(
                    "STEP_FINISHED",
                    json!({"stepName": "compaction", "summarized": summarized, "kept": kept}),
                ));
            }
            AgentEvent::Usage {
                input_tokens,
                output_tokens,
            } => {
                out.push(AguiEvent::new(
                    "STATE_DELTA",
                    json!({"delta": [{
                        "op": "add",
                        "path": "/lastUsage",
                        "value": {"inputTokens": input_tokens, "outputTokens": output_tokens}
                    }]}),
                ));
            }
            AgentEvent::Error { message } => {
                self.close_text(&mut out);
                out.push(run_error(message, None));
            }
            // UiPrompt / MemoryRecall / UiHint / Steering / ModelChange / RunEnd：
            // 不出网或由宿主另发官方生命周期。禁止 RAW/CUSTOM。
            _ => {}
        }
        let _ = (&self.thread_id, &self.run_id);
        out
    }

    pub fn finish_open(&mut self) -> Vec<AguiEvent> {
        let mut out = Vec::new();
        self.close_text(&mut out);
        self.close_reason(&mut out);
        out
    }
}

pub fn last_user_from_input(input: &RunAgentInput) -> Option<Message> {
    let last = input.messages.iter().rev().find(|m| m.role == "user")?;
    Some(agui_user_to_message(last))
}

pub fn agui_user_to_message(m: &AguiMessage) -> Message {
    let id = m
        .id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let mut blocks = Vec::new();
    match &m.content {
        Some(Value::String(s)) => {
            if !s.is_empty() {
                blocks.push(ContentBlock::Text { text: s.clone() });
            }
        }
        Some(Value::Array(arr)) => {
            for part in arr {
                if part.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                        blocks.push(ContentBlock::Text { text: t.into() });
                    }
                } else if part.get("type").and_then(|t| t.as_str()) == Some("image") {
                    if let Some(src) = part.get("source") {
                        if src.get("type").and_then(|t| t.as_str()) == Some("data") {
                            let data = src.get("value").and_then(|v| v.as_str()).unwrap_or("");
                            let media = src
                                .get("mimeType")
                                .and_then(|v| v.as_str())
                                .unwrap_or("image/png");
                            blocks.push(ContentBlock::Image {
                                media_type: media.into(),
                                data: data.into(),
                            });
                        }
                    }
                }
            }
        }
        _ => {}
    }
    if blocks.is_empty() {
        blocks.push(ContentBlock::Text {
            text: String::new(),
        });
    }
    Message {
        id,
        role: Role::User,
        blocks,
        provider: None,
        created_at: chrono::Utc::now(),
    }
}

pub fn tree_to_agui_messages(tree: &SessionTree) -> Vec<Value> {
    tree.history()
        .into_iter()
        .map(|m| message_to_agui(m))
        .collect()
}

fn message_to_agui(m: &Message) -> Value {
    let role = match m.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };
    let text = m
        .blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let tool_calls: Vec<Value> = m
        .blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
            } => Some(json!({
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": arguments.to_string() }
            })),
            _ => None,
        })
        .collect();
    let mut v = json!({"id": m.id, "role": role, "content": text});
    if !tool_calls.is_empty() {
        v["toolCalls"] = json!(tool_calls);
    }
    if let Some(ContentBlock::ToolResult {
        tool_call_id,
        content,
        ..
    }) = m.blocks.iter().find(|b| matches!(b, ContentBlock::ToolResult { .. }))
    {
        v["toolCallId"] = json!(tool_call_id);
        v["content"] = json!(content);
    }
    v
}

pub fn cloud_state(
    session_id: &str,
    name: Option<&str>,
    model: Option<&str>,
    thinking: Option<&str>,
    auto_compaction: bool,
    pending: &[String],
    backend: Option<&str>,
    region: Option<&str>,
) -> Value {
    json!({
        "sessionId": session_id,
        "sessionName": name,
        "model": model,
        "thinkingLevel": thinking,
        "autoCompaction": auto_compaction,
        "pendingInterruptIds": pending,
        "region": region,
        "runtime": {
            "backend": backend.unwrap_or("remote-http"),
            "status": "ready",
            "region": region
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_text_delta_triad() {
        let mut m = EventMapper::new("t".into(), "r".into());
        let evs = m.map(&AgentEvent::TextDelta {
            delta: "hi".into(),
        });
        assert_eq!(evs[0].typ, "TEXT_MESSAGE_START");
        assert_eq!(evs[1].typ, "TEXT_MESSAGE_CONTENT");
        assert!(!evs.iter().any(|e| e.typ == "CUSTOM" || e.typ == "RAW"));
    }

    #[test]
    fn maps_tool_start_to_official_triad() {
        let mut m = EventMapper::new("t".into(), "r".into());
        let evs = m.map(&AgentEvent::ToolStart {
            tool_call_id: "c1".into(),
            name: "write".into(),
            arguments: json!({"path": "a"}),
        });
        assert_eq!(
            evs.iter().map(|e| e.typ.as_str()).collect::<Vec<_>>(),
            ["TOOL_CALL_START", "TOOL_CALL_ARGS", "TOOL_CALL_END"]
        );
    }
}
