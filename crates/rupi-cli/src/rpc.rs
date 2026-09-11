//! `--mode rpc`：stdin JSONL 命令 / stdout JSONL 响应与事件（对标 Pi RPC）。
//!
//! 记录分隔符只认 LF；输入行尾 `\r` 会剥掉。事件沿用 `AgentEvent` 的 serde 形状
//!（`type` 字段）；命令回包为 `{type:"response", command, success, id?}`。

use rupi_agent::{AgentSession, MessageInbox, QueueMode};
use rupi_core::{AgentEvent, CancelFlag};
use rupi_llm::ThinkingLevel;
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::Write;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};

#[derive(Debug, Deserialize)]
pub struct RpcCommand {
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub typ: String,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default, rename = "streamingBehavior")]
    pub streaming_behavior: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub level: Option<String>,
    #[serde(default, rename = "customInstructions")]
    pub custom_instructions: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

#[derive(Debug)]
pub enum ParseErr {
    InvalidJson(String),
}

impl std::fmt::Display for ParseErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseErr::InvalidJson(s) => write!(f, "{s}"),
        }
    }
}

/// 剥行尾 CR，再解析。空行返回 None。
pub fn parse_line(line: &str) -> Result<Option<RpcCommand>, ParseErr> {
    let line = line.strip_suffix('\r').unwrap_or(line).trim();
    if line.is_empty() {
        return Ok(None);
    }
    serde_json::from_str(line)
        .map(Some)
        .map_err(|e| ParseErr::InvalidJson(e.to_string()))
}

pub fn response_ok(id: Option<&str>, command: &str, data: Option<Value>) -> Value {
    let mut v = json!({"type": "response", "command": command, "success": true});
    if let Some(id) = id {
        v["id"] = json!(id);
    }
    if let Some(data) = data {
        v["data"] = data;
    }
    v
}

pub fn response_err(id: Option<&str>, command: &str, error: impl AsRef<str>) -> Value {
    let mut v = json!({
        "type": "response",
        "command": command,
        "success": false,
        "error": error.as_ref(),
    });
    if let Some(id) = id {
        v["id"] = json!(id);
    }
    v
}

fn emit(value: &Value) {
    if let Ok(s) = serde_json::to_string(value) {
        println!("{s}");
        let _ = std::io::stdout().flush();
    }
}

fn emit_event(e: &AgentEvent) {
    if let Ok(s) = serde_json::to_string(e) {
        println!("{s}");
        let _ = std::io::stdout().flush();
    }
}

/// 驱动 RPC 循环直到 stdin EOF。诊断走 stderr。
pub async fn serve(mut session: AgentSession) -> anyhow::Result<AgentSession> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        let line = match lines.next_line().await? {
            None => break,
            Some(l) => l,
        };
        let cmd = match parse_line(&line) {
            Ok(None) => continue,
            Ok(Some(c)) => c,
            Err(e) => {
                emit(&response_err(None, "unknown", e.to_string()));
                continue;
            }
        };
        if matches!(cmd.typ.as_str(), "prompt" | "steer" | "follow_up")
            && !session.is_streaming()
            && cmd
                .message
                .as_deref()
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false)
        {
            let message = cmd.message.clone().unwrap_or_default();
            emit(&response_ok(cmd.id.as_deref(), &cmd.typ, None));
            run_prompt(&mut session, &message, &mut lines).await?;
            continue;
        }
        dispatch(&mut session, &cmd).await?;
    }
    Ok(session)
}

