//! `--mode rpc`：stdin JSONL 命令 / stdout JSONL 响应与事件（对标 Pi RPC）。
//!
//! 记录分隔符只认 LF；输入行尾 `\r` 会剥掉。事件沿用 `AgentEvent` 的 serde 形状
//!（`type` 字段）；命令回包为 `{type:"response", command, success, id?}`。
//!
//! `bash` / `abort_bash`：可取消句柄。本机 `sh -c` 与远程 Executor 共用同一条路——
//! 取消只置位 [`CancelFlag`]，杀进程 / 打远端 abort 由已注册 `bash` 工具的
//! `execute_with_cancel` 落地。stdin 在命令进行中继续读，避免 `abort_bash` 排在
//! 它所要取消的 `bash` 后面。

use rupi_agent::{AgentSession, MessageInbox, QueueMode, QueuedImage, QueuedMessage};
use rupi_core::{AgentEvent, CancelFlag, Message, Role};
use rupi_llm::ThinkingLevel;
use rupi_tools::ToolOutput;
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
    #[serde(default)]
    pub images: Vec<RpcImage>,
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
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default, rename = "modelId")]
    pub model_id: Option<String>,
    #[serde(default, rename = "sessionPath")]
    pub session_path: Option<String>,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, rename = "entryId")]
    pub entry_id: Option<String>,
    /// Pi RPC `bash.command`。
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default, rename = "excludeFromContext")]
    pub exclude_from_context: Option<bool>,
    #[serde(default)]
    pub timeout: Option<u64>,
    #[serde(default, rename = "timeout_secs")]
    pub timeout_secs: Option<u64>,
    /// `abort_bash` 可点名取消的句柄（默认取消当前进行中的那一个）。
    #[serde(default)]
    pub handle: Option<String>,
}

