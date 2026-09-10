//! rupi-tools: Tool trait + Pi 默认七件套 Read / Write / Edit / Bash / Glob / Grep / Think + 注册表。

use async_trait::async_trait;
use rupi_core::{CancelFlag, ToolDefinition};
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
    /// 带取消的执行入口：主循环把本轮 `CancelFlag` 透进来。默认直接调 `execute`
    /// （瞬间完成的工具无需重写）；长耗时工具（bash 子进程、子 agent 内层循环、
    /// 外部进程、MCP 远端）重写本方法做真抢占。注意返回 `Err` 会 abort 整轮，
    /// 取消一律走 `Ok(ToolOutput::err("cancelled by user"))` 回模型。
    async fn execute_with_cancel(
        &self,
        arguments: serde_json::Value,
        _cancel: &CancelFlag,
    ) -> anyhow::Result<ToolOutput> {
        self.execute(arguments).await
    }
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

    /// 带取消的执行入口（主循环用这个，把本轮 flag 一路透给工具）。
    pub async fn execute_with_cancel(
        &self,
        name: &str,
        arguments: serde_json::Value,
        cancel: &CancelFlag,
    ) -> anyhow::Result<ToolOutput> {
        match self.tools.get(name) {
            Some(t) => t.execute_with_cancel(arguments, cancel).await,
            None => Ok(ToolOutput::err(format!("unknown tool: {name}"))),
        }
    }

    /// 默认七件套，与 Pi 保持一致（read/write/edit/bash/glob/grep/think）。
    pub fn with_builtins() -> Self {
        let mut r = Self::new();
        r.register(Arc::new(ReadTool));
        r.register(Arc::new(WriteTool));
        r.register(Arc::new(EditTool));
        r.register(Arc::new(BashTool));
        r.register(Arc::new(GlobTool));
        r.register(Arc::new(GrepTool));
        r.register(Arc::new(ThinkTool));
        r
    }

    /// 沙箱七件套：read/write/edit/glob/grep 的 `path` 约束在 `root` 内
    ///（bash/mcp/扩展进程不在此层约束；glob 的 `pattern` 另禁 `..` 与绝对路径）。
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
        r.register(Arc::new(SandboxedTool::new(
            Arc::new(GlobTool),
            root.clone(),
        )));
        r.register(Arc::new(SandboxedTool::new(
            Arc::new(GrepTool),
            root.clone(),
        )));
        r.register(Arc::new(ThinkTool));
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

// ---- file mutation queue ----

/// 同文件写串行化（对标上游 `withFileMutationQueue`）：并行工具执行时，
/// 同一 canonical path 的 write/edit 排队通过，防 read-modify-write 丢更新；
/// 不同文件互不阻塞。异常路径（文件与父目录都不存在）退回原文 key，
/// 与上游 not_found/not_supported 时退回 absolutePath 同理。
pub struct FileMutationQueue {
    inner: std::sync::Mutex<HashMap<std::path::PathBuf, Arc<tokio::sync::Mutex<()>>>>,
}

impl FileMutationQueue {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(HashMap::new()),
        }
    }

    pub async fn with_queued<F, Fut, T>(&self, key: std::path::PathBuf, f: F) -> T
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let slot = {
            let mut inner = self.inner.lock().expect("mutation queue poisoned");
            inner
                .entry(key.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let guard = slot.lock().await;
        let out = f().await;
        drop(guard);
        // 仅剩 map 内与本地引用时回收槽位：后来者若在 remove 前 clone 过，
        // 计数≥3 即跳过删除，由它们排空后回收；remove 后新来者建新槽，
        // 此时临界区已结束，不存在并发重叠。
        let mut inner = self.inner.lock().expect("mutation queue poisoned");
        if Arc::strong_count(&slot) == 2 {
            inner.remove(&key);
        }
        out
    }
}

