use async_trait::async_trait;
use rupi_agent_core::{AgentTool, AgentToolResult, PathGuard};
use serde_json::{json, Value};
use std::path::PathBuf;
use walkdir::WalkDir;

pub struct FindTool {
    pub cwd: PathBuf,
    pub guard: PathGuard,
}

#[async_trait]
impl AgentTool for FindTool {
    fn name(&self) -> &str {
        "find"
    }
    fn description(&self) -> &str {
        "Find files by glob-ish name under a directory."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string"},
                "path": {"type": "string"},
                "max": {"type": "integer"}
            },
            "required": ["pattern"]
        })
    }
    async fn execute(&self, _id: &str, args: Value) -> AgentToolResult {
        let pattern = args["pattern"].as_str().unwrap_or("*").to_ascii_lowercase();
        let root = args["path"].as_str().unwrap_or(".");
        let resolved = match self.guard.resolve(root) {
            Ok(p) => p,
            Err(e) => return AgentToolResult::err(e),
        };
        let max = args["max"].as_u64().unwrap_or(100) as usize;
        let needle = pattern.trim_matches('*');
        let mut hits = Vec::new();
        for entry in WalkDir::new(&resolved).into_iter().filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if needle.is_empty() || name.contains(needle) {
                hits.push(entry.path().display().to_string());
                if hits.len() >= max {
                    break;
                }
            }
        }
        AgentToolResult::ok(if hits.is_empty() {
            "no files".into()
        } else {
            hits.join("\n")
        })
    }
}
