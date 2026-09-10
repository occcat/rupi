use super::{Tool, ToolContext, ToolResult};
use crate::truncate::GREP_MAX_LINE_LENGTH;
use async_trait::async_trait;
use ignore::WalkBuilder;
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

pub struct GrepTool;
const MAX_MATCHES: usize = 200;

#[derive(Deserialize)]
struct GrepArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default)]
    #[serde(rename = "caseInsensitive")]
    case_insensitive: Option<bool>,
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents with a regular expression. Recursively searches from path (default: cwd), honoring .gitignore."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Regular expression to search for"},
                "path": {"type": "string", "description": "Directory or file to search (default: cwd)"},
                "glob": {"type": "string", "description": "Optional glob filter, e.g. *.rs"},
                "caseInsensitive": {"type": "boolean"}
            },
            "required": ["pattern"]
        })
    }

    fn prompt_snippet(&self) -> &str {
        "Search file contents with regex"
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> ToolResult {
        let args: GrepArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolResult::err(format!("invalid arguments: {e}")),
        };
        let mut builder = regex::RegexBuilder::new(&args.pattern);
        builder.case_insensitive(args.case_insensitive.unwrap_or(false));
        let re = match builder.build() {
            Ok(r) => r,
            Err(e) => return ToolResult::err(format!("invalid regex: {e}")),
        };
        let root = args
            .path
            .as_deref()
            .map(|p| ctx.resolve(p))
            .unwrap_or_else(|| ctx.cwd.clone());
        if root.is_file() {
            return match search_file(&root, &re) {
                Ok(lines) => format_matches(lines),
                Err(e) => ToolResult::err(e),
            };
        }
        let glob = args.glob.clone();
        let mut matches = Vec::new();
        let walker = WalkBuilder::new(&root)
            .hidden(false)
            .git_ignore(true)
            .git_exclude(true)
            .build();
        for entry in walker {
            if matches.len() >= MAX_MATCHES {
                break;
            }
            let Ok(entry) = entry else { continue };
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let path = entry.path();
            if let Some(g) = &glob {
                if !glob_match(g, path) {
                    continue;
                }
            }
            if let Ok(found) = search_file(path, &re) {
                matches.extend(found);
            }
        }
        format_matches(matches)
    }
}

fn glob_match(pattern: &str, path: &Path) -> bool {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let full = path.to_string_lossy();
    wildcard_match(pattern, name) || wildcard_match(pattern, &full)
}

fn wildcard_match(pattern: &str, text: &str) -> bool {
    glob_to_regex(pattern)
        .ok()
        .map(|re| re.is_match(text))
        .unwrap_or(false)
}

fn glob_to_regex(pattern: &str) -> Result<Regex, regex::Error> {
    let mut out = String::from("^");
    for ch in pattern.chars() {
        match ch {
            '*' => out.push_str(".*"),
            '?' => out.push('.'),
            '.' | '+' | '(' | ')' | '|' | '^' | '$' | '{' | '}' | '[' | ']' | '\\' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out.push('$');
    Regex::new(&out)
}

fn search_file(path: &Path, re: &Regex) -> Result<Vec<String>, String> {
    let bytes = fs::read(path).map_err(|e| e.to_string())?;
    if bytes.iter().take(8000).any(|b| *b == 0) {
        return Ok(Vec::new());
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if re.is_match(line) {
            let mut clipped = line.to_string();
            if clipped.chars().count() > GREP_MAX_LINE_LENGTH {
                clipped = clipped.chars().take(GREP_MAX_LINE_LENGTH).collect();
                clipped.push('…');
            }
            out.push(format!("{}:{}:{}", path.display(), i + 1, clipped));
        }
    }
    Ok(out)
}

fn format_matches(matches: Vec<String>) -> ToolResult {
    if matches.is_empty() {
        return ToolResult::ok("No matches found.");
    }
    let truncated = matches.len() >= MAX_MATCHES;
    let mut body = matches.into_iter().take(MAX_MATCHES).collect::<Vec<_>>().join("\n");
    if truncated {
        body.push_str(&format!("\n\n[truncated to {MAX_MATCHES} matches]"));
    }
    ToolResult::ok(body).with_details(json!({"truncated": truncated}))
}