impl Default for FileMutationQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// 突变 key：存在文件取 canonical（消 `..`/符号链接，与沙箱 resolve 同口径）；
/// 新建文件取“父目录 canonical + 文件名”；都失败退回绝对路径原文。
pub fn mutation_key(path: &str) -> std::path::PathBuf {
    let p = std::path::Path::new(path);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    };
    if let Ok(c) = abs.canonicalize() {
        return c;
    }
    if let (Some(parent), Some(name)) = (abs.parent(), abs.file_name()) {
        if let Ok(c) = parent.canonicalize() {
            return c.join(name);
        }
    }
    abs
}

fn global_mutation_queue() -> &'static FileMutationQueue {
    static QUEUE: std::sync::LazyLock<FileMutationQueue> =
        std::sync::LazyLock::new(FileMutationQueue::new);
    &QUEUE
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
        let key = mutation_key(path);
        global_mutation_queue()
            .with_queued(key, || async {
                if let Some(parent) = std::path::Path::new(path).parent() {
                    if !parent.as_os_str().is_empty() {
                        tokio::fs::create_dir_all(parent).await?;
                    }
                }
                Ok(match tokio::fs::write(path, content).await {
                    Ok(()) => ToolOutput::ok(format!(
                        "wrote {path} ({} bytes)",
                        content.len()
                    )),
                    Err(e) => ToolOutput::err(format!("write failed: {e}")),
                })
            })
            .await
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
        let key = mutation_key(path);
        global_mutation_queue()
            .with_queued(key, || async {
                let content = tokio::fs::read_to_string(path)
                    .await
                    .map_err(|e| anyhow::anyhow!("read failed: {e}"))?;
                if !content.contains(old) {
                    return Ok(ToolOutput::err("old_string not found"));
                }
                let updated = content.replacen(old, new, 1);
                tokio::fs::write(path, updated).await?;
                Ok(ToolOutput::ok(format!("edited {path}")))
            })
            .await
    }
}

pub struct BashTool;
#[async_trait]
impl Tool for BashTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "bash".into(),
            description: "Execute a shell command (bounded output; default 30s timeout; $RUPI_SESSION_ID holds the current session id)".into(),
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
        let (command, timeout_secs) = Self::parse_args(&arguments);
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
    /// 真抢占：取消置位即 kill 子进程并回收，已产出内容随取消错误一并返回。
    async fn execute_with_cancel(
        &self,
        arguments: serde_json::Value,
        cancel: &CancelFlag,
    ) -> anyhow::Result<ToolOutput> {
        let (command, timeout_secs) = Self::parse_args(&arguments);
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg(command);
        // 自成进程组（setsid）：取消/超时杀整组，sh -c fork 出的孙进程不留孤儿。
        // BashTool 本就调 sh，Unix 假设与既有用例一致。
        use std::os::unix::process::CommandExt as _;
        cmd.as_std_mut().process_group(0);
        let mut child = match cmd
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => return Ok(ToolOutput::err(format!("spawn failed: {e}"))),
        };
        let timeout = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs));
        tokio::pin!(timeout);
        // wait 只等退出（&mut，不移动 child，select 三分支借用互不冲突），
        // stdout/stderr 随后手动排空（进程退出/kill 后读必到 EOF）。
        enum End {
            Done(std::process::ExitStatus),
            Cancelled,
            TimedOut,
        }
        let end = tokio::select! {
            _ = cancel.cancelled() => End::Cancelled,
            _ = &mut timeout => End::TimedOut,
            res = child.wait() => match res {
                Ok(status) => End::Done(status),
                Err(e) => return Ok(ToolOutput::err(format!("wait failed: {e}"))),
            },
        };
        match end {
            End::Done(status) => {
                let (out_text, err_text) = Self::drain(&mut child).await;
                let mut s = out_text;
                if !err_text.is_empty() {
                    s.push_str(&format!("\n[stderr]\n{err_text}"));
                }
                s = truncate_middle(&s, MAX_TOOL_OUTPUT);
                if status.success() {
                    Ok(ToolOutput::ok(s))
                } else {
                    Ok(ToolOutput::err(format!("exit {status}: {s}")))
                }
            }
            End::Cancelled => {
                Self::kill_tree(&mut child).await;
                // 残余输出只等 500ms：孙进程可能继承管道写端（如 sh -c fork 出子进程），
                // 无限等会拖到命令自然结束，违背取消语义；超时则舍弃残余直接返回。
                let (out_text, _) = tokio::time::timeout(
                    std::time::Duration::from_millis(500),
                    Self::drain(&mut child),
                )
                .await
                .unwrap_or_default();
                let partial = truncate_middle(&out_text, MAX_TOOL_OUTPUT);
                let mut msg = String::from("cancelled by user");
                if !partial.is_empty() {
                    msg.push_str(&format!("\n[partial output]\n{partial}"));
                }
                Ok(ToolOutput::err(msg))
            }
            End::TimedOut => {
                Self::kill_tree(&mut child).await;
                Ok(ToolOutput::err(format!(
                    "command timed out after {timeout_secs}s"
                )))
            }
        }
    }
}

