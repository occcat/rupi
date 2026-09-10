use rupi_ai::{estimate_tokens, content_text, Message};

/// Matches pi-agent-core `DEFAULT_COMPACTION_SETTINGS`.
#[derive(Debug, Clone)]
pub struct CompactionSettings {
    pub context_window: u32,
    pub reserve_tokens: u32,
    pub keep_recent_tokens: u32,
}

pub const DEFAULT_COMPACTION_SETTINGS: CompactionSettings = CompactionSettings {
    context_window: 128_000,
    reserve_tokens: 16_384,
    keep_recent_tokens: 20_000,
};

pub fn estimate_message_tokens(message: &Message) -> u32 {
    match message {
        Message::User { content, .. } | Message::Assistant { content, .. } => {
            estimate_tokens(&content_text(content))
                + message
                    .tool_calls()
                    .iter()
                    .map(|c| estimate_tokens(&format!("{c:?}")))
                    .sum::<u32>()
        }
        Message::ToolResult { content, .. } => estimate_tokens(&content_text(content)),
    }
}

pub fn estimate_context_tokens(messages: &[Message]) -> u32 {
    messages.iter().map(estimate_message_tokens).sum()
}

pub fn should_compact(messages: &[Message], settings: &CompactionSettings) -> bool {
    let tokens = estimate_context_tokens(messages);
    tokens > settings.context_window.saturating_sub(settings.reserve_tokens)
}

/// Walk backwards until `keep_recent_tokens` is accumulated; cut before that point.
pub fn find_cut_point(messages: &[Message], keep_recent_tokens: u32) -> usize {
    let mut accumulated = 0u32;
    for (idx, msg) in messages.iter().enumerate().rev() {
        accumulated = accumulated.saturating_add(estimate_message_tokens(msg));
        if accumulated >= keep_recent_tokens {
            return idx;
        }
    }
    0
}

/// Extractive compaction used when no LLM summarizer is configured.
/// Produces a user-role summary plus the recent tail, matching Pi's
/// "keep recent + replace prefix with summary" shape.
pub fn compact_messages(messages: &[Message], settings: &CompactionSettings) -> Vec<Message> {
    if messages.len() < 4 {
        return messages.to_vec();
    }
    let cut = find_cut_point(messages, settings.keep_recent_tokens).max(1);
    let prefix = &messages[..cut];
    let tail = &messages[cut..];
    let mut summary = serialize_conversation(prefix);
    let max_summary_chars = (settings.keep_recent_tokens as usize).saturating_mul(4).max(2000);
    if summary.chars().count() > max_summary_chars {
        summary = truncate(&summary, max_summary_chars);
    }
    let mut out = vec![Message::user(format!(
        "<compaction_summary>\nThe following is a compact summary of earlier conversation turns.\n\n{summary}\n</compaction_summary>"
    ))];
    out.extend(tail.iter().cloned());
    out
}

pub fn serialize_conversation(messages: &[Message]) -> String {
    let mut lines = Vec::new();
    for msg in messages {
        match msg {
            Message::User { content, .. } => {
                lines.push(format!("User: {}", content_text(content)));
            }
            Message::Assistant { content, .. } => {
                let text = content_text(content);
                if !text.is_empty() {
                    lines.push(format!("Assistant: {text}"));
                }
                for call in msg.tool_calls() {
                    if let rupi_ai::ContentBlock::ToolCall { name, arguments, .. } = call {
                        lines.push(format!("Assistant called {name}({arguments})"));
                    }
                }
            }
            Message::ToolResult {
                tool_name, content, is_error, ..
            } => {
                let tag = if *is_error { "error" } else { "ok" };
                lines.push(format!(
                    "Tool {tool_name} [{tag}]: {}",
                    truncate(&content_text(content), 500)
                ));
            }
        }
    }
    lines.join("\n")
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max).collect();
        format!("{t}…")
    }
}