async fn dispatch(session: &mut AgentSession, cmd: &RpcCommand) -> anyhow::Result<()> {
    let id = cmd.id.as_deref();
    match cmd.typ.as_str() {
        "prompt" => {
            emit(&response_err(
                id,
                "prompt",
                "empty message or missing streamingBehavior while busy",
            ));
        }
        "steer" | "follow_up" => {
            emit(&response_err(id, &cmd.typ, "empty message"));
        }
        "abort" => {
            session.abort();
            emit(&response_ok(id, "abort", None));
        }
        "clear_queue" => {
            let (steering, follow_up) = session.clear_queue();
            emit(&response_ok(
                id,
                "clear_queue",
                Some(json!({"steering": steering, "followUp": follow_up})),
            ));
        }
        "new_session" => {
            session.new_session();
            emit(&response_ok(
                id,
                "new_session",
                Some(json!({"cancelled": false, "sessionId": session.session_id})),
            ));
        }
        "get_state" => {
            emit(&response_ok(
                id,
                "get_state",
                Some(serde_json::to_value(session.state())?),
            ));
        }
        "get_messages" => {
            emit(&response_ok(
                id,
                "get_messages",
                Some(json!({"messages": session.messages()})),
            ));
        }
        "set_steering_mode" => match cmd.mode.as_deref().and_then(QueueMode::parse) {
            Some(m) => {
                session.set_steering_mode(m);
                emit(&response_ok(id, "set_steering_mode", None));
            }
            None => emit(&response_err(
                id,
                "set_steering_mode",
                "mode must be all or one-at-a-time",
            )),
        },
        "set_follow_up_mode" => match cmd.mode.as_deref().and_then(QueueMode::parse) {
            Some(m) => {
                session.set_follow_up_mode(m);
                emit(&response_ok(id, "set_follow_up_mode", None));
            }
            None => emit(&response_err(
                id,
                "set_follow_up_mode",
                "mode must be all or one-at-a-time",
            )),
        },
        "set_thinking_level" => match cmd.level.as_deref() {
            Some(s) => match s.parse::<ThinkingLevel>() {
                Ok(l) => {
                    session.agent.thinking = Some(l);
                    emit(&response_ok(id, "set_thinking_level", None));
                }
                Err(e) => emit(&response_err(id, "set_thinking_level", e.to_string())),
            },
            None => emit(&response_err(id, "set_thinking_level", "missing level")),
        },
        "compact" => {
            let prompt = cmd.custom_instructions.as_deref();
            session
                .agent
                .force_compress_with_prompt(
                    &*session.provider,
                    &mut session.session,
                    &session.mem,
                    &|_| {},
                    prompt,
                )
                .await;
            emit(&response_ok(
                id,
                "compact",
                Some(json!({"summary": session.session.summary})),
            ));
        }
        "get_session_stats" => {
            let usage = session.last_usage().unwrap_or((0, 0));
            emit(&response_ok(
                id,
                "get_session_stats",
                Some(json!({
                    "sessionId": session.session_id,
                    "totalMessages": session.session.history().len(),
                    "tokens": {"input": usage.0, "output": usage.1},
                    "pendingMessageCount": session.inbox.pending_count(),
                })),
            ));
        }
        "set_auto_compaction" => {
            session.agent.compaction_enabled = cmd.enabled.unwrap_or(true);
            emit(&response_ok(id, "set_auto_compaction", None));
        }
        other => emit(&response_err(
            id,
            other,
            format!("unknown command: {other}"),
        )),
    }
    Ok(())
}

