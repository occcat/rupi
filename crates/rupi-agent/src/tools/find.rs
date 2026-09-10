use super::{Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use ignore::WalkBuilder;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::Path;

pub struct FindTool;
const MAX_RESULTS: usize = 200;

#[derive(Deserialize)]
struct FindArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
}

#[async_trait]
impl Tool for FindTool {
    fn name(&self) -> &str {
        "find"
    }

    fn description(&self) -> &str {
        "Find files by glob pattern (e.g. **/*.rs, Cargo.toml). Honors .gitignore."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Glob pattern to match against relative paths"},
                "path": {"type": "string", "description": "Directory to search (default: cwd)"}
            },
            "required": ["pattern"]
        })
    }

    fn prompt_snippet(&self) -> &str {
        "Find files by glob pattern"
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> ToolResult {
        let args: FindArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolResult::err(format!("invalid arguments: {e}")),
        };
        let root = args
            .path
            .as_deref()
            .map(|p| ctx.resolve(p))
            .unwrap_or_else(|| ctx.cwd.clone());
        let pattern = args.pattern;
        let mut hits = Vec::new();
        for entry in WalkBuilder::new(&root).hidden(false).git_ignore(true).build() {
            if hits.len() >= MAX_RESULTS {
                break;
            }
            let Ok(entry) = entry else { continue };
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let rel = entry
                .path()
                .strip_prefix(&root)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .replace('\\', "/");
            if glob_match(&pattern, &rel) || glob_match(&pattern, file_name(entry.path())) {
                hits.push(entry.path().display().to_string());
            }
        }
        if hits.is_empty() {
            ToolResult::ok("No files found.")
        } else {
            let truncated = hits.len() >= MAX_RESULTS;
            let mut body = hits.join("\n");
            if truncated {
                body.push_str(&format!("\n\n[truncated to {MAX_RESULTS} results]"));
            }
            ToolResult::ok(body).with_details(json!({"truncated": truncated}))
        }
    }
}

fn file_name(path: &Path) -> &str {
    path.file_name().and_then(|s| s.to_str()).unwrap_or("")
}

fn glob_match(pattern: &str, text: &str) -> bool {
    glob_to_regex(pattern)
        .ok()
        .map(|re| re.is_match(text))
        .unwrap_or(false)
}

fn glob_to_regex(pattern: &str) -> Result<regex::Regex, regex::Error> {
    let mut out = String::from("^");
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' if i + 1 < chars.len() && chars[i + 1] == '*' => {
                out.push_str(".*");
                i += 2;
                if i < chars.len() && chars[i] == '/' {
                    i += 1;
                    out.push_str("/?");
                }
                continue;
            }
            '*' => out.push_str("[^/]*"),
            '?' => out.push_str("[^/]"),
            '.' | '+' | '(' | ')' | '|' | '^' | '$' | '{' | '}' | '[' | ']' | '\\' => {
                out.push('\\');
                out.push(chars[i]);
            }
            c => out.push(c),
        }
        i += 1;
    }
    out.push('$');
    regex::Regex::new(&out)
}
