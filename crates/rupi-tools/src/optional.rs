//! 可选内建：`find` / `ls`。默认不进 [`super::ToolRegistry::with_builtins`]，
//! 由 `--tools` / `settings.tools` 打开。

use super::{glob_rel_matches, ignore_walker, spawn_blocking_tool, Tool, ToolOutput};
use async_trait::async_trait;
use ignore::WalkState;
use rupi_core::ToolDefinition;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 可选内建名（默认关）。
pub const OPTIONAL_BUILTIN_NAMES: &[&str] = &["find", "ls"];

/// 从 `--tools` / `settings.tools` 白名单里抽出要打开的可选内建（去重、保序）。
pub fn optional_builtins_requested<'a>(
    lists: impl IntoIterator<Item = Option<&'a [String]>>,
) -> Vec<String> {
    let mut out = Vec::new();
    for list in lists.into_iter().flatten() {
        for n in list {
            if OPTIONAL_BUILTIN_NAMES.contains(&n.as_str()) && !out.iter().any(|e| e == n) {
                out.push(n.clone());
            }
        }
    }
    out
}

const FIND_DEFAULT_LIMIT: usize = 1000;
const LS_DEFAULT_LIMIT: usize = 500;

/// 按 glob 找文件（对标 Pi `find`）：相对搜索目录的路径，遵守 `.gitignore`。
pub struct FindTool;

#[async_trait]
impl Tool for FindTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "find".into(),
            description: "Search for files by glob pattern. Returns matching file paths relative to the search directory. Respects .gitignore. Default-off builtin: enable with --tools find or settings.tools.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "glob like *.rs or **/*.json (no .., no absolute)"},
                    "path": {"type": "string", "description": "directory to search (default .)"},
                    "limit": {"type": "integer", "description": "max results (default 1000)"}
                },
                "required": ["pattern"]
            }),
            prompt_snippet: Some("find(pattern, path?, limit?): files by glob (respects .gitignore)".into()),
        }
    }

    async fn execute(&self, arguments: serde_json::Value) -> anyhow::Result<ToolOutput> {
        let pattern = arguments
            .get("pattern")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if pattern.is_empty() {
            return Ok(ToolOutput::err("find requires pattern"));
        }
        if pattern.contains("..") {
            return Ok(ToolOutput::err("find pattern must not contain '..'"));
        }
        if Path::new(pattern).is_absolute() {
            return Ok(ToolOutput::err("find pattern must be relative"));
        }
        if let Err(e) = glob::Pattern::new(pattern) {
            return Ok(ToolOutput::err(format!("bad glob pattern: {e}")));
        }
        let base = arguments
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(".")
            .to_owned();
        let limit = arguments
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(FIND_DEFAULT_LIMIT as u64)
            .clamp(1, 10_000) as usize;
        let pattern = pattern.to_owned();
        Ok(spawn_blocking_tool(move || find_blocking(base, pattern, limit)).await)
    }
}

fn find_blocking(base: String, pattern: String, limit: usize) -> ToolOutput {
    let root = PathBuf::from(&base);
    if !root.exists() {
        return ToolOutput::err(format!("Path not found: {base}"));
    }
    if !root.is_dir() {
        return ToolOutput::err(format!("Not a directory: {base}"));
    }
    let hits = Mutex::new(Vec::new());
    let extra = std::sync::atomic::AtomicUsize::new(0);
    ignore_walker(&root).build_parallel().run(|| {
        let hits = &hits;
        let extra = &extra;
        let pattern = &pattern;
        let root = &root;
        Box::new(move |res| {
            let Ok(ent) = res else {
                return WalkState::Continue;
            };
            let path = ent.path();
            if !path.is_file() {
                return WalkState::Continue;
            }
            let rel = path.strip_prefix(root).unwrap_or(path);
            if !glob_rel_matches(pattern, rel) {
                return WalkState::Continue;
            }
            let mut g = hits.lock().unwrap();
            if g.len() >= limit {
                extra.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return WalkState::Quit;
            }
            g.push(rel.to_string_lossy().replace('\\', "/"));
            WalkState::Continue
        })
    });
    let mut hits = hits.into_inner().unwrap();
    hits.sort();
    let truncated = extra.load(std::sync::atomic::Ordering::Relaxed);
    let mut out = hits.join("\n");
    if truncated > 0 {
        out.push_str(&format!("\n...[truncated {truncated} more]"));
    }
    ToolOutput::ok(out)
}

/// 列目录（对标 Pi `ls`）：一层、含点文件、目录带尾 `/`。
pub struct LsTool;

#[async_trait]
impl Tool for LsTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "ls".into(),
            description: "List directory contents. Returns entries sorted alphabetically, with '/' suffix for directories. Includes dotfiles. Default-off builtin: enable with --tools ls or settings.tools.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "directory to list (default .)"},
                    "limit": {"type": "integer", "description": "max entries (default 500)"}
                }
            }),
            prompt_snippet: Some("ls(path?, limit?): list directory entries".into()),
        }
    }

    async fn execute(&self, arguments: serde_json::Value) -> anyhow::Result<ToolOutput> {
        let base = arguments
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(".")
            .to_owned();
        let limit = arguments
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(LS_DEFAULT_LIMIT as u64)
            .clamp(1, 10_000) as usize;
        Ok(spawn_blocking_tool(move || ls_blocking(base, limit)).await)
    }
}

fn ls_blocking(base: String, limit: usize) -> ToolOutput {
    let root = PathBuf::from(&base);
    if !root.exists() {
        return ToolOutput::err(format!("Path not found: {base}"));
    }
    if !root.is_dir() {
        return ToolOutput::err(format!("Not a directory: {base}"));
    }
    let rd = match std::fs::read_dir(&root) {
        Ok(rd) => rd,
        Err(e) => return ToolOutput::err(format!("read directory failed: {e}")),
    };
    let mut entries = Vec::new();
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        if name == "." || name == ".." {
            continue;
        }
        let is_dir = ent.path().is_dir();
        entries.push(if is_dir { format!("{name}/") } else { name });
    }
    entries.sort_by(|a, b| a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase()));
    let total = entries.len();
    if total > limit {
        entries.truncate(limit);
        let mut out = entries.join("\n");
        out.push_str(&format!("\n...[truncated {} more]", total - limit));
        ToolOutput::ok(out)
    } else {
        ToolOutput::ok(entries.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requested_only_optional_names() {
        let a = vec!["read".into(), "find".into(), "think".into()];
        let b = vec!["ls".into(), "find".into()];
        assert_eq!(
            optional_builtins_requested([Some(a.as_slice()), Some(b.as_slice())]),
            vec!["find".to_string(), "ls".to_string()]
        );
        assert!(optional_builtins_requested([None, Some(&[] as &[String])]).is_empty());
    }
}
