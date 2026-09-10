use async_trait::async_trait;
use rupi_agent_core::{AgentTool, AgentToolResult, PathGuard};
use regex::Regex;
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use walkdir::WalkDir;

pub struct GrepTool {
    pub cwd: PathBuf,
    pub guard: PathGuard,
}

#[async_trait]
impl AgentTool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }
    fn description(&self) -> &str {
        "Search file contents with a regex. Returns matching lines with paths."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string"},
                "path": {"type": "string"},
                "glob": {"type": "string"},
                "max_matches": {"type": "integer"}
            },
            "required": ["pattern"]
        })
    }
    async fn execute(&self, _id: &str, args: Value) -> AgentToolResult {
        let pattern = args["pattern"].as_str().unwrap_or("");
        let re = match Regex::new(pattern) {
            Ok(r) => r,
            Err(e) => return AgentToolResult::err(e.to_string()),
        };
        let root = args["path"].as_str().unwrap_or(".");
        let resolved = match self.guard.resolve(root) {
            Ok(p) => p,
            Err(e) => return AgentToolResult::err(e),
        };
        let glob = args["glob"].as_str();
        let max = args["max_matches"].as_u64().unwrap_or(50) as usize;
        let mut hits = Vec::new();
        for entry in WalkDir::new(&resolved).into_iter().filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() {
                continue;
            }
            if let Some(g) = glob {
                if let Some(name) = entry.path().file_name().and_then(|s| s.to_str()) {
                    if !glob_match(g, name) {
                        continue;
                    }
                }
            }
            if let Ok(text) = fs::read_to_string(entry.path()) {
                for (i, line) in text.lines().enumerate() {
                    if re.is_match(line) {
                        hits.push(format!("{}:{}:{}", entry.path().display(), i + 1, line));
                        if hits.len() >= max {
                            break;
                        }
                    }
                }
            }
            if hits.len() >= max {
                break;
            }
        }
        if hits.is_empty() {
            AgentToolResult::ok("no matches")
        } else {
            AgentToolResult::ok(hits.join("\n"))
        }
    }
}

fn glob_match(pat: &str, name: &str) -> bool {
    if let Some(suf) = pat.strip_prefix("*.") {
        name.ends_with(&format!(".{suf}")) || name.ends_with(suf)
    } else {
        name.contains(pat.trim_start_matches('*'))
    }
}
