//! Precise multi-edit matching Pi's edit tool: all `oldText` values are matched
//! against the original file (not incrementally), must be unique, and must not overlap.

use super::{Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use similar::{ChangeTag, TextDiff};
use tokio::fs;

pub struct EditTool;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SingleEdit {
    old_text: String,
    new_text: String,
}

#[derive(Debug, Deserialize)]
struct EditArgs {
    path: String,
    #[serde(default)]
    edits: Option<Value>,
    #[serde(default)]
    #[serde(rename = "oldText")]
    old_text: Option<String>,
    #[serde(default)]
    #[serde(rename = "newText")]
    new_text: Option<String>,
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Make precise file edits with exact text replacement, including multiple disjoint edits in one call. Each edits[].oldText is matched against the original file, not incrementally. oldText must be unique and edits must not overlap."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the file to edit (relative or absolute)"},
                "edits": {
                    "type": "array",
                    "description": "One or more targeted replacements matched against the original file.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "oldText": {"type": "string"},
                            "newText": {"type": "string"}
                        },
                        "required": ["oldText", "newText"]
                    }
                }
            },
            "required": ["path", "edits"]
        })
    }

    fn prompt_snippet(&self) -> &str {
        "Make precise file edits with exact text replacement, including multiple disjoint edits in one call"
    }

    fn prompt_guidelines(&self) -> &[&str] {
        &[
            "Use edit for precise changes (edits[].oldText must match exactly)",
            "When changing multiple separate locations in one file, use one edit call with multiple entries in edits[] instead of multiple edit calls",
            "Each edits[].oldText is matched against the original file, not after earlier edits are applied. Do not emit overlapping or nested edits. Merge nearby changes into one edit.",
            "Keep edits[].oldText as small as possible while still being unique in the file. Do not pad with large unchanged regions.",
        ]
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> ToolResult {
        if ctx.aborted() {
            return ToolResult::err("Operation aborted");
        }
        let parsed: EditArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolResult::err(format!("invalid arguments: {e}")),
        };
        let edits = match collect_edits(&parsed) {
            Ok(e) => e,
            Err(e) => return ToolResult::err(e),
        };
        let path = ctx.resolve(&parsed.path);
        let original = match fs::read_to_string(&path).await {
            Ok(s) => s,
            Err(e) => return ToolResult::err(format!("failed to read {}: {e}", path.display())),
        };
        match apply_edits(&original, &edits) {
            Ok(new_content) => {
                if let Err(e) = fs::write(&path, new_content.as_bytes()).await {
                    return ToolResult::err(format!("failed to write {}: {e}", path.display()));
                }
                let diff = generate_diff(&original, &new_content, &path.display().to_string());
                ToolResult::ok(format!("Successfully edited {}\n\n{diff}", path.display()))
                    .with_details(json!({
                        "path": path.display().to_string(),
                        "diff": diff,
                    }))
            }
            Err(e) => ToolResult::err(e),
        }
    }
}

fn collect_edits(args: &EditArgs) -> Result<Vec<SingleEdit>, String> {
    let mut edits = Vec::new();
    if let Some(raw) = &args.edits {
        match raw {
            Value::Array(arr) => {
                for item in arr {
                    edits.push(parse_single(item)?);
                }
            }
            Value::String(s) => {
                let parsed: Value =
                    serde_json::from_str(s).map_err(|e| format!("edits JSON string is invalid: {e}"))?;
                if let Value::Array(arr) = parsed {
                    for item in &arr {
                        edits.push(parse_single(item)?);
                    }
                } else {
                    edits.push(parse_single(&parsed)?);
                }
            }
            other => edits.push(parse_single(other)?),
        }
    }
    if let (Some(old), Some(new)) = (&args.old_text, &args.new_text) {
        edits.push(SingleEdit {
            old_text: old.clone(),
            new_text: new.clone(),
        });
    }
    if edits.is_empty() {
        return Err("Edit tool input is invalid. edits must contain at least one replacement.".into());
    }
    Ok(edits)
}

fn parse_single(v: &Value) -> Result<SingleEdit, String> {
    serde_json::from_value(v.clone()).map_err(|e| format!("invalid edit entry: {e}"))
}

fn apply_edits(original: &str, edits: &[SingleEdit]) -> Result<String, String> {
    #[derive(Clone)]
    struct Span {
        start: usize,
        end: usize,
        new_text: String,
    }
    let mut spans = Vec::new();
    for (i, edit) in edits.iter().enumerate() {
        if edit.old_text.is_empty() {
            return Err(format!("edit[{i}].oldText must not be empty"));
        }
        let matches: Vec<usize> = original.match_indices(&edit.old_text).map(|(i, _)| i).collect();
        if matches.is_empty() {
            return Err(format!(
                "edit[{i}] oldText was not found in the file. It must match exactly, including whitespace."
            ));
        }
        if matches.len() > 1 {
            return Err(format!(
                "edit[{i}] oldText matched {} times. It must be unique in the file. Add surrounding context to make it unique.",
                matches.len()
            ));
        }
        let start = matches[0];
        let end = start + edit.old_text.len();
        spans.push(Span {
            start,
            end,
            new_text: edit.new_text.clone(),
        });
    }
    spans.sort_by_key(|s| s.start);
    for pair in spans.windows(2) {
        if pair[0].end > pair[1].start {
            return Err(
                "edits overlap or are nested. Each oldText is matched against the original file; merge nearby changes into one edit.".into(),
            );
        }
    }
    let mut out = String::with_capacity(original.len());
    let mut cursor = 0usize;
    for span in spans {
        out.push_str(&original[cursor..span.start]);
        out.push_str(&span.new_text);
        cursor = span.end;
    }
    out.push_str(&original[cursor..]);
    Ok(out)
}

fn generate_diff(old: &str, new: &str, path: &str) -> String {
    let diff = TextDiff::from_lines(old, new);
    let mut out = format!("--- a/{path}\n+++ b/{path}\n");
    for op in diff.ops() {
        for change in diff.iter_changes(op) {
            match change.tag() {
                ChangeTag::Delete => out.push_str(&format!("-{}", change.value())),
                ChangeTag::Insert => out.push_str(&format!("+{}", change.value())),
                ChangeTag::Equal => {}
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_disjoint_edits_against_original() {
        let src = "aaa\nbbb\nccc\n";
        let edits = vec![
            SingleEdit {
                old_text: "aaa".into(),
                new_text: "AAA".into(),
            },
            SingleEdit {
                old_text: "ccc".into(),
                new_text: "CCC".into(),
            },
        ];
        assert_eq!(apply_edits(src, &edits).unwrap(), "AAA\nbbb\nCCC\n");
    }

    #[test]
    fn rejects_non_unique() {
        let src = "foo foo";
        let edits = vec![SingleEdit {
            old_text: "foo".into(),
            new_text: "bar".into(),
        }];
        assert!(apply_edits(src, &edits).unwrap_err().contains("matched 2 times"));
    }
}
