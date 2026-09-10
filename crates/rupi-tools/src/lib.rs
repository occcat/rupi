//! rupi-tools: Tool trait + Pi 默认四件套 Read / Write / Edit / Bash + 注册表。

use async_trait::async_trait;
use rupi_core::ToolDefinition;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }
    pub fn err(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDefinition;
    async fn execute(&self, arguments: serde_json::Value) -> anyhow::Result<ToolOutput>;
}

#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.definition().name.clone(), tool);
    }

    /// 注销（热重载删除扩展时用）。
    pub fn unregister(&mut self, name: &str) -> bool {
        self.tools.remove(name).is_some()
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|t| t.definition()).collect()
    }

    pub async fn execute(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> anyhow::Result<ToolOutput> {
        match self.tools.get(name) {
            Some(t) => t.execute(arguments).await,
            None => Ok(ToolOutput::err(format!("unknown tool: {name}"))),
        }
    }

    /// 默认四件套，与 Pi 保持一致。
    pub fn with_builtins() -> Self {
        let mut r = Self::new();
        r.register(Arc::new(ReadTool));
        r.register(Arc::new(WriteTool));
        r.register(Arc::new(EditTool));
        r.register(Arc::new(BashTool));
        r
    }

    /// 沙箱四件套：read/write/edit 的 `path` 约束在 `root` 内（bash/mcp/扩展进程不在此层约束）。
    pub fn with_sandboxed_builtins(root: &std::path::Path) -> Self {
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let mut r = Self::new();
        r.register(Arc::new(SandboxedTool::new(
            Arc::new(ReadTool),
            root.clone(),
        )));
        r.register(Arc::new(SandboxedTool::new(
            Arc::new(WriteTool),
            root.clone(),
        )));
        r.register(Arc::new(SandboxedTool::new(
            Arc::new(EditTool),
            root.clone(),
        )));
        r.register(Arc::new(BashTool));
        r
    }
}

/// 工作区沙箱守卫：`path` 参数解析（相对→root 下，绝对→原样），canonicalize 消解
/// `..` 与符号链接后必须仍在 `root` 内，否则拒绝执行。缺 `path` 参数透传给内层判错。
pub struct SandboxedTool {
    inner: Arc<dyn Tool>,
    root: std::path::PathBuf,
}

impl SandboxedTool {
    pub fn new(inner: Arc<dyn Tool>, root: std::path::PathBuf) -> Self {
        Self { inner, root }
    }

    /// root（canonicalize 过的绝对路径），供调用方展示。
    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    fn resolve(&self, path: &str) -> Option<std::path::PathBuf> {
        let p = std::path::Path::new(path);
        let joined = if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.root.join(p)
        };
        // 存在文件直接 canonicalize；新建文件则 canonicalize 父目录后拼回文件名
        if let Ok(c) = joined.canonicalize() {
            return Some(c);
        }
        let parent = joined.parent()?;
        let c = parent.canonicalize().ok()?;
        Some(c.join(joined.file_name()?))
    }
}

#[async_trait]
impl Tool for SandboxedTool {
    fn definition(&self) -> ToolDefinition {
        self.inner.definition()
    }

    async fn execute(&self, arguments: serde_json::Value) -> anyhow::Result<ToolOutput> {
        let mut arguments = arguments;
        if let Some(path) = arguments
            .get("path")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
        {
            match self.resolve(&path) {
                // 放行并改写为绝对路径：内层不再依赖进程 cwd，结果稳定
                Some(c) if c.starts_with(&self.root) => {
                    arguments["path"] = serde_json::Value::String(c.to_string_lossy().into_owned());
                }
                _ => {
                    return Ok(ToolOutput::err(format!(
                        "path escapes workspace root ({}): {path}",
                        self.root.display()
                    )))
                }
            }
        }
        self.inner.execute(arguments).await
    }
}

// ---- builtins ----