async fn run_prompt<R: tokio::io::AsyncBufRead + Unpin>(
    session: &mut AgentSession,
    message: &str,
    lines: &mut tokio::io::Lines<R>,
) -> anyhow::Result<()> {
    let inbox = session.inbox.clone();
    let cancel = session.cancel.clone();
    let snap = StateSnap {
        session_id: session.session_id.clone(),
        session_name: session.session_name.clone(),
        thinking: session.agent.thinking,
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    let on_event = move |e: AgentEvent| {
        let _ = tx.send(e);
    };
    let fut = session.prompt(message, &on_event);
    tokio::pin!(fut);
    let mut done = false;
    while !done {
        tokio::select! {
            biased;
            res = &mut fut => {
                if let Err(e) = res {
                    emit(&json!({"type":"error","message": format!("{e:#}")}));
                }
                done = true;
            }
            Some(e) = rx.recv() => emit_event(&e),
            line = lines.next_line() => {
                match line {
                    Ok(Some(l)) => match parse_line(&l) {
                        Ok(None) => {}
                        Ok(Some(cmd)) => handle_during_prompt(&inbox, &cancel, &snap, &cmd),
                        Err(e) => emit(&response_err(None, "unknown", e.to_string())),
                    },
                    Ok(None) | Err(_) => {}
                }
            }
        }
    }
    while let Ok(e) = rx.try_recv() {
        emit_event(&e);
    }
    Ok(())
}

struct StateSnap {
    session_id: String,
    session_name: Option<String>,
    thinking: Option<ThinkingLevel>,
}

fn handle_during_prompt(
    inbox: &Arc<MessageInbox>,
    cancel: &CancelFlag,
    snap: &StateSnap,
    cmd: &RpcCommand,
) {
    let id = cmd.id.as_deref();
    let steer = cmd.typ == "steer"
        || (cmd.typ == "prompt" && cmd.streaming_behavior.as_deref() == Some("steer"));
    let follow = cmd.typ == "follow_up"
        || (cmd.typ == "prompt"
            && matches!(
                cmd.streaming_behavior.as_deref(),
                Some("followUp") | Some("follow_up")
            ));
    if steer {
        if let Some(m) = &cmd.message {
            inbox.steer(m);
        }
        emit(&response_ok(id, cmd.typ.as_str(), None));
        return;
    }
    if follow {
        if let Some(m) = &cmd.message {
            inbox.follow_up(m);
        }
        emit(&response_ok(id, cmd.typ.as_str(), None));
        return;
    }
    match cmd.typ.as_str() {
        "prompt" => emit(&response_err(
            id,
            "prompt",
            "agent is streaming; set streamingBehavior to steer or followUp",
        )),
        "abort" => {
            cancel.cancel();
            emit(&response_ok(id, "abort", None));
        }
        "clear_queue" => {
            let (steering, follow_up) = inbox.clear();
            emit(&response_ok(
                id,
                "clear_queue",
                Some(json!({"steering": steering, "followUp": follow_up})),
            ));
        }
        "get_state" => {
            emit(&response_ok(
                id,
                "get_state",
                Some(json!({
                    "sessionId": snap.session_id,
                    "sessionName": snap.session_name,
                    "thinkingLevel": snap.thinking,
                    "isStreaming": true,
                    "steeringMode": inbox.steering_mode(),
                    "followUpMode": inbox.follow_up_mode(),
                    "pendingMessageCount": inbox.pending_count(),
                })),
            ));
        }
        "set_steering_mode" => {
            if let Some(m) = cmd.mode.as_deref().and_then(QueueMode::parse) {
                inbox.set_steering_mode(m);
                emit(&response_ok(id, "set_steering_mode", None));
            } else {
                emit(&response_err(id, "set_steering_mode", "invalid mode"));
            }
        }
        "set_follow_up_mode" => {
            if let Some(m) = cmd.mode.as_deref().and_then(QueueMode::parse) {
                inbox.set_follow_up_mode(m);
                emit(&response_ok(id, "set_follow_up_mode", None));
            } else {
                emit(&response_err(id, "set_follow_up_mode", "invalid mode"));
            }
        }
        other => emit(&response_err(
            id,
            other,
            format!("{other} not available while streaming"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_strips_cr_and_skips_blank() {
        let c = parse_line("{\"type\":\"get_state\"}\r\n").unwrap().unwrap();
        assert_eq!(c.typ, "get_state");
        assert!(parse_line("\n").unwrap().is_none());
        assert!(parse_line("not-json").is_err());
    }

    #[test]
    fn response_shapes() {
        let ok = response_ok(Some("1"), "prompt", None);
        assert_eq!(ok["type"], "response");
        assert_eq!(ok["success"], true);
        assert_eq!(ok["id"], "1");
        let err = response_err(None, "prompt", "busy");
        assert_eq!(err["success"], false);
        assert_eq!(err["error"], "busy");
    }

    #[test]
    fn prompt_streaming_fields() {
        let c =
            parse_line(r#"{"id":"r","type":"prompt","message":"hi","streamingBehavior":"steer"}"#)
                .unwrap()
                .unwrap();
        assert_eq!(c.streaming_behavior.as_deref(), Some("steer"));
        assert_eq!(c.message.as_deref(), Some("hi"));
    }
}
