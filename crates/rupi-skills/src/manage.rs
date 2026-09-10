use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rupi_agent_core::{AgentTool, AgentToolResult};
use serde_json::{json, Value};

use crate::load::{valid_name, Skill};

#[derive(Debug, Clone, Copy)]
pub struct SkillWriteApproval {
    pub required: bool,
}

pub struct SkillManageTool {
    pub skills_dir: PathBuf,
    pub approval: SkillWriteApproval,
    pub pending: Arc<Mutex<Vec<Value>>>,
}

impl SkillManageTool {
    pub fn new(skills_dir: PathBuf) -> Self {
        Self {
            skills_dir,
            approval: SkillWriteApproval { required: false },
            pending: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn apply(&self, args: &Value) -> Result<String, String> {
        let action = args["action"].as_str().unwrap_or("");
        match action {
            "create" | "edit" | "write_file" => {
                let name = args["name"].as_str().ok_or("name required")?;
                if !valid_name(name) {
                    return Err("name must be lowercase a-z, 0-9, hyphens".into());
                }
                let description = args["description"].as_str().unwrap_or("");
                let body = args["content"].as_str().or(args["body"].as_str()).unwrap_or("");
                if self.approval.required {
                    self.pending.lock().unwrap().push(args.clone());
                    return Ok("staged for approval".into());
                }
                self.write_skill(name, description, body)
            }
            "delete" | "remove_file" => {
                let name = args["name"].as_str().ok_or("name required")?;
                if self.approval.required {
                    self.pending.lock().unwrap().push(args.clone());
                    return Ok("staged for approval".into());
                }
                let path = self.skill_path(name);
                if path.exists() {
                    if path.is_dir() {
                        fs::remove_dir_all(&path).map_err(|e| e.to_string())?;
                    } else {
                        fs::remove_file(&path).map_err(|e| e.to_string())?;
                    }
                    Ok(format!("deleted {name}"))
                } else {
                    Err(format!("skill `{name}` not found"))
                }
            }
            "list" => {
                let mut names = Vec::new();
                if self.skills_dir.is_dir() {
                    for e in fs::read_dir(&self.skills_dir).map_err(|e| e.to_string())? {
                        let e = e.map_err(|e| e.to_string())?;
                        names.push(e.file_name().to_string_lossy().into_owned());
                    }
                }
                names.sort();
                Ok(names.join("\n"))
            }
            "view" | "read" => {
                let name = args["name"].as_str().ok_or("name required")?;
                let path = self.skill_path(name);
                let file = if path.is_dir() {
                    path.join("SKILL.md")
                } else {
                    path
                };
                fs::read_to_string(file).map_err(|e| e.to_string())
            }
            "patch" => {
                let name = args["name"].as_str().ok_or("name required")?;
                let old = args["old_text"].as_str().ok_or("old_text required")?;
                let new = args["new_text"].as_str().or(args["content"].as_str()).ok_or("new_text required")?;
                if self.approval.required {
                    self.pending.lock().unwrap().push(args.clone());
                    return Ok("staged for approval".into());
                }
                let path = {
                    let p = self.skill_path(name);
                    if p.is_dir() {
                        p.join("SKILL.md")
                    } else {
                        p
                    }
                };
                let body = fs::read_to_string(&path).map_err(|e| e.to_string())?;
                if !body.contains(old) {
                    return Err("old_text not found".into());
                }
                let updated = body.replacen(old, new, 1);
                fs::write(&path, updated).map_err(|e| e.to_string())?;
                Ok("patched".into())
            }
            _ => Err("action must be create, edit, patch, delete, list, or view".into()),
        }
    }

    fn skill_path(&self, name: &str) -> PathBuf {
        let dir = self.skills_dir.join(name);
        if dir.is_dir() {
            dir
        } else if dir.join("SKILL.md").exists() {
            dir
        } else {
            self.skills_dir.join(format!("{name}.md"))
        }
    }

    fn write_skill(&self, name: &str, description: &str, body: &str) -> Result<String, String> {
        fs::create_dir_all(&self.skills_dir).map_err(|e| e.to_string())?;
        let dir = self.skills_dir.join(name);
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let content = format!(
            "---\nname: {name}\ndescription: {description}\n---\n\n{body}\n"
        );
        fs::write(dir.join("SKILL.md"), content).map_err(|e| e.to_string())?;
        Ok(format!("wrote skill {name}"))
    }

    pub fn read_skill(&self, name: &str) -> Option<Skill> {
        let path = {
            let p = self.skill_path(name);
            if p.is_dir() {
                p.join("SKILL.md")
            } else {
                p
            }
        };
        crate::load::load_skills(&[path.parent()?.to_path_buf()])
            .0
            .into_iter()
            .next()
    }
}

#[async_trait]
impl AgentTool for SkillManageTool {
    fn name(&self) -> &str {
        "skill_manage"
    }
    fn description(&self) -> &str {
        "Create, update, delete, list, or view agent-managed skills (procedural memory)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["create", "edit", "patch", "delete", "list", "view"]},
                "name": {"type": "string"},
                "description": {"type": "string"},
                "content": {"type": "string"},
                "body": {"type": "string"},
                "old_text": {"type": "string"},
                "new_text": {"type": "string"}
            },
            "required": ["action"]
        })
    }
    async fn execute(&self, _id: &str, args: Value) -> AgentToolResult {
        match self.apply(&args) {
            Ok(s) => AgentToolResult::ok(s),
            Err(e) => AgentToolResult::err(e),
        }
    }
}
