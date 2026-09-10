use rupi_ai::{
    content_text, estimate_messages_tokens, estimate_tokens, AssistantMessage, ContentBlock,
    Message, Model, Provider, StopReason, StreamOptions, StreamRequest,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionSettings {
    pub enabled: bool,
    /// Reserve this many tokens for the next model turn (Pi default ~16k).
    pub reserve_tokens: u32,
    /// Keep this many most-recent messages after the summary.
    pub retain_tail: usize,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            reserve_tokens: 16_384,
            retain_tail: 6,
        }
    }
}

pub fn should_compact(estimated: u32, context_window: u32, settings: &CompactionSettings) -> bool {
    if !settings.enabled {
        return false;
    }
    let window = if context_window == 0 {
        128_000
    } else {
        context_window
    };
    estimated + settings.reserve_tokens >= window
}

#[derive(Debug, Clone)]
pub struct CompactResult {
    pub summary: String,
    pub tokens_before: u32,
    pub replacement_messages: Vec<Message>,
}

pub async fn compact_messages(
    messages: &[Message],
    system: &str,
    provider: &dyn Provider,
    model: &Model,
    settings: &CompactionSettings,
) -> Result<CompactResult, String> {
    let tokens_before = estimate_messages_tokens(messages) + estimate_tokens(system);
    if messages.len() <= settings.retain_tail + 1 {
        return Err("not enough messages to compact".into());
    }
    let cut = find_cut_point(messages, settings.retain_tail);
    let (head, tail) = messages.split_at(cut);
    let serialized = serialize_conversation(head);
    let summary = match generate_summary(provider, model, &serialized).await {
        Ok(s) if !s.trim().is_empty() => s,
        _ => extractive_summary(head),
    };
    let mut replacement = vec![Message::user_text(format!(
        "<compaction_summary tokens_before=\"{tokens_before}\">\n{summary}\n</compaction_summary>"
    ))];
    replacement.extend(tail.iter().cloned());
    Ok(CompactResult {
        summary,
        tokens_before,
        replacement_messages: replacement,
    })
}

/// Cut on a turn boundary so we don't split an assistant message from its tool results.
pub fn find_cut_point(messages: &[Message], retain_tail: usize) -> usize {
    if messages.len() <= retain_tail {
        return 0;
    }
    let mut idx = messages.len() - retain_tail;
    while idx > 0 {
        match &messages[idx] {
            Message::User { .. } => break,
            _ => idx -= 1,
        }
    }
    idx.max(1)
}

pub fn serialize_conversation(messages: &[Message]) -> String {
    let mut out = String::new();
    for msg in messages {
        match msg {
            Message::System { content, .. } => {
                out.push_str("SYSTEM: ");
                out.push_str(content);
                out.push_str("\n\n");
            }
            Message::User { content, .. } => {
                out.push_str("USER: ");
                out.push_str(&content_text(content));
                out.push_str("\n\n");
            }
            Message::Assistant(a) => {
                out.push_str("ASSISTANT: ");
                out.push_str(&a.combined_text());
                for b in &a.content {
                    if let ContentBlock::ToolCall { name, arguments, .. } = b {
                        out.push_str(&format!("\n  tool {name}({arguments})"));
                    }
                }
                out.push_str("\n\n");
            }
            Message::Tool {
                tool_name, content, ..
            } => {
                out.push_str("TOOL[");
                out.push_str(tool_name);
                out.push_str("]: ");
                out.push_str(&content_text(content));
                out.push_str("\n\n");
            }
        }
    }
    out
}

async fn generate_summary(
    provider: &dyn Provider,
    model: &Model,
    serialized: &str,
) -> Result<String, String> {
    let prompt = format!(
        "Summarize this coding-agent transcript for future context. Preserve: user goals, files read/modified, decisions, errors, and remaining work. Be dense.\n\n{serialized}"
    );
    let req = StreamRequest {
        model: model.clone(),
        system: Some("You compress conversation history. Reply with the summary only.".into()),
        messages: vec![Message::user_text(prompt)],
        tools: vec![],
        options: StreamOptions {
            max_tokens: Some(1024),
            ..Default::default()
        },
    };
    match provider.complete(req).await {
        Ok(AssistantMessage {
            stop_reason: StopReason::EndTurn,
            content,
            ..
        }) => Ok(content_text(&content)),
        Ok(m) => m
            .error_message
            .ok_or_else(|| "summary generation failed".into())
            .and_then(|e| Err(e)),
        Err(e) => Err(e.to_string()),
    }
}

fn extractive_summary(messages: &[Message]) -> String {
    let mut files = Vec::new();
    let mut goals = Vec::new();
    for msg in messages {
        match msg {
            Message::User { content, .. } => {
                let t = content_text(content);
                if !t.is_empty() {
                    goals.push(t.chars().take(240).collect::<String>());
                }
            }
            Message::Assistant(a) => {
                for b in &a.content {
                    if let ContentBlock::ToolCall {
                        name, arguments, ..
                    } = b
                    {
                        if let Some(p) = arguments.get("path").and_then(|v| v.as_str()) {
                            files.push(format!("{name}:{p}"));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    format!(
        "Goals:\n{}\n\nFile operations:\n{}",
        goals.join("\n"),
        files.join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cut_point_lands_on_user() {
        let msgs = vec![
            Message::user_text("a"),
            Message::Assistant(AssistantMessage::text_only("b")),
            Message::user_text("c"),
            Message::Assistant(AssistantMessage::text_only("d")),
            Message::user_text("e"),
        ];
        let cut = find_cut_point(&msgs, 2);
        assert!(matches!(msgs[cut], Message::User { .. }));
    }
}
