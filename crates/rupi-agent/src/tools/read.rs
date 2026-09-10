use super::{Tool, ToolContext, ToolResult};
use crate::truncate::{format_size, truncate_head, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::fs;

pub struct ReadTool;

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
    #[serde(default)]
    offset: Option<u32>,
    #[serde(default)]
    limit: Option<u32>,
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read the contents of a file. For text files, output is truncated to 2000 lines or 50KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the file to read (relative or absolute)"},
                "offset": {"type": "integer", "description": "Line number to start reading from (1-indexed)"},
                "limit": {"type": "integer", "description": "Maximum number of lines to read"}
            },
            "required": ["path"]
        })
    }

    fn prompt_snippet(&self) -> &str {
        "Read file contents"
    }

    fn prompt_guidelines(&self) -> &[&str] {
        &["Use read to examine files instead of cat or sed."]
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> ToolResult {
        if ctx.aborted() {
            return ToolResult::err("Operation aborted");
        }
        let args: ReadArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolResult::err(format!("invalid arguments: {e}")),
        };
        let path = ctx.resolve(&args.path);
        match fs::read(&path).await {
            Ok(bytes) => {
                if looks_binary(&bytes) {
                    return ToolResult::err(format!(
                        "refusing to read binary file {} ({} bytes)",
                        path.display(),
                        bytes.len()
                    ));
                }
                let mut text = String::from_utf8_lossy(&bytes).into_owned();
                if let Some(offset) = args.offset {
                    let start = offset.saturating_sub(1) as usize;
                    let lines: Vec<&str> = text.lines().collect();
                    let slice = if start >= lines.len() {
                        Vec::new()
                    } else {
                        let end = args
                            .limit
                            .map(|l| (start + l as usize).min(lines.len()))
                            .unwrap_or(lines.len());
                        lines[start..end].to_vec()
                    };
                    text = numbered_lines(&slice, start + 1);
                    return ToolResult::ok(text).with_details(json!({
                        "path": path.display().to_string(),
                        "offset": offset,
                    }));
                }
                if let Some(limit) = args.limit {
                    let lines: Vec<&str> = text.lines().take(limit as usize).collect();
                    text = numbered_lines(&lines, 1);
                    return ToolResult::ok(text);
                }
                let truncation = truncate_head(&text, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
                let numbered = number_content(&truncation.content);
                let mut out = numbered;
                if truncation.truncated {
                    out.push_str(&format!(
                        "\n\n[truncated: showing {} / {} lines, {} / {} ({})]",
                        truncation.output_lines,
                        truncation.total_lines,
                        format_size(truncation.output_bytes),
                        format_size(truncation.total_bytes),
                        truncation.truncated_by.unwrap_or("unknown")
                    ));
                }
                ToolResult::ok(out).with_details(json!({
                    "path": path.display().to_string(),
                    "truncated": truncation.truncated,
                }))
            }
            Err(e) => ToolResult::err(format!("failed to read {}: {e}", path.display())),
        }
    }
}

fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8000).any(|b| *b == 0)
}

fn numbered_lines(lines: &[&str], start: usize) -> String {
    lines
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:>6}|{}", start + i, line))
        .collect::<Vec<_>>()
        .join("\n")
}

fn number_content(content: &str) -> String {
    if content.is_empty() {
        return String::new();
    }
    let lines: Vec<&str> = content.lines().collect();
    numbered_lines(&lines, 1)
}
