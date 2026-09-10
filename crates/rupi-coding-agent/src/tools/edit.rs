use async_trait::async_trait;
use rupi_agent_core::{AgentTool, AgentToolResult, PathGuard};
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;

pub struct EditTool {
    pub cwd: PathBuf,
    pub guard: PathGuard,
}

#[async_trait]
impl AgentTool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }
    fn description(&self) -> &str {
        "Replace exact `old_text` with `new_text` in a file. Fails if old_text is missing or not unique unless replace_all is true."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "old_text": {"type": "string"},
                "new_text": {"type": "string"},
                "replace_all": {"type": "boolean"}
            },
            "required": ["path", "old_text", "new_text"]
        })
    }
    async fn execute(&self, _id: &str, args: Value) -> AgentToolResult {
        let path = args["path"].as_str().unwrap_or("");
        let old = args["old_text"].as_str().unwrap_or("");
        let new = args["new_text"].as_str().unwrap_or("");
        let replace_all = args["replace_all"].as_bool().unwrap_or(false);
        let resolved = match self.guard.resolve(path) {
            Ok(p) => p,
            Err(e) => return AgentToolResult::err(e),
        };
        let original = match fs::read_to_string(&resolved) {
            Ok(s) => s,
            Err(e) => return AgentToolResult::err(e.to_string()),
        };
        let count = original.matches(old).count();
        if count == 0 {
            return AgentToolResult::err("old_text not found");
        }
        if count > 1 && !replace_all {
            return AgentToolResult::err(format!(
                "old_text matched {count} times; pass replace_all=true or provide a unique substring"
            ));
        }
        let updated = if replace_all {
            original.replace(old, new)
        } else {
            original.replacen(old, new, 1)
        };
        match fs::write(&resolved, updated) {
            Ok(()) => AgentToolResult::ok(format!("updated {}", resolved.display())),
            Err(e) => AgentToolResult::err(e.to_string()),
        }
    }
}
