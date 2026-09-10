//! Hermes-style background self-improvement review.
//! After a qualifying turn/session, a forked agent with only memory + skill tools
//! decides whether to persist facts or procedural skills.

use crate::discover::Skill;
use crate::manage::{SkillManageTool, SkillOrigin};
use crate::view::{SkillLibrary, SkillViewTool, SkillsListTool};
use rupi_agent::{run_agent_loop, AgentLoopConfig, ToolRegistry};
use rupi_ai::{content_text, Message, Model, Provider};
use rupi_memory::MemoryTool;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub const MEMORY_REVIEW_PROMPT: &str = "Review the conversation above and consider saving to memory if appropriate.\n\n\
Focus on:\n\
1. Has the user revealed things about themselves — their persona, desires, preferences, or personal details worth remembering?\n\
2. Has the user expressed expectations about how you should behave, their work style, or ways they want you to operate?\n\n\
If something stands out, save it using the memory tool. If nothing is worth saving, just say 'Nothing to save.' and stop.";

pub const SKILL_REVIEW_PROMPT: &str = "Review the conversation above and update the skill library. Be ACTIVE — most sessions produce at least one skill update, even if small.\n\n\
Target shape of the library: CLASS-LEVEL skills, each with a rich SKILL.md and a `references/` directory for session-specific detail.\n\n\
Signals to look for (any one of these warrants action):\n\
 • User corrected your style, tone, format, or workflow.\n\
 • A non-trivial technique, fix, workaround, or tool-usage pattern emerged.\n\
 • A skill that got loaded this session was wrong, missing a step, or outdated.\n\n\
Preference order:\n\
 1. UPDATE A CURRENTLY-LOADED SKILL. Call skill_view(name) BEFORE any skill_manage patch/edit/write_file (read-before-write).\n\
 2. UPDATE AN EXISTING UMBRELLA via skills_list + skill_view, then patch.\n\
 3. ADD A SUPPORT FILE under references/, templates/, or scripts/.\n\
 4. CREATE A NEW CLASS-LEVEL UMBRELLA SKILL when no existing skill covers the class. The name MUST NOT be a session artifact (PR number, 'fix-X-today').\n\n\
If nothing is worth saving, say 'Nothing to save.' and stop.";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewSettings {
    pub enabled: bool,
    pub min_tool_calls: usize,
    pub write_approval: bool,
}

impl Default for ReviewSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            min_tool_calls: 1,
            write_approval: false,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ReviewOutcome {
    pub ran: bool,
    pub skipped_reason: Option<String>,
    pub assistant_text: String,
    pub tool_names: Vec<String>,
}

    #[allow(dead_code)]
    pub fn should_trigger(tool_count: usize, settings: &ReviewSettings) -> bool {
    settings.enabled && tool_count >= settings.min_tool_calls
}

pub async fn run_self_improvement_review(
    transcript: &[Message],
    provider: Arc<dyn Provider>,
    model: Model,
    memory: Arc<rupi_memory::MemoryStore>,
    skills: Vec<Skill>,
    skills_dir: std::path::PathBuf,
    cwd: std::path::PathBuf,
    settings: &ReviewSettings,
) -> ReviewOutcome {
    if !settings.enabled {
        return ReviewOutcome {
            skipped_reason: Some("disabled".into()),
            ..Default::default()
        };
    }
    let library = SkillLibrary::new(skills);
    let mut tools = ToolRegistry::empty();
    tools.push(Arc::new(MemoryTool::new(memory)));
    tools.push(Arc::new(SkillsListTool::new(library.clone())));
    tools.push(Arc::new(SkillViewTool::new(library.clone())));
    tools.push(Arc::new(
        SkillManageTool::new(library, skills_dir)
            .with_origin(SkillOrigin::BackgroundReview)
            .with_write_approval(settings.write_approval),
    ));

    let mut replay = transcript.to_vec();
    replay.push(Message::user_text(format!(
        "{MEMORY_REVIEW_PROMPT}\n\n---\n\n{SKILL_REVIEW_PROMPT}"
    )));

    let config = AgentLoopConfig::new(
        model,
        provider,
        tools,
        "You are a background self-improvement reviewer. You may ONLY use memory, skills_list, skill_view, and skill_manage. Do not claim to have used other tools.".into(),
        cwd,
    );

    let mut tool_names = Vec::new();
    let msgs = run_agent_loop(replay[replay.len().saturating_sub(1)..].to_vec(), {
        let mut hist = transcript.to_vec();
        // already included last prompt separately; history is prior conversation
        hist.truncate(hist.len().saturating_sub(0));
        transcript.to_vec()
    }, &config, |ev| {
        if let rupi_agent::AgentEvent::ToolExecutionStart { name, .. } = ev {
            tool_names.push(name);
        }
    })
    .await;

    let assistant_text = msgs
        .iter()
        .rev()
        .find_map(|m| match m {
            Message::Assistant(a) => {
                let t = a.combined_text();
                if t.is_empty() {
                    None
                } else {
                    Some(t)
                }
            }
            _ => None,
        })
        .unwrap_or_default();

    ReviewOutcome {
        ran: true,
        skipped_reason: None,
        assistant_text,
        tool_names,
    }
}

    #[allow(dead_code)]
    pub fn digest_transcript(messages: &[Message]) -> String {
    let mut out = String::new();
    for msg in messages {
        match msg {
            Message::User { content, .. } => {
                out.push_str("USER: ");
                out.push_str(&content_text(content));
                out.push('\n');
            }
            Message::Assistant(a) => {
                out.push_str("ASSISTANT: ");
                out.push_str(&a.combined_text());
                out.push('\n');
            }
            Message::Tool {
                tool_name, content, ..
            } => {
                out.push_str("TOOL[");
                out.push_str(tool_name);
                out.push_str("]: ");
                out.push_str(&content_text(content));
                out.push('\n');
            }
            _ => {}
        }
    }
    out
}
