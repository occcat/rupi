use async_trait::async_trait;
use rupi_agent_core::{AgentTool, AgentToolResult, PathGuard};
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;

pub struct WriteTool {
    pub cwd: PathBuf,
    pub guard: PathGuard,
}

#[async_trait]
impl AgentTool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }
    fn description(&self) -> &str {
        "Write contents to a file, creating parent directories as needed."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "content": {"type": "string"}
            },
            "required": ["path", "content"]
        })
    }
    async fn execute(&self, _id: &str, args: Value) -> AgentToolResult {
        let path = args["path"].as_str().unwrap_or("");
        let content = args["content"].as_str().unwrap_or("");
        let resolved = match self.guard.resolve(path) {
            Ok(p) => p,
            Err(e) => return AgentToolResult::err(e),
        };
        if let Some(parent) = resolved.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                return AgentToolResult::err(e.to_string());
            }
        }
        match fs::write(&resolved, content) {
            Ok(()) => AgentToolResult::ok(format!("wrote {} bytes to {}", content.len(), resolved.display())),
            Err(e) => AgentToolResult::err(e.to_string()),
        }
    }
}