/// Pi RPC `images[]`：`{type, data, mimeType}`。
#[derive(Debug, Deserialize)]
pub struct RpcImage {
    #[serde(default, rename = "type")]
    #[allow(dead_code)]
    pub typ: Option<String>,
    #[serde(default)]
    pub data: Option<String>,
    #[serde(default, rename = "mimeType", alias = "mediaType")]
    pub mime_type: Option<String>,
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

/// 一次 RPC `bash` 的可取消句柄（本机进程组或远程 Executor 工具都认这面旗）。
struct BashJob {
    handle: String,
    cancel: CancelFlag,
}

fn bash_timeout_secs(cmd: &RpcCommand) -> u64 {
    cmd.timeout_secs.or(cmd.timeout).unwrap_or(30).clamp(1, 300)
}

fn bash_handle_id(cmd: &RpcCommand) -> String {
    cmd.id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
}

fn has_bash_tool(session: &AgentSession) -> bool {
    session.tools.names().iter().any(|n| n == "bash")
}

fn record_bash_context(session: &mut AgentSession, command: &str, output: &str) {
    let text = format!("Ran `{command}`\n```\n{output}\n```");
    session.session.push(Message::text(Role::User, text));
}

fn split_exit_tool_output(content: &str) -> (Option<i32>, &str) {
    // BashTool 失败：`exit {ExitStatus}: {output}`，Unix 上 Display 为
    // `exit status: N` 或 `signal: N`。
    if let Some(rest) = content.strip_prefix("exit exit status: ") {
        if let Some((code, out)) = rest.split_once(": ") {
            return (code.parse().ok(), out);
        }
        return (rest.parse().ok(), "");
    }
    if let Some(rest) = content.strip_prefix("exit signal: ") {
        if let Some((_, out)) = rest.split_once(": ") {
            return (None, out);
        }
        return (None, "");
    }
    (None, content)
}

fn bash_output_text(content: &str) -> String {
    if let Some(rest) = content.strip_prefix("cancelled by user") {
        let rest = rest.strip_prefix('\n').unwrap_or(rest);
        return rest
            .strip_prefix("[partial output]\n")
            .unwrap_or(rest)
            .to_string();
    }
    if content.starts_with("exit ") {
        return split_exit_tool_output(content).1.to_string();
    }
    content.to_string()
}

fn bash_result_from_tool(out: &ToolOutput, handle: &str) -> Value {
    let cancelled = out.is_error && out.content.contains("cancelled by user");
    let timed_out = out.is_error && out.content.contains("command timed out");
    let truncated = out.content.contains("[truncated:");
    let output = bash_output_text(&out.content);
    let exit_code = if cancelled || timed_out {
        Value::Null
    } else if !out.is_error {
        json!(0)
    } else {
        split_exit_tool_output(&out.content)
            .0
            .map(|n| json!(n))
            .unwrap_or(Value::Null)
    };
    json!({
        "output": output,
        "exitCode": exit_code,
        "cancelled": cancelled,
        "truncated": truncated,
        "handle": handle,
    })
}

fn emit_bash_update(id: Option<&str>, handle: &str, delta: &str) {
    if delta.is_empty() {
        return;
    }
    let mut v = json!({
        "type": "bash_execution_update",
        "delta": delta,
        "handle": handle,
    });
    v["id"] = json!(id.unwrap_or(handle));
    emit(&v);
}

fn abort_bash_job(job: &BashJob, cmd: &RpcCommand) {
    if let Some(h) = cmd
        .handle
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if h == job.handle {
            job.cancel.cancel();
        }
        return;
    }
    job.cancel.cancel();
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
        if matches!(cmd.typ.as_str(), "prompt" | "steer" | "follow_up") && !session.is_streaming() {
            if let Some(message) = queued_from_cmd(&cmd) {
                emit(&response_ok(cmd.id.as_deref(), &cmd.typ, None));
                run_prompt(&mut session, message, &mut lines).await?;
                continue;
            }
        }
        if cmd.typ == "bash" {
            run_bash(&mut session, &cmd, &mut lines).await?;
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
        "abort_bash" => {
            emit(&response_ok(id, "abort_bash", None));
        }
        "bash" => {
            emit(&response_err(id, "bash", "missing command"));
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
        "set_model" => match model_spec(cmd) {
            Some(spec) => {
                let data = apply_set_model(session, &spec).await;
                emit(&response_ok(id, "set_model", Some(data)));
            }
            None => emit(&response_err(id, "set_model", "missing provider/modelId")),
        },
        "get_available_models" => {
            emit(&response_ok(
                id,
                "get_available_models",
                Some(AgentSession::available_models()),
            ));
        }
        "switch_session" => {
            let dest = cmd
                .session_path
                .as_deref()
                .or(cmd.session.as_deref())
                .unwrap_or("")
                .trim();
            if dest.is_empty() {
                emit(&response_err(
                    id,
                    "switch_session",
                    "missing sessionPath or session",
                ));
            } else {
                match session.switch_session(dest) {
                    Ok(data) => emit(&response_ok(id, "switch_session", Some(data))),
                    Err(e) => emit(&response_err(id, "switch_session", format!("{e:#}"))),
                }
            }
        }
        "fork" => match session.fork_session(cmd.entry_id.as_deref()) {
            Ok(data) => emit(&response_ok(id, "fork", Some(data))),
            Err(e) => emit(&response_err(id, "fork", format!("{e:#}"))),
        },
        "clone" => match session.clone_session() {
            Ok(data) => emit(&response_ok(id, "clone", Some(data))),
            Err(e) => emit(&response_err(id, "clone", format!("{e:#}"))),
        },
        "get_tree" => {
            emit(&response_ok(id, "get_tree", Some(session.get_tree())));
        }
        "set_session_name" => {
            let name = cmd.name.as_deref().unwrap_or("");
            match session.set_session_name(name) {
                Ok(()) => emit(&response_ok(
                    id,
                    "set_session_name",
                    Some(json!({"name": session.session_name})),
                )),
                Err(e) => emit(&response_err(id, "set_session_name", format!("{e:#}"))),
            }
        }
        "get_commands" => {
            emit(&response_ok(
                id,
                "get_commands",
                Some(session.get_commands()),
            ));
        }
        other => emit(&response_err(
            id,
            other,
            format!("unknown command: {other}"),
        )),
    }
    Ok(())
}

fn queued_from_cmd(cmd: &RpcCommand) -> Option<QueuedMessage> {
    let text = cmd.message.clone().unwrap_or_default();
    let images = parse_rpc_images(&cmd.images);
    if text.trim().is_empty() && images.is_empty() {
        None
    } else {
        Some(QueuedMessage { text, images })
    }
}

fn parse_rpc_images(images: &[RpcImage]) -> Vec<QueuedImage> {
    images
        .iter()
        .filter_map(|img| {
            let data = img.data.as_deref()?.trim();
            if data.is_empty() {
                return None;
            }
            Some(QueuedImage {
                media_type: img
                    .mime_type
                    .clone()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "image/png".into()),
                data: data.to_string(),
            })
        })
        .collect()
}