impl BashTool {
    fn parse_args(arguments: &serde_json::Value) -> (&str, u64) {        let command = arguments
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let timeout_secs = arguments
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(30)
            .clamp(1, 300);
        (command, timeout_secs)
    }

    /// 杀整组进程树：先 killpg 发 SIGKILL（组长即直接子进程 pid，setsid 保证），
    /// 再补一次定向 kill 兜底，最后 wait 回收僵尸。killpg 失败一律忽略
    /// （组已空/进程已死的 ESRCH 等），wait 保证无僵尸。
    async fn kill_tree(child: &mut tokio::process::Child) {
        if let Some(pid) = child.id() {
            // SAFETY: killpg 仅向进程组发信号，不涉及内存，参数为刚取到的存活 pid。
            unsafe {
                libc::killpg(pid as libc::pid_t, libc::SIGKILL);
            }
        }
        let _ = child.kill().await;
        let _ = child.wait().await;
    }

    /// 排空子进程 stdout/stderr 管道。进程退出（或 kill+wait 回收）后读必到 EOF，
    /// 故本函数必返回，不会挂起。
    async fn drain(child: &mut tokio::process::Child) -> (String, String) {
        use tokio::io::AsyncReadExt as _;
        let mut out = Vec::new();
        let mut err = Vec::new();
        if let Some(mut o) = child.stdout.take() {
            let _ = o.read_to_end(&mut out).await;
        }
        if let Some(mut e) = child.stderr.take() {
            let _ = e.read_to_end(&mut err).await;
        }
        (
            String::from_utf8_lossy(&out).to_string(),
            String::from_utf8_lossy(&err).to_string(),
        )
    }
}

/// 按 glob 列文件（`**/*.rs`）。`path` 为基准目录（沙箱改写到 root 内），
/// `pattern` 禁 `..` 与绝对路径：两者配合结果恒在基准目录下。
pub struct GlobTool;
#[async_trait]
impl Tool for GlobTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "glob".into(),
            description: "List files matching a glob pattern under path (capped at 200)".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "glob like **/*.rs (no .., no absolute)"},
                    "path": {"type": "string", "description": "base dir (default .)"}
                },
                "required": ["pattern"]
            }),
            prompt_snippet: Some("glob(pattern, path?): list matching files".into()),
        }
    }
    async fn execute(&self, arguments: serde_json::Value) -> anyhow::Result<ToolOutput> {
        let pattern = arguments
            .get("pattern")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if pattern.contains("..") {
            return Ok(ToolOutput::err("glob pattern must not contain '..'"));
        }
        if std::path::Path::new(pattern).is_absolute() {
            return Ok(ToolOutput::err("glob pattern must be relative"));
        }
        let base = arguments
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(".");
        let full = std::path::Path::new(base).join(pattern);
        let mut hits: Vec<String> = vec![];
        match glob::glob(&full.to_string_lossy()) {
            Ok(paths) => {
                for p in paths.flatten() {
                    hits.push(p.to_string_lossy().into_owned());
                    if hits.len() >= 200 {
                        break;
                    }
                }
            }
            Err(e) => return Ok(ToolOutput::err(format!("bad glob pattern: {e}"))),
        }
        hits.sort();
        Ok(ToolOutput::ok(hits.join("\n")))
    }
}

