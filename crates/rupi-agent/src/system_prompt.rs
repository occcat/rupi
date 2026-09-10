use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct ToolSnippet {
    pub name: String,
    pub snippet: String,
}

#[derive(Debug, Clone)]
pub struct SkillPromptEntry {
    pub name: String,
    pub description: String,
    pub file_path: String,
    pub disable_model_invocation: bool,
}

#[derive(Debug, Clone, Default)]
pub struct BuildSystemPromptOptions {
    pub custom_prompt: Option<String>,
    pub selected_tools: Vec<String>,
    pub tool_snippets: Vec<ToolSnippet>,
    pub prompt_guidelines: Vec<String>,
    pub append_system_prompt: Vec<String>,
    pub cwd: PathBuf,
    pub context_files: Vec<(String, String)>,
    pub skills: Vec<SkillPromptEntry>,
    pub memory_block: Option<String>,
    pub docs_hint: Option<String>,
}

pub fn build_system_prompt(options: BuildSystemPromptOptions) -> String {
    let cwd = options.cwd.to_string_lossy().replace('\\', "/");
    let append = if options.append_system_prompt.is_empty() {
        String::new()
    } else {
        format!("\n\n{}", options.append_system_prompt.join("\n\n"))
    };

    if let Some(custom) = options.custom_prompt {
        let mut prompt = custom;
        prompt.push_str(&append);
        prompt.push_str(&format_context(&options.context_files));
        prompt.push_str(&format_skills(&options.skills, &options.selected_tools));
        if let Some(mem) = &options.memory_block {
            prompt.push_str("\n\n");
            prompt.push_str(mem);
        }
        prompt.push_str(&format!("\nCurrent working directory: {cwd}\n"));
        return prompt;
    }

    let visible: Vec<&ToolSnippet> = options
        .tool_snippets
        .iter()
        .filter(|s| options.selected_tools.is_empty() || options.selected_tools.iter().any(|n| n == &s.name))
        .collect();
    let tools_list = if visible.is_empty() {
        "(none)".into()
    } else {
        visible
            .iter()
            .map(|s| format!("- {}: {}", s.name, s.snippet))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let mut guidelines = Vec::new();
    let has_bash = options.selected_tools.iter().any(|t| t == "bash");
    let has_grep = options.selected_tools.iter().any(|t| t == "grep");
    let has_find = options.selected_tools.iter().any(|t| t == "find");
    let has_ls = options.selected_tools.iter().any(|t| t == "ls");
    if has_bash && !has_grep && !has_find && !has_ls {
        guidelines.push("Use bash for file operations like ls, rg, find".to_string());
    }
    for g in &options.prompt_guidelines {
        let t = g.trim();
        if !t.is_empty() && !guidelines.iter().any(|x| x == t) {
            guidelines.push(t.to_string());
        }
    }
    guidelines.push("Be concise in your responses".into());
    guidelines.push("Show file paths clearly when working with files".into());
    guidelines.push("Save durable facts with the memory tool; save reusable procedures as skills".into());

    let guidelines_text = guidelines
        .iter()
        .map(|g| format!("- {g}"))
        .collect::<Vec<_>>()
        .join("\n");

    let mut prompt = format!(
        "You are an expert coding assistant operating inside rupi, a Rust port of the Pi coding agent harness. You help users by reading files, executing commands, editing code, and writing new files.\n\nAvailable tools:\n{tools_list}\n\nIn addition to the tools above, you may have access to other custom tools depending on the project (MCP, memory, skills).\n\nGuidelines:\n{guidelines_text}"
    );

    if let Some(hint) = options.docs_hint {
        prompt.push_str("\n\n");
        prompt.push_str(&hint);
    }

    prompt.push_str(&append);
    prompt.push_str(&format_context(&options.context_files));
    prompt.push_str(&format_skills(&options.skills, &options.selected_tools));
    if let Some(mem) = &options.memory_block {
        prompt.push_str("\n\n");
        prompt.push_str(mem);
    }
    prompt.push_str(&format!("\nCurrent working directory: {cwd}"));
    prompt
}

fn format_context(files: &[(String, String)]) -> String {
    if files.is_empty() {
        return String::new();
    }
    let mut out = String::from("\n\n<project_context>\n\nProject-specific instructions and guidelines:\n\n");
    for (path, content) in files {
        out.push_str(&format!(
            "<project_instructions path=\"{path}\">\n{content}\n</project_instructions>\n\n"
        ));
    }
    out.push_str("</project_context>\n");
    out
}

fn format_skills(skills: &[SkillPromptEntry], tools: &[String]) -> String {
    let can_read = tools.iter().any(|t| t == "read" || t == "bash" || t == "skill_view");
    if !can_read {
        return String::new();
    }
    let visible: Vec<&SkillPromptEntry> = skills
        .iter()
        .filter(|s| !s.disable_model_invocation)
        .collect();
    if visible.is_empty() {
        return String::new();
    }
    let mut lines = vec![
        String::new(),
        "The following skills provide specialized instructions for specific tasks.".into(),
        "Read the full skill file when the task matches its description (use skill_view, or read if skill_view is unavailable).".into(),
        "When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md) and use that absolute path in tool commands.".into(),
        String::new(),
        "<available_skills>".into(),
    ];
    for skill in visible {
        lines.push("  <skill>".into());
        lines.push(format!("    <name>{}</name>", escape_xml(&skill.name)));
        lines.push(format!(
            "    <description>{}</description>",
            escape_xml(&skill.description)
        ));
        lines.push(format!(
            "    <location>{}</location>",
            escape_xml(&skill.file_path)
        ));
        lines.push("  </skill>".into());
    }
    lines.push("</available_skills>".into());
    lines.join("\n")
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

    #[allow(dead_code)]
    pub fn skill_entries(skills: Vec<SkillPromptEntry>) -> Vec<SkillPromptEntry> {
    skills
}
