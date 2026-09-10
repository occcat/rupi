use crate::discover::{valid_skill_name, Skill};
use crate::view::SkillLibrary;
use async_trait::async_trait;
use rupi_agent::{Tool, ToolContext, ToolResult};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillOrigin {
    Foreground,
    BackgroundReview,
}

pub struct SkillManageTool {
    library: Arc<SkillLibrary>,
    skills_dir: PathBuf,
    origin: SkillOrigin,
    write_approval: bool,
    pending_dir: PathBuf,
}

impl SkillManageTool {
    pub fn new(library: Arc<SkillLibrary>, skills_dir: PathBuf) -> Self {
        let pending_dir = skills_dir
            .parent()
            .unwrap_or(skills_dir.as_path())
            .join("pending")
            .join("skills");
        Self {
            library,
            skills_dir,
            origin: SkillOrigin::Foreground,
            write_approval: false,
            pending_dir,
        }
    }

    pub fn with_origin(mut self, origin: SkillOrigin) -> Self {
        self.origin = origin;
        self
    }

    pub fn with_write_approval(mut self, enabled: bool) -> Self {
        self.write_approval = enabled;
        self
    }
}

#[async_trait]
impl Tool for SkillManageTool {
    fn name(&self) -> &str {
        "skill_manage"
    }

    fn description(&self) -> &str {
        "Create, edit, patch, or delete skills (procedural memory). Actions: create, edit, patch, delete, write_file, remove_file. Skills live as SKILL.md packages (agentskills.io)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "edit", "patch", "delete", "write_file", "remove_file"]
                },
                "name": {"type": "string"},
                "content": {"type": "string", "description": "Full SKILL.md for create/edit"},
                "description": {"type": "string"},
                "old_string": {"type": "string"},
                "new_string": {"type": "string"},
                "file_path": {"type": "string"},
                "file_content": {"type": "string"}
            },
            "required": ["action", "name"]
        })
    }

    fn prompt_snippet(&self) -> &str {
        "Create or update reusable skills from experience"
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> ToolResult {
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
        if let Err(e) = valid_skill_name(name) {
            return ToolResult::err(e);
        }
        match action {
            "create" => self.create(name, &args),
            "edit" => self.edit(name, &args),
            "patch" => self.patch(name, &args),
            "delete" => self.delete(name),
            "write_file" => self.write_file(name, &args),
            "remove_file" => self.remove_file(name, &args),
            _ => ToolResult::err("unknown action"),
        }
    }
}

impl SkillManageTool {
    fn skill_dir(&self, name: &str) -> PathBuf {
        self.skills_dir.join(name)
    }

    fn skill_md(&self, name: &str) -> PathBuf {
        self.skill_dir(name).join("SKILL.md")
    }

    fn require_viewed(&self, path: &Path) -> Result<(), String> {
        if self.origin != SkillOrigin::BackgroundReview {
            return Ok(());
        }
        if self.library.has_viewed(path) || !path.exists() {
            return Ok(());
        }
        Err(format!(
            "Refusing background curator write for '{}': the current content has not been loaded in this review turn. Call skill_view(name) for SKILL.md, or skill_view(name, file_path=...) for a supporting file, then retry the write using the content just returned.",
            path.display()
        ))
    }