/// 正则搜文件内容（`path` 文件或目录，目录递归；跳隐藏文件与二进制/超大文件，结果按行 capped）。
pub struct GrepTool;
#[async_trait]
impl Tool for GrepTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "grep".into(),
            description: "Search file contents by regex (path file or dir; capped at 50 hits)"
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "regex"},
                    "path": {"type": "string", "description": "file or dir (default .)"},
                    "include": {"type": "string", "description": "optional glob filter like *.rs"},
                    "max_results": {"type": "integer", "description": "default 50, max 200"}
                },
                "required": ["pattern"]
            }),
            prompt_snippet: Some("grep(pattern, path?, include?): regex search contents".into()),
        }
    }
    async fn execute(&self, arguments: serde_json::Value) -> anyhow::Result<ToolOutput> {
        let pat = arguments
            .get("pattern")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let re = match regex::Regex::new(pat) {
            Ok(re) => re,
            Err(e) => return Ok(ToolOutput::err(format!("bad regex: {e}"))),
        };
        let base = arguments
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(".");
        let include = arguments.get("include").and_then(|v| v.as_str());
        let max_results = arguments
            .get("max_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(50)
            .clamp(1, 200) as usize;
        let include_re = match include {
            Some(g) => match glob::Pattern::new(g) {
                Ok(p) => Some(p),
                Err(e) => return Ok(ToolOutput::err(format!("bad include glob: {e}"))),
            },
            None => None,
        };
        let base_path = std::path::Path::new(base);
        // 单文件直接读；目录 walk（跳隐藏、跳 >2MB、最多看 2000 文件防爆）
        let mut files: Vec<std::path::PathBuf> = vec![];
        if base_path.is_file() {
            files.push(base_path.to_path_buf());
        } else if base_path.is_dir() {
            for e in walkdir::WalkDir::new(base_path)
                .follow_links(false)
                .into_iter()
                .flatten()
            {
                let p = e.path();
                if !p.is_file() {
                    continue;
                }
                if p.file_name()
                    .map(|n| n.to_string_lossy().starts_with('.'))
                    .unwrap_or(true)
                {
                    continue;
                }
                if p.metadata().map(|m| m.len() > 2 << 20).unwrap_or(true) {
                    continue;
                }
                files.push(p.to_path_buf());
                if files.len() >= 2000 {
                    break;
                }
            }
        } else {
            return Ok(ToolOutput::err(format!("grep path not found: {base}")));
        }
        let mut hits: Vec<String> = vec![];
        let mut truncated = 0usize;
        'files: for f in &files {
            if let Some(inc) = &include_re {
                if !inc.matches_path(f) {
                    continue;
                }
            }
            // 非 UTF-8（二进制）直接跳过
            let body = match std::fs::read_to_string(f) {
                Ok(b) => b,
                Err(_) => continue,
            };
            for (i, line) in body.lines().enumerate() {
                if re.is_match(line) {
                    if hits.len() >= max_results {
                        truncated += 1;
                        continue;
                    }
                    let mut l: String = line.chars().take(500).collect();
                    if line.chars().count() > 500 {
                        l.push('…');
                    }
                    hits.push(format!("{}:{}:{l}", f.to_string_lossy(), i + 1));
                }
            }
            if truncated > 0 && hits.len() >= max_results {
                break 'files;
            }
        }
        let mut out = hits.join("\n");
        if truncated > 0 {
            out.push_str(&format!("\n...[truncated {truncated} more matches]"));
        }
        Ok(ToolOutput::ok(out))
    }
}

