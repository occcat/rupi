use crate::discover::Skill;
use async_trait::async_trait;
use rupi_agent::{Tool, ToolContext, ToolResult};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub struct SkillLibrary {
    pub skills: Vec<Skill>,
    pub viewed: Mutex<HashSet<PathBuf>>,
}

impl SkillLibrary {
    pub fn new(skills: Vec<Skill>) -> Arc<Self> {
        Arc::new(Self {
            skills,
            viewed: Mutex::new(HashSet::new()),
        })
    }

    pub fn find(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.name == name)
    }

    pub fn mark_viewed(&self, path: PathBuf) {
        self.viewed.lock().unwrap().insert(path);
    }

    pub fn has_viewed(&self, path: &std::path::Path) -> bool {
        self.viewed.lock().unwrap().contains(path)
    }

    pub fn reload_from(&mut self, skills: Vec<Skill>) {
        self.skills = skills;
    }
}

pub struct SkillViewTool {
    library: Arc<SkillLibrary>,
}

impl SkillViewTool {
    pub fn new(library: Arc<SkillLibrary>) -> Self {
        Self { library }
    }
}

#[async_trait]
impl Tool for SkillViewTool {
    fn name(&self) -> &str {
        "skill_view"
    }

    fn description(&self) -> &str {
        "Load a skill's SKILL.md (or a supporting file). Progressive disclosure: only names/descriptions sit in the system prompt until you call this."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Skill name"},
                "file_path": {"type": "string", "description": "Optional supporting file relative to the skill directory (e.g. references/notes.md)"}
            },
            "required": ["name"]
        })
    }

    fn prompt_snippet(&self) -> &str {
        "Load full skill instructions on demand"
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> ToolResult {
        let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let Some(skill) = self.library.find(name) else {
            return ToolResult::err(format!("unknown skill `{name}`"));
        };
        if let Some(rel) = args.get("file_path").and_then(|v| v.as_str()) {
            let rel = rel.replace('\\', "/");
            if rel.contains("..") {
                return ToolResult::err("file_path must not contain ..");
            }
            let path = skill.base_dir.join(&rel);
            match std::fs::read_to_string(&path) {
                Ok(content) => {
                    self.library.mark_viewed(path.clone());
                    ToolResult::ok(format!(
                        "<skill_file name=\"{}\" path=\"{}\">\n{content}\n</skill_file>",
                        skill.name,
                        path.display()
                    ))
                }
                Err(e) => ToolResult::err(format!("failed to read {}: {e}", path.display())),
            }
        } else {
            self.library.mark_viewed(skill.file_path.clone());
            ToolResult::ok(format!(
                "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{}\n</skill>",
                skill.name,
                skill.file_path.display(),
                skill.base_dir.display(),
                skill.content
            ))
        }
    }
}

pub struct SkillsListTool {
    library: Arc<SkillLibrary>,
}

impl SkillsListTool {
    pub fn new(library: Arc<SkillLibrary>) -> Self {
        Self { library }
    }
}

#[async_trait]
impl Tool for SkillsListTool {
    fn name(&self) -> &str {
        "skills_list"
    }

    fn description(&self) -> &str {
        "List installed skills with name, description, and path."
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _args: Value, _ctx: &ToolContext) -> ToolResult {
        if self.library.skills.is_empty() {
            return ToolResult::ok("No skills installed.");
        }
        let body = self
            .library
            .skills
            .iter()
            .map(|s| format!("- {} — {} ({})", s.name, s.description, s.file_path.display()))
            .collect::<Vec<_>>()
            .join("\n");
        ToolResult::ok(body)
    }
}
