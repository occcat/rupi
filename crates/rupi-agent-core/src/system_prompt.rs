//! System prompt assembly. Skill XML matches
//! `packages/agent/src/harness/system-prompt.ts`.

use chrono::Local;

#[derive(Debug, Clone, Default)]
pub struct SkillPromptEntry {
    pub name: String,
    pub description: String,
    pub location: String,
    pub disable_model_invocation: bool,
}

#[derive(Debug, Clone, Default)]
pub struct SystemPromptParts {
    pub identity: String,
    pub tool_guidance: String,
    pub skills: Vec<SkillPromptEntry>,
    pub context_files: Vec<(String, String)>,
    pub memory_block: String,
    pub append: Vec<String>,
    pub cwd: String,
}

pub fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub fn format_skills_for_system_prompt(skills: &[SkillPromptEntry]) -> String {
    let visible: Vec<_> = skills
        .iter()
        .filter(|s| !s.disable_model_invocation)
        .collect();
    if visible.is_empty() {
        return String::new();
    }
    let mut lines = vec![
        "The following skills provide specialized instructions for specific tasks.".into(),
        "Read the full skill file when the task matches its description.".into(),
        "When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.".into(),
        String::new(),
        "<available_skills>".into(),
    ];
    for skill in visible {
        lines.push(" <skill>".into());
        lines.push(format!(" <name>{}</name>", escape_xml(&skill.name)));
        lines.push(format!(
            " <description>{}</description>",
            escape_xml(&skill.description)
        ));
        lines.push(format!(
            " <location>{}</location>",
            escape_xml(&skill.location)
        ));
        lines.push(" </skill>".into());
    }
    lines.push("</available_skills>".into());
    lines.join("\n")
}

pub fn build_system_prompt(parts: &SystemPromptParts) -> String {
    let mut sections = Vec::new();
    let identity = if parts.identity.is_empty() {
        default_identity(&parts.cwd)
    } else {
        parts.identity.clone()
    };
    sections.push(identity);

    if !parts.tool_guidance.is_empty() {
        sections.push(parts.tool_guidance.clone());
    }

    let skills = format_skills_for_system_prompt(&parts.skills);
    if !skills.is_empty() {
        sections.push(skills);
    }

    if !parts.memory_block.is_empty() {
        sections.push(parts.memory_block.clone());
    }

    for (path, content) in &parts.context_files {
        sections.push(format!("# Context from {path}\n\n{content}"));
    }

    for extra in &parts.append {
        if !extra.trim().is_empty() {
            sections.push(extra.clone());
        }
    }

    sections.join("\n\n")
}

fn default_identity(cwd: &str) -> String {
    let now = Local::now().format("%Y-%m-%d %H:%M:%S %Z");
    format!(
        "You are rupi, a Rust recreation of the Pi coding agent (upstream @earendil-works/pi-coding-agent 0.85.1).\n\
         You help the user with software engineering tasks in the working directory `{cwd}`.\n\
         Current time: {now}.\n\
         Be concise. Use tools to inspect and change files. Prefer the smallest correct change.\n\
         Default tools are read, write, edit, and bash. Additional tools (grep/find/ls, MCP, memory, skills) may be present.\n\
         Load a skill with the read tool when the task matches a skill description. Do not dump skill bodies unless needed."
    )
}
