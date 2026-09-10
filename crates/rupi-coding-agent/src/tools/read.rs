use async_trait::async_trait;
use rupi_agent_core::{AgentTool, AgentToolResult, PathGuard};
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;

pub struct ReadTool {
    pub cwd: PathBuf,
    pub guard: PathGuard,
}

#[async_trait]
impl AgentTool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }
    fn description(&self) -> &str {
        "Read a file. Optional offset (1-based) and limit (lines)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "offset": {"type": "integer"},
                "limit": {"type": "integer"}
            },
            "required": ["path"]
        })
    }
    async fn execute(&self, _id: &str, args: Value) -> AgentToolResult {
        let path = args["path"].as_str().unwrap_or("");
        let resolved = match self.guard.resolve(path) {
            Ok(p) => p,
            Err(e) => return AgentToolResult::err(e),
        };
        match fs::read_to_string(&resolved) {
            Ok(content) => {
                let lines: Vec<&str> = content.lines().collect();
                let offset = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
                let limit = args["limit"].as_u64().unwrap_or(lines.len() as u64) as usize;
                let start = offset.saturating_sub(1).min(lines.len());
                let end = (start + limit).min(lines.len());
                let mut out = String::new();
                for (i, line) in lines[start..end].iter().enumerate() {
                    out.push_str(&format!("{:>6}|{}\n", start + i + 1, line));
                }
                if out.is_empty() {
                    AgentToolResult::ok("(empty file)")
                } else {
                    AgentToolResult::ok(out)
                }
            }
            Err(e) => AgentToolResult::err(format!("{}: {e}", resolved.display())),
        }
    }
}