pub struct ReadTool;
#[async_trait]
impl Tool for ReadTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read".into(),
            description: "Read a file from disk (paged; large files are truncated)".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "file path"},
                    "offset": {"type": "integer", "description": "0-based start line (default 0)"},
                    "limit": {"type": "integer", "description": "max lines (default 2000, max 5000)"}
                },
                "required": ["path"]
            }),
            prompt_snippet: Some("read(path, offset?, limit?): read file content, paged".into()),
        }
    }
    async fn execute(&self, arguments: serde_json::Value) -> anyhow::Result<ToolOutput> {
        let path = arguments.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let offset = arguments
            .get("offset")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let limit = arguments
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(2000)
            .clamp(1, 5000) as usize;
        match tokio::fs::read_to_string(path).await {
            Ok(c) => {
                let lines: Vec<&str> = c.lines().collect();
                if offset >= lines.len() {
                    return Ok(ToolOutput::ok(format!(
                        "[read {path}: offset {offset} past end ({} lines)]",
                        lines.len()
                    )));
                }
                let end = (offset + limit).min(lines.len());
                let mut s = lines[offset..end].join("\n");
                if s.len() < c.len() && end < lines.len() {
                    s.push_str(&format!(
                        "\n…[truncated: lines {}-{} of {} — pass offset={} for more]",
                        offset + 1,
                        end,
                        lines.len(),
                        end
                    ));
                } else if end == lines.len() && offset > 0 {
                    s.push_str(&format!("\n[end of file: {} lines]", lines.len()));
                }
                // 单行超长同样截断（minified/二进制行），保上下文有界
                Ok(ToolOutput::ok(truncate_middle(&s, MAX_TOOL_OUTPUT)))
            }
            Err(e) => Ok(ToolOutput::err(format!("read {path} failed: {e}"))),
        }
    }
}

pub struct WriteTool;
#[async_trait]
impl Tool for WriteTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "write".into(),
            description: "Write content to a file (creates parent dirs)".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"]
            }),
            prompt_snippet: Some("write(path, content): create/overwrite file".into()),
        }
    }
    async fn execute(&self, arguments: serde_json::Value) -> anyhow::Result<ToolOutput> {
        let path = arguments.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let content = arguments
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if path.is_empty() {
            return Ok(ToolOutput::err("path required"));
        }
        if let Some(parent) = std::path::Path::new(path).parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await?;
            }
        }
        match tokio::fs::write(path, content).await {
            Ok(()) => Ok(ToolOutput::ok(format!(
                "wrote {path} ({} bytes)",
                content.len()
            ))),
            Err(e) => Ok(ToolOutput::err(format!("write failed: {e}"))),
        }
    }
}

pub struct EditTool;
#[async_trait]
impl Tool for EditTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "edit".into(),
            description: "Exact string replacement in a file".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_string": {"type": "string"},
                    "new_string": {"type": "string"}
                },
                "required": ["path", "old_string", "new_string"]
            }),
            prompt_snippet: Some("edit(path, old_string, new_string): exact replacement".into()),
        }
    }
    async fn execute(&self, arguments: serde_json::Value) -> anyhow::Result<ToolOutput> {
        let path = arguments.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let old = arguments
            .get("old_string")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let new = arguments
            .get("new_string")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let content = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| anyhow::anyhow!("read failed: {e}"))?;
        if !content.contains(old) {
            return Ok(ToolOutput::err("old_string not found"));
        }
        let updated = content.replacen(old, new, 1);
        tokio::fs::write(path, updated).await?;
        Ok(ToolOutput::ok(format!("edited {path}")))
    }
}

pub struct BashTool;
#[async_trait]
impl Tool for BashTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "bash".into(),
            description: "Execute a shell command (bounded output; default 30s timeout)".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "timeout_secs": {"type": "integer", "description": "1-300, default 30"}
                },
                "required": ["command"]
            }),
            prompt_snippet: Some("bash(command, timeout_secs?): run shell command".into()),
        }
    }
    async fn execute(&self, arguments: serde_json::Value) -> anyhow::Result<ToolOutput> {
        let command = arguments
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let timeout_secs = arguments
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(30)
            .clamp(1, 300);
        let run = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .output();
        match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), run).await {
            Ok(Ok(out)) => {
                let mut s = String::from_utf8_lossy(&out.stdout).to_string();
                if !out.stderr.is_empty() {
                    s.push_str(&format!(
                        "\n[stderr]\n{}",
                        String::from_utf8_lossy(&out.stderr)
                    ));
                }
                s = truncate_middle(&s, MAX_TOOL_OUTPUT);
                if out.status.success() {
                    Ok(ToolOutput::ok(s))
                } else {
                    Ok(ToolOutput::err(format!("exit {}: {s}", out.status)))
                }
            }
            Ok(Err(e)) => Ok(ToolOutput::err(format!("spawn failed: {e}"))),
            Err(_) => Ok(ToolOutput::err(format!(
                "command timed out after {timeout_secs}s"
            ))),
        }
    }
}

/// 工具回包有界：超限保留首尾、中部折叠并标注截掉字符数，保上下文窗口不被大输出撑爆。
pub const MAX_TOOL_OUTPUT: usize = 12_000;

