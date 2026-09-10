use rupi_ai::content_text;
use rupi_ai::Message;

use crate::manage::SkillManageTool;
use rupi_memory::MemoryTool;

/// Outcome of a post-turn self-accumulation pass.
#[derive(Debug, Clone, Default)]
pub struct Accumulation {
    pub memories: Vec<String>,
    pub skills: Vec<String>,
    pub skipped: bool,
}

/// Hermes-style reviewer: after the user-facing turn settles, decide what is
/// worth keeping. The reviewer is sandboxed to memory + skill_manage.
pub struct Reviewer {
    pub enabled: bool,
}

impl Default for Reviewer {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Heuristic accumulator used when no extra LLM call is configured.
/// Extracts explicit remember-that facts and non-trivial workflows.
pub fn accumulate_from_transcript(
    messages: &[Message],
    memory: Option<&MemoryTool>,
    skills: Option<&SkillManageTool>,
) -> Accumulation {
    let mut acc = Accumulation::default();
    let blob = messages
        .iter()
        .map(|m| match m {
            Message::User { content, .. } | Message::Assistant { content, .. } => {
                content_text(content)
            }
            Message::ToolResult { content, .. } => content_text(content),
        })
        .collect::<Vec<_>>()
        .join("\n");

    if blob.trim().is_empty() {
        acc.skipped = true;
        return acc;
    }

    for fact in extract_remember_facts(&blob) {
        if let Some(mem) = memory {
            let args = serde_json::json!({"action": "add", "target": "memory", "content": &fact});
            match mem.apply(&args) {
                Ok(r) => acc.memories.push(format!("{r}: {fact}")),
                Err(_) => acc.memories.push(fact),
            }
        } else {
            acc.memories.push(fact);
        }
    }

    if let Some((name, description, body)) = extract_workflow(&blob) {
        if let Some(sk) = skills {
            let args = serde_json::json!({
                "action": "create",
                "name": name,
                "description": description,
                "content": body
            });
            if let Ok(r) = sk.apply(&args) {
                acc.skills.push(r);
            }
        } else {
            acc.skills.push(name);
        }
    }

    acc
}

fn extract_remember_facts(blob: &str) -> Vec<String> {
    let mut facts = Vec::new();
    for line in blob.lines() {
        let l = line.trim();
        let lower = l.to_ascii_lowercase();
        for prefix in ["remember that ", "please remember ", "don't forget "] {
            if let Some(idx) = lower.find(prefix) {
                let rest = l.get(idx + prefix.len()..).unwrap_or("").trim();
                if rest.len() > 8 {
                    facts.push(rest.trim_end_matches('.').to_string());
                }
            }
        }
        if lower.contains("i prefer ") && l.len() < 200 {
            facts.push(l.to_string());
        }
    }
    facts
}

fn extract_workflow(blob: &str) -> Option<(String, String, String)> {
    let lower = blob.to_ascii_lowercase();
    if !(lower.contains("workflow")
        || lower.contains("runbook")
        || lower.contains("next time")
        || lower.contains("the steps are"))
    {
        return None;
    }
    let body: String = blob.lines().rev().take(40).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
    if body.chars().count() < 80 {
        return None;
    }
    Some((
        "session-workflow".into(),
        "Workflow extracted from a prior session; review before reuse.".into(),
        body,
    ))
}