    fn maybe_stage(&self, dest: &Path, contents: &str) -> Result<String, String> {
        if self.write_approval {
            fs::create_dir_all(&self.pending_dir).map_err(|e| e.to_string())?;
            let staged = self.pending_dir.join(format!(
                "{}.pending",
                dest.file_name().unwrap_or_default().to_string_lossy()
            ));
            fs::write(&staged, contents).map_err(|e| e.to_string())?;
            return Ok(format!(
                "staged write at {} (write_approval=true); apply from /skills pending",
                staged.display()
            ));
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        fs::write(dest, contents).map_err(|e| e.to_string())?;
        Ok(format!("wrote {}", dest.display()))
    }

    fn create(&self, name: &str, args: &Value) -> ToolResult {
        let dir = self.skill_dir(name);
        if dir.exists() {
            return ToolResult::err(format!("skill `{name}` already exists"));
        }
        let description = args
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("Agent-created skill.");
        let body = args.get("content").and_then(|v| v.as_str()).map(|s| s.to_string());
        let content = body.unwrap_or_else(|| {
            format!(
                "---\nname: {name}\ndescription: {description}\n---\n\n# {name}\n\n{description}\n"
            )
        });
        if !content.contains("---") {
            return ToolResult::err("SKILL.md must include YAML frontmatter with name and description");
        }
        match self.maybe_stage(&dir.join("SKILL.md"), &content) {
            Ok(msg) => {
                let _ = fs::create_dir_all(dir.join("references"));
                ToolResult::ok(format!("created skill `{name}`: {msg}"))
            }
            Err(e) => ToolResult::err(e),
        }
    }

    fn edit(&self, name: &str, args: &Value) -> ToolResult {
        let path = self.skill_md(name);
        if let Err(e) = self.require_viewed(&path) {
            return ToolResult::err(e);
        }
        let Some(content) = args.get("content").and_then(|v| v.as_str()) else {
            return ToolResult::err("content is required for edit");
        };
        match self.maybe_stage(&path, content) {
            Ok(msg) => ToolResult::ok(format!("edited `{name}`: {msg}")),
            Err(e) => ToolResult::err(e),
        }
    }

    fn patch(&self, name: &str, args: &Value) -> ToolResult {
        let rel = args.get("file_path").and_then(|v| v.as_str());
        let path = match rel {
            Some(r) => self.skill_dir(name).join(r),
            None => self.skill_md(name),
        };
        if let Err(e) = self.require_viewed(&path) {
            return ToolResult::err(e);
        }
        let old = args.get("old_string").and_then(|v| v.as_str()).unwrap_or("");
        let new = args.get("new_string").and_then(|v| v.as_str()).unwrap_or("");
        if old.is_empty() {
            return ToolResult::err("old_string is required for patch");
        }
        let original = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => return ToolResult::err(format!("read failed: {e}")),
        };
        let count = original.matches(old).count();
        if count == 0 {
            return ToolResult::err("old_string not found");
        }
        if count > 1 {
            return ToolResult::err("old_string matched multiple times; make it unique");
        }
        let updated = original.replacen(old, new, 1);
        match self.maybe_stage(&path, &updated) {
            Ok(msg) => ToolResult::ok(format!("patched `{name}`: {msg}")),
            Err(e) => ToolResult::err(e),
        }
    }

    fn delete(&self, name: &str) -> ToolResult {
        let dir = self.skill_dir(name);
        if !dir.exists() {
            return ToolResult::err(format!("skill `{name}` not found"));
        }
        if self.write_approval {
            return ToolResult::ok(format!(
                "delete of `{name}` staged (write_approval=true); not removed"
            ));
        }
        match fs::remove_dir_all(&dir) {
            Ok(()) => ToolResult::ok(format!("deleted skill `{name}`")),
            Err(e) => ToolResult::err(e.to_string()),
        }
    }

    fn write_file(&self, name: &str, args: &Value) -> ToolResult {
        let rel = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
        if rel.is_empty() || rel.contains("..") {
            return ToolResult::err("file_path is required and must not contain ..");
        }
        if !(rel.starts_with("references/")
            || rel.starts_with("templates/")
            || rel.starts_with("scripts/")
            || rel.starts_with("assets/"))
        {
            return ToolResult::err(
                "file_path must start with references/, templates/, scripts/, or assets/",
            );
        }
        let path = self.skill_dir(name).join(rel);
        if let Err(e) = self.require_viewed(&path) {
            if path.exists() {
                return ToolResult::err(e);
            }
        }
        let content = args.get("file_content").and_then(|v| v.as_str()).unwrap_or("");
        match self.maybe_stage(&path, content) {
            Ok(msg) => ToolResult::ok(msg),
            Err(e) => ToolResult::err(e),
        }
    }

    fn remove_file(&self, name: &str, args: &Value) -> ToolResult {
        let rel = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
        let path = self.skill_dir(name).join(rel);
        if rel.is_empty() || rel.contains("..") || rel == "SKILL.md" {
            return ToolResult::err("refusing to remove that path");
        }
        match fs::remove_file(&path) {
            Ok(()) => ToolResult::ok(format!("removed {}", path.display())),
            Err(e) => ToolResult::err(e.to_string()),
        }
    }
}

    #[allow(dead_code)]
    pub fn snapshot_library(skills: &[Skill]) -> String {
    skills
        .iter()
        .map(|s| format!("- {}: {}", s.name, s.description))
        .collect::<Vec<_>>()
        .join("\n")
}
