use super::{Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::fs;

pub struct LsTool;

#[derive(Deserialize)]
struct LsArgs {
    #[serde(default)]
    path: Option<String>,
}

#[async_trait]
impl Tool for LsTool {
    fn name(&self) -> &str {
        "ls"
    }

    fn description(&self) -> &str {
        "List directory contents. Defaults to the working directory."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Directory to list (relative or absolute)"}
            }
        })
    }

    fn prompt_snippet(&self) -> &str {
        "List directory contents"
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> ToolResult {
        let args: LsArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolResult::err(format!("invalid arguments: {e}")),
        };
        let path = args
            .path
            .as_deref()
            .map(|p| ctx.resolve(p))
            .unwrap_or_else(|| ctx.cwd.clone());
        let mut entries = match fs::read_dir(&path).await {
            Ok(e) => e,
            Err(e) => return ToolResult::err(format!("failed to list {}: {e}", path.display())),
        };
        let mut names = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            let mut name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                name.push('/');
            }
            names.push(name);
        }
        names.sort();
        if names.is_empty() {
            ToolResult::ok("(empty directory)")
        } else {
            ToolResult::ok(names.join("\n")).with_details(json!({
                "path": path.display().to_string(),
                "count": names.len()
            }))
        }
    }
}