fn model_spec(cmd: &RpcCommand) -> Option<String> {
    let provider = cmd
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let model = cmd
        .model_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    match (provider, model) {
        (Some(p), Some(m)) => Some(format!("{p}/{m}")),
        (None, Some(m)) => Some(m.to_string()),
        (Some(p), None) => Some(p.to_string()),
        (None, None) => None,
    }
}

async fn apply_set_model(session: &mut AgentSession, spec: &str) -> Value {
    let data = session.set_model(spec);
    let ev = AgentEvent::ModelChange {
        provider: data
            .get("provider")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        model: data
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    };
    for ext in &session.extensions {
        let _ = ext.on_event(&ev).await;
    }
    emit_event(&ev);
    data
}

async fn run_prompt<R: tokio::io::AsyncBufRead + Unpin>(
    session: &mut AgentSession,
    message: QueuedMessage,
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
    let fut = session.prompt_queued(message, &on_event);
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

/// 跑一条 RPC `bash`：stdin 继续读，使 `abort_bash` 能取消当前句柄。
async fn run_bash<R: tokio::io::AsyncBufRead + Unpin>(
    session: &mut AgentSession,
    cmd: &RpcCommand,
    lines: &mut tokio::io::Lines<R>,
) -> anyhow::Result<()> {
    let id = cmd.id.as_deref();
    let command = cmd.command.as_deref().unwrap_or("").trim();
    if command.is_empty() {
        emit(&response_err(id, "bash", "missing command"));
        return Ok(());
    }
    if !has_bash_tool(session) {
        emit(&response_err(id, "bash", "bash tool is not registered"));
        return Ok(());
    }

    let handle = bash_handle_id(cmd);
    let job = BashJob {
        handle: handle.clone(),
        cancel: CancelFlag::new(),
    };
    let args = json!({
        "command": command,
        "timeout_secs": bash_timeout_secs(cmd),
    });
    let tools = session.tools.clone();
    let cancel = job.cancel.clone();
    let fut = tools.execute_with_cancel("bash", args, &cancel);
    tokio::pin!(fut);

    let mut stdin_open = true;
    let out = loop {
        tokio::select! {
            biased;
            res = &mut fut => {
                break match res {
                    Ok(o) => o,
                    Err(e) => {
                        emit(&response_err(id, "bash", format!("{e:#}")));
                        return Ok(());
                    }
                };
            }
            line = lines.next_line(), if stdin_open => {
                match line {
                    Ok(Some(l)) => match parse_line(&l) {
                        Ok(None) => {}
                        Ok(Some(c)) => handle_during_bash(&job, &c),
                        Err(e) => emit(&response_err(None, "unknown", e.to_string())),
                    },
                    Ok(None) | Err(_) => stdin_open = false,
                }
            }
        }
    };

    let data = bash_result_from_tool(&out, &job.handle);
    if let Some(delta) = data.get("output").and_then(|v| v.as_str()) {
        emit_bash_update(id, &job.handle, delta);
    }
    if !cmd.exclude_from_context.unwrap_or(false) {
        record_bash_context(session, command, data["output"].as_str().unwrap_or(""));
    }
    emit(&response_ok(id, "bash", Some(data)));
    Ok(())
}

fn handle_during_bash(job: &BashJob, cmd: &RpcCommand) {
    let id = cmd.id.as_deref();
    match cmd.typ.as_str() {
        "abort_bash" => {
            abort_bash_job(job, cmd);
            emit(&response_ok(id, "abort_bash", None));
        }
        "abort" => {
            job.cancel.cancel();
            emit(&response_ok(id, "abort", None));
        }
        "get_state" => {
            emit(&response_ok(
                id,
                "get_state",
                Some(json!({
                    "isStreaming": false,
                    "bashHandle": job.handle,
                    "pendingMessageCount": 0,
                })),
            ));
        }
        other => emit(&response_err(
            id,
            other,
            format!("{other} not available while bash is running"),
        )),
    }
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
        if let Some(q) = queued_from_cmd(cmd) {
            inbox.steer_msg(q);
        }
        emit(&response_ok(id, cmd.typ.as_str(), None));
        return;
    }
    if follow {
        if let Some(q) = queued_from_cmd(cmd) {
            inbox.follow_up_msg(q);
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
        "abort_bash" => {
            emit(&response_ok(id, "abort_bash", None));
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

    #[test]
    fn prompt_images_and_set_model_fields() {
        let c = parse_line(
            r#"{"type":"prompt","message":"look","images":[{"type":"image","data":"AAAA","mimeType":"image/png"}]}"#,
        )
        .unwrap()
        .unwrap();
        let q = queued_from_cmd(&c).unwrap();
        assert_eq!(q.text, "look");
        assert_eq!(q.images.len(), 1);
        assert_eq!(q.images[0].media_type, "image/png");
        assert_eq!(q.images[0].data, "AAAA");

        let m = parse_line(r#"{"type":"set_model","provider":"openai","modelId":"gpt-4o-mini"}"#)
            .unwrap()
            .unwrap();
        assert_eq!(model_spec(&m).as_deref(), Some("openai/gpt-4o-mini"));
    }

    #[test]
    fn switch_session_accepts_path_or_id() {
        let c = parse_line(r#"{"type":"switch_session","sessionPath":"/tmp/a.jsonl"}"#)
            .unwrap()
            .unwrap();
        assert_eq!(c.session_path.as_deref(), Some("/tmp/a.jsonl"));
        let c = parse_line(r#"{"type":"fork","entryId":"abc"}"#)
            .unwrap()
            .unwrap();
        assert_eq!(c.entry_id.as_deref(), Some("abc"));
    }

    #[test]
    fn bash_and_abort_fields() {
        let c = parse_line(
            r#"{"id":"req-1","type":"bash","command":"ls -la","excludeFromContext":true,"timeout_secs":12}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(c.typ, "bash");
        assert_eq!(c.command.as_deref(), Some("ls -la"));
        assert_eq!(c.exclude_from_context, Some(true));
        assert_eq!(bash_timeout_secs(&c), 12);
        assert_eq!(bash_handle_id(&c), "req-1");

        let a = parse_line(r#"{"id":"a1","type":"abort_bash","handle":"req-1"}"#)
            .unwrap()
            .unwrap();
        assert_eq!(a.typ, "abort_bash");
        assert_eq!(a.handle.as_deref(), Some("req-1"));
    }

    #[test]
    fn bash_result_maps_ok_cancel_exit() {
        let ok = bash_result_from_tool(&ToolOutput::ok("hello"), "h1");
        assert_eq!(ok["exitCode"], 0);
        assert_eq!(ok["cancelled"], false);
        assert_eq!(ok["truncated"], false);
        assert_eq!(ok["handle"], "h1");
        assert_eq!(ok["output"], "hello");

        let cancel = bash_result_from_tool(
            &ToolOutput::err("cancelled by user\n[partial output]\npartial"),
            "h1",
        );
        assert_eq!(cancel["cancelled"], true);
        assert_eq!(cancel["exitCode"], Value::Null);
        assert_eq!(cancel["output"], "partial");

        let fail = bash_result_from_tool(&ToolOutput::err("exit exit status: 7: boom"), "h1");
        assert_eq!(fail["exitCode"], 7);
        assert_eq!(fail["cancelled"], false);
        assert_eq!(fail["output"], "boom");

        let trunc = bash_result_from_tool(
            &ToolOutput::ok("tail\n\n[truncated: kept last 1 / 9 lines, 4B / 9B]"),
            "h1",
        );
        assert_eq!(trunc["truncated"], true);
    }

    #[tokio::test]
    async fn bash_result_maps_real_tool_output() {
        let tools = rupi_tools::ToolRegistry::with_builtins();
        let ok = tools
            .execute(
                "bash",
                json!({"command": "printf 'hello-map\\n'", "timeout_secs": 10}),
            )
            .await
            .unwrap();
        let data = bash_result_from_tool(&ok, "h");
        assert_eq!(data["exitCode"], 0);
        assert_eq!(data["cancelled"], false);
        assert!(
            data["output"].as_str().unwrap().contains("hello-map"),
            "{}",
            data
        );

        let fail = tools
            .execute("bash", json!({"command": "exit 7", "timeout_secs": 10}))
            .await
            .unwrap();
        let data = bash_result_from_tool(&fail, "h");
        assert_eq!(data["exitCode"], 7, "{data}");
        assert!(!data["output"].as_str().unwrap().contains("exit exit"));
    }

    #[tokio::test]
    async fn run_bash_abort_handle_is_not_serialized() {
        let mut session =
            rupi_agent::create_agent_session(Arc::new(rupi_llm::MockProvider::new(vec![])));
        let stdin = "{\"id\":\"a\",\"type\":\"abort_bash\"}\n";
        let mut lines = BufReader::new(stdin.as_bytes()).lines();
        let cmd = parse_line(r#"{"id":"b","type":"bash","command":"sleep 30","timeout_secs":60}"#)
            .unwrap()
            .unwrap();
        let start = std::time::Instant::now();
        run_bash(&mut session, &cmd, &mut lines).await.unwrap();
        assert!(
            start.elapsed() < std::time::Duration::from_secs(8),
            "abort_bash must cancel the in-flight handle, not wait for sleep"
        );
        assert!(
            session
                .messages()
                .iter()
                .any(|m| m.full_text().contains("sleep 30")),
            "cancelled bash still recorded for the next prompt"
        );
    }

    #[tokio::test]
    async fn run_bash_exclude_from_context_and_missing_command() {
        let mut session =
            rupi_agent::create_agent_session(Arc::new(rupi_llm::MockProvider::new(vec![])));
        let mut empty = BufReader::new(&b""[..]).lines();
        let skip = parse_line(
            r#"{"id":"x","type":"bash","command":"printf 'secret-not-in-ctx\\n'","excludeFromContext":true}"#,
        )
        .unwrap()
        .unwrap();
        run_bash(&mut session, &skip, &mut empty).await.unwrap();
        assert!(
            session
                .messages()
                .iter()
                .all(|m| !m.full_text().contains("secret-not-in-ctx")),
            "excludeFromContext must skip the session tree"
        );

        let mut empty = BufReader::new(&b""[..]).lines();
        let missing = parse_line(r#"{"id":"z","type":"bash"}"#).unwrap().unwrap();
        run_bash(&mut session, &missing, &mut empty).await.unwrap();
    }

    #[test]
    fn abort_bash_job_matches_handle() {
        let job = BashJob {
            handle: "req-1".into(),
            cancel: CancelFlag::new(),
        };
        let other = parse_line(r#"{"type":"abort_bash","handle":"nope"}"#)
            .unwrap()
            .unwrap();
        abort_bash_job(&job, &other);
        assert!(!job.cancel.is_cancelled());
        let hit = parse_line(r#"{"type":"abort_bash","handle":"req-1"}"#)
            .unwrap()
            .unwrap();
        abort_bash_job(&job, &hit);
        assert!(job.cancel.is_cancelled());
    }
}