/// 思考通道：模型写下扩展推理，无执行副作用，回固定确认（正文已在 ToolCall 历史里）。
pub struct ThinkTool;
#[async_trait]
impl Tool for ThinkTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "think".into(),
            description: "Record extended reasoning (no side effects)".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "thought": {"type": "string"}
                },
                "required": ["thought"]
            }),
            prompt_snippet: Some("think(thought): reason out loud before acting".into()),
        }
    }
    async fn execute(&self, arguments: serde_json::Value) -> anyhow::Result<ToolOutput> {
        let thought = arguments
            .get("thought")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if thought.is_empty() {
            return Ok(ToolOutput::err("think requires non-empty thought"));
        }
        Ok(ToolOutput::ok("noted."))
    }
}

/// 工具回包有界：超限保留首尾、中部折叠并标注截掉字符数，保上下文窗口不被大输出撑爆。
pub const MAX_TOOL_OUTPUT: usize = 12_000;

/// 会话环境变量名：对标上游 `PI_SESSION_ID`。bash 子进程自动继承父进程环境，
/// 前端（REPL/TUI）在会话建立后调用 [`export_session_id`] 导出一次，脚本里 `$RUPI_SESSION_ID` 即用。
pub const SESSION_ENV_VAR: &str = "RUPI_SESSION_ID";