pub fn truncate_middle(s: &str, limit: usize) -> String {
    if s.len() <= limit {
        return s.to_owned();
    }
    let keep = limit / 2;
    let head: String = s.chars().take(keep).collect();
    let tail: String = s
        .chars()
        .rev()
        .take(keep)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!(
        "{head}\n…[truncated {} chars]…\n{tail}",
        s.len().saturating_sub(limit)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn builtins_register_and_unknown_errors() {
        let r = ToolRegistry::with_builtins();
        assert_eq!(r.definitions().len(), 4);
        let out = r.execute("nope", serde_json::json!({})).await.unwrap();
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn write_read_roundtrip() {
        let dir = std::env::temp_dir().join(format!("rupi-test-{}", std::process::id()));
        let p = dir.join("a.txt");
        let r = ToolRegistry::with_builtins();
        let path = p.to_string_lossy().to_string();
        r.execute(
            "write",
            serde_json::json!({"path": path, "content": "hello"}),
        )
        .await
        .unwrap();
        let out = r
            .execute("read", serde_json::json!({"path": path}))
            .await
            .unwrap();
        assert_eq!(out.content, "hello");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn read_pages_and_marks_truncation() {
        let dir = std::env::temp_dir().join(format!("rupi-pg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let r = ToolRegistry::with_builtins();
        let path = dir.join("big.txt").to_string_lossy().to_string();
        let body: String = (1..=50)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        r.execute("write", serde_json::json!({"path": path, "content": body}))
            .await
            .unwrap();
        // 第一页：10 行 + 截断标注（含 offset 指引）
        let p1 = r
            .execute("read", serde_json::json!({"path": path, "limit": 10}))
            .await
            .unwrap();
        assert!(!p1.is_error);
        assert!(p1.content.starts_with("line1\nline2"));
        assert!(p1.content.contains("truncated: lines 1-10 of 50"));
        assert!(p1.content.contains("offset=10"));
        // 第二页：offset 翻页，末页带 end-of-file
        let p2 = r
            .execute(
                "read",
                serde_json::json!({"path": path, "offset": 40, "limit": 20}),
            )
            .await
            .unwrap();
        assert!(p2.content.starts_with("line41"));
        assert!(p2.content.contains("[end of file: 50 lines]"));
        // 越界 offset 给明确提示而非空串
        let past = r
            .execute("read", serde_json::json!({"path": path, "offset": 99}))
            .await
            .unwrap();
        assert!(past.content.contains("past end"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn bash_truncates_huge_output_and_honors_timeout() {
        let r = ToolRegistry::with_builtins();
        // 大输出折叠：保留首尾 + 标注截掉字符数，总长有界
        let big = r
            .execute("bash", serde_json::json!({"command": "seq 1 200000"}))
            .await
            .unwrap();
        assert!(!big.is_error);
        assert!(big.content.contains("[truncated "));
        assert!(big.content.len() <= MAX_TOOL_OUTPUT + 256);
        assert!(big.content.starts_with("1\n2\n"));
        // 超时参数生效（1s 杀掉 sleep 5）
        let slow = r
            .execute(
                "bash",
                serde_json::json!({"command": "sleep 5", "timeout_secs": 1}),
            )
            .await
            .unwrap();
        assert!(slow.is_error);
        assert!(slow.content.contains("timed out after 1s"));
    }

    #[tokio::test]
    async fn sandbox_blocks_escapes_allows_inside() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!("rupi-sbx-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/inner.txt"), "inner").unwrap();
        let r = ToolRegistry::with_sandboxed_builtins(&root);
        // 内部相对路径读写正常
        let ok = r
            .execute("read", serde_json::json!({"path": "sub/inner.txt"}))
            .await
            .unwrap();
        assert!(!ok.is_error, "{}", ok.content);
        // `..` 逃逸拒绝
        let esc = r
            .execute("read", serde_json::json!({"path": "../nope.txt"}))
            .await
            .unwrap();
        assert!(esc.is_error);
        assert!(esc.content.contains("escapes workspace root"));
        // 绝对路径逃逸拒绝
        let abs = r
            .execute(
                "write",
                serde_json::json!({"path": "/tmp/rupi-sbx-outside.txt", "content": "x"}),
            )
            .await
            .unwrap();
        assert!(abs.is_error);
        // 符号链接逃逸拒绝
        symlink("/tmp", root.join("sub/link")).unwrap();
        let link = r
            .execute("read", serde_json::json!({"path": "sub/link/nope.txt"}))
            .await
            .unwrap();
        assert!(link.is_error);
        // 不存在的新文件（父目录在内）放行，由内层 write 正常创建
        let fresh = r
            .execute(
                "write",
                serde_json::json!({"path": "sub/fresh.txt", "content": "new"}),
            )
            .await
            .unwrap();
        assert!(!fresh.is_error, "{}", fresh.content);
        assert!(!std::path::Path::new("/tmp/rupi-sbx-outside.txt").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
