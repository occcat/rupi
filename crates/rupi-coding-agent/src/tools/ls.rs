use async_trait::async_trait;
use rupi_agent_core::{AgentTool, AgentToolResult, PathGuard};
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;

pub struct LsTool {
    pub cwd: PathBuf,
    pub guard: PathGuard,
}

#[async_trait]
impl AgentTool for LsTool {
    fn name(&self) -> &str {
        "ls"
    }
    fn description(&self) -> &str {
        "List a directory."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"}
            }
        })
    }
    async fn execute(&self, _id: &str, args: Value) -> AgentToolResult {
        let path = args["path"].as_str().unwrap_or(".");
        let resolved = match self.guard.resolve(path) {
            Ok(p) => p,
            Err(e) => return AgentToolResult::err(e),
        };
        match fs::read_dir(&resolved) {
            Ok(rd) => {
                let mut names: Vec<String> = rd
                    .flatten()
                    .map(|e| {
                        let n = e.file_name().to_string_lossy().into_owned();
                        if e.path().is_dir() {
                            format!("{n}/")
                        } else {
                            n
                        }
                    })
                    .collect();
                names.sort();
                AgentToolResult::ok(names.join("\n"))
            }
            Err(e) => AgentToolResult::err(e.to_string()),
        }
    }
}
