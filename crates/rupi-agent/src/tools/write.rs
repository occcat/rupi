use super::{Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::fs;

pub struct WriteTool;

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the file to write (relative or absolute)"},
                "content": {"type": "string", "description": "Content to write to the file"}
            },
            "required": ["path", "content"]
        })
    }

    fn prompt_snippet(&self) -> &str {
        "Create or overwrite files"
    }

    fn prompt_guidelines(&self) -> &[&str] {
        &["Use write only for new files or complete rewrites."]
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> ToolResult {
        if ctx.aborted() {
            return ToolResult::err("Operation aborted");
        }
        let args: WriteArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolResult::err(format!("invalid arguments: {e}")),
        };
        let path = ctx.resolve(&args.path);
        if let Some(parent) = path.parent() {
            if let Err(e) = fs::create_dir_all(parent).await {
                return ToolResult::err(format!("failed to create parent directories: {e}"));
            }
        }
        match fs::write(&path, args.content.as_bytes()).await {
            Ok(()) => ToolResult::ok(format!(
                "Wrote {} bytes to {}",
                args.content.len(),
                path.display()
            ))
            .with_details(json!({"path": path.display().to_string()})),
            Err(e) => ToolResult::err(format!("failed to write {}: {e}", path.display())),
        }
    }
}