/// 导出当前会话 id 到进程环境（子进程继承；resume 沿用 db 会话 id，跨进程稳定）。
pub fn export_session_id(id: &str) {
    std::env::set_var(SESSION_ENV_VAR, id);
}

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
        assert_eq!(r.definitions().len(), 7);
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
    async fn bash_cancel_kills_child_and_returns_fast() {
        // 真抢占：sleep 30 跑 300ms 后置位，调用须秒级返回取消错误，而非等 30s。
        use rupi_core::CancelFlag;
        let tool = BashTool;
        let cancel = CancelFlag::new();
        let killer = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            killer.cancel();
        });
        let start = std::time::Instant::now();
        let out = tool
            .execute_with_cancel(
                serde_json::json!({"command": "sleep 30", "timeout_secs": 60}),
                &cancel,
            )
            .await
            .unwrap();
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
        assert!(out.is_error);
        assert!(out.content.contains("cancelled by user"), "{}", out.content);
        // 不置位时走正常路径（无取消回归）。
        let ok = tool
            .execute_with_cancel(
                serde_json::json!({"command": "echo hi"}),
                &CancelFlag::new(),
            )
            .await
            .unwrap();
        assert!(!ok.is_error);
        assert!(ok.content.contains("hi"));
    }

    #[tokio::test]
    async fn bash_cancel_kills_process_group_no_orphans() {
        // 进程组语义：sh -c fork 出的孙进程随取消一起死，不留孤儿。
        // 用独特时长做 pgrep 探针；无 pgrep 的环境跳过断言（不断门）。
        use rupi_core::CancelFlag;
        if tokio::process::Command::new("which")
            .arg("pgrep")
            .output()
            .await
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            return;
        }
        let tool = BashTool;
        let probe = "sleep 47";
        // 先确认探针干净（防其他测试残留干扰）。
        let pre = tokio::process::Command::new("sh")
            .arg("-c")
            .arg("pgrep -f '[s]leep 47' || true")
            .output()
            .await
            .unwrap();
        assert!(pre.stdout.is_empty(), "probe polluted: {pre:?}");
        let cancel = CancelFlag::new();
        let killer = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            killer.cancel();
        });
        let out = tool
            .execute_with_cancel(
                serde_json::json!({"command": probe, "timeout_secs": 60}),
                &cancel,
            )
            .await
            .unwrap();
        assert!(out.content.contains("cancelled by user"), "{}", out.content);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let post = tokio::process::Command::new("sh")
            .arg("-c")
            .arg("pgrep -f '[s]leep 47' || true")
            .output()
            .await
            .unwrap();
        assert!(
            post.stdout.is_empty(),
            "orphan sleep survived cancel: {}",
            String::from_utf8_lossy(&post.stdout)
        );
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

    #[tokio::test]
    async fn bash_sees_exported_session_id() {
        export_session_id("sess-hook-test");
        assert_eq!(std::env::var(SESSION_ENV_VAR).unwrap(), "sess-hook-test");
        // 子进程继承父进程环境：脚本直接可用
        let r = ToolRegistry::with_builtins();
        let out = r
            .execute(
                "bash",
                serde_json::json!({"command": "printf %s \"$RUPI_SESSION_ID\""}),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(out.content, "sess-hook-test");
    }

    #[tokio::test]
    async fn glob_finds_by_pattern_and_rejects_escapes() {
        let dir = std::env::temp_dir().join(format!("rupi-glob-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.rs"), "x").unwrap();
        std::fs::write(dir.join("b.txt"), "x").unwrap();
        std::fs::write(dir.join("sub/c.rs"), "x").unwrap();
        let r = ToolRegistry::with_builtins();
        let base = dir.to_string_lossy().to_string();
        let out = r
            .execute(
                "glob",
                serde_json::json!({"pattern": "**/*.rs", "path": base}),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("a.rs"));
        assert!(out.content.contains("c.rs"));
        assert!(!out.content.contains("b.txt"));
        // `..` 与绝对 pattern 拒绝
        let esc = r
            .execute("glob", serde_json::json!({"pattern": "../x", "path": base}))
            .await
            .unwrap();
        assert!(esc.is_error);
        let abs = r
            .execute(
                "glob",
                serde_json::json!({"pattern": "/tmp/x", "path": base}),
            )
            .await
            .unwrap();
        assert!(abs.is_error);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn grep_finds_regex_skips_binary_and_bad_pattern() {
        let dir = std::env::temp_dir().join(format!("rupi-grep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "hello\nneedle here\nbye\n").unwrap();
        std::fs::write(dir.join("bin.dat"), [0u8, 159, 0, 1]).unwrap();
        let r = ToolRegistry::with_builtins();
        let base = dir.to_string_lossy().to_string();
        let out = r
            .execute(
                "grep",
                serde_json::json!({"pattern": "needle", "path": base}),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("a.txt:2:needle here"));
        assert!(!out.content.contains("bin.dat"));
        // 非法正则判错不炸
        let bad = r
            .execute(
                "grep",
                serde_json::json!({"pattern": "[unclosed", "path": base}),
            )
            .await
            .unwrap();
        assert!(bad.is_error);
        assert!(bad.content.contains("bad regex"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn think_notes_and_requires_content() {
        let r = ToolRegistry::with_builtins();
        let ok = r
            .execute(
                "think",
                serde_json::json!({"thought": "consider edge cases first"}),
            )
            .await
            .unwrap();
        assert!(!ok.is_error);
        assert_eq!(ok.content, "noted.");
        let empty = r
            .execute("think", serde_json::json!({"thought": ""}))
            .await
            .unwrap();
        assert!(empty.is_error);
    }

    #[tokio::test]
    async fn sandboxed_glob_and_grep_stay_inside_root() {
        let root = std::env::temp_dir().join(format!("rupi-gg-sbx-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("in.rs"), "needle\n").unwrap();
        let r = ToolRegistry::with_sandboxed_builtins(&root);
        // 相对 path 解析到 root 内，正常命中
        let out = r
            .execute("glob", serde_json::json!({"pattern": "*.rs", "path": "."}))
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("in.rs"));
        let g = r
            .execute(
                "grep",
                serde_json::json!({"pattern": "needle", "path": "."}),
            )
            .await
            .unwrap();
        assert!(!g.is_error, "{}", g.content);
        assert!(g.content.contains("in.rs"));
        // root 外绝对路径拒绝
        let esc = r
            .execute(
                "glob",
                serde_json::json!({"pattern": "*.rs", "path": "/tmp"}),
            )
            .await
            .unwrap();
        assert!(esc.is_error);
        assert!(esc.content.contains("escapes workspace root"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn mutation_queue_serializes_same_key() {
        // 同 key 临界区互斥：8 任务各睡 20ms，最大并发恒为 1（任意 runtime 下确定成立）。
        use std::sync::atomic::{AtomicUsize, Ordering};
        let q = std::sync::Arc::new(FileMutationQueue::new());
        let key = std::path::PathBuf::from("same-key");
        let cur = std::sync::Arc::new(AtomicUsize::new(0));
        let max = std::sync::Arc::new(AtomicUsize::new(0));
        let mut handles = vec![];
        for _ in 0..8 {
            let (qq, k, c, m) = (q.clone(), key.clone(), cur.clone(), max.clone());
            handles.push(tokio::spawn(async move {
                qq.with_queued(k, || async {
                    let n = c.fetch_add(1, Ordering::SeqCst) + 1;
                    m.fetch_max(n, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    c.fetch_sub(1, Ordering::SeqCst);
                    n
                })
                .await
            }));
        }
        let mut sum = 0;
        for h in handles {
            sum += h.await.unwrap();
        }
        assert_eq!(sum, 8, "8 个临界区应全部串行执行完毕（每次进入时并发为 1）");
        assert_eq!(
            max.load(Ordering::SeqCst),
            1,
            "同 key 必须串行，最大并发只能是 1"
        );
    }

    #[tokio::test]
    async fn mutation_queue_keys_are_independent() {
        // 不同 key 互不阻塞：结果各自正确；排空后槽位回收（复用同一 key 仍正常）。
        let q = std::sync::Arc::new(FileMutationQueue::new());
        let mut handles = vec![];
        for i in 0..8 {
            let qq = q.clone();
            handles.push(tokio::spawn(async move {
                qq.with_queued(std::path::PathBuf::from(format!("key-{i}")), || async move {
                    i * 10
                })
                .await
            }));
        }
        let mut got = vec![];
        for h in handles {
            got.push(h.await.unwrap());
        }
        got.sort();
        assert_eq!(got, vec![0, 10, 20, 30, 40, 50, 60, 70]);
        assert!(q.inner.lock().unwrap().is_empty(), "排空后槽位应回收");
    }

    #[tokio::test]
    async fn concurrent_edits_to_same_file_all_survive() {
        // 端到端：20 个并发 edit 各改独立锚点，串行化后应全数落盘、无一丢失。
        //（无队列时 read-modify-write 竞态几乎必丢更新；有队列则确定通过。）
        let dir = std::env::temp_dir().join(format!("rupi-mq-{}-{}", std::process::id(), line!()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("slots.txt");
        let body: String = (0..20).map(|i| format!("slot-{i}:0")).collect::<Vec<_>>().join("\n");
        std::fs::write(&path, &body).unwrap();
        let path_s = path.to_string_lossy().to_string();
        let r = std::sync::Arc::new(ToolRegistry::with_builtins());
        let mut handles = vec![];
        for i in 0..20 {
            let (rr, p) = (r.clone(), path_s.clone());
            handles.push(tokio::spawn(async move {
                rr.execute(
                    "edit",
                    serde_json::json!({
                        "path": p,
                        "old_string": format!("slot-{i}:0"),
                        "new_string": format!("slot-{i}:1"),
                    }),
                )
                .await
                .unwrap()
            }));
        }
        for h in handles {
            let out = h.await.unwrap();
            assert!(!out.is_error, "{}", out.content);
        }
        let final_body = std::fs::read_to_string(&path).unwrap();
        for i in 0..20 {
            assert!(final_body.contains(&format!("slot-{i}:1")), "slot-{i} 更新丢失");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
