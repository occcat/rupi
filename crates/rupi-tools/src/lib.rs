//! rupi-tools: Tool trait + Pi 默认七件套 Read / Write / Edit / Bash / Glob / Grep / Think + 注册表。

use async_trait::async_trait;
use ignore::{WalkBuilder, WalkState};
use rupi_core::{CancelFlag, ToolDefinition};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

mod truncate;
mod utf8;
pub use truncate::{
    format_size, truncate_head, truncate_tail, TruncationResult, DEFAULT_MAX_BYTES,
    DEFAULT_MAX_LINES,
};
pub use utf8::{decode_utf8, Utf8Decoder};

/// 工具回包里的图片（read/@file 读到 png/jpg 等）：主循环写入 `ContentBlock::Image`。
#[derive(Debug, Clone)]
pub struct ToolImage {
    pub media_type: String,
    pub data: String,
}

/// 扩展/工具给出的 UI 提示（对标 Pi extension UI hints）。
#[derive(Debug, Clone)]
pub struct UiHint {
    pub kind: String,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    pub images: Vec<ToolImage>,
    pub ui_hint: Option<UiHint>,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            images: Vec::new(),
            ui_hint: None,
        }
    }
    pub fn err(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            images: Vec::new(),
            ui_hint: None,
        }
    }

    pub fn with_images(mut self, images: Vec<ToolImage>) -> Self {
        self.images = images;
        self
    }

    pub fn with_ui_hint(mut self, hint: UiHint) -> Self {
        self.ui_hint = Some(hint);
        self
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

    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// `--tools` / `--exclude-tools` 过滤（不改工具实现，只改注册表可见集）。
    pub fn retain<F: Fn(&str) -> bool>(&mut self, f: F) {
        self.tools.retain(|k, _| f(k));
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
/// `..` 与符号链接后必须仍在 `root` 内，否则拒绝执行。悬空符号链接跟随目标
/// （不能把链接名当成「新建文件」而放行）。缺 `path` 参数透传给内层判错。
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
        resolve_sandbox_path(&joined, 0)
    }
}

/// 消 `.` / `..`，不碰磁盘。悬空符号链接的目标靠这个落到真实意图路径，
/// 再和 root 做前缀比较——不能把「链接名本身」当成新建文件。
fn normalize_path(path: &std::path::Path) -> std::path::PathBuf {
    use std::path::{Component, PathBuf};
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Prefix(_) | Component::RootDir => out.push(c),
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    let _ = out.pop();
                }
                Some(Component::RootDir) | Some(Component::Prefix(_)) => {}
                _ => {
                    if !out.has_root() {
                        out.push(c);
                    }
                }
            },
            Component::Normal(_) => out.push(c),
        }
    }
    out
}

/// 跟随符号链接（含悬空）直到真实目标或「父目录 + 新文件名」。
/// 深度封顶防环；canonicalize 只用于已存在的非链接节点。
fn resolve_sandbox_path(path: &std::path::Path, depth: u8) -> Option<std::path::PathBuf> {
    const SYMLINK_MAX: u8 = 32;
    if depth > SYMLINK_MAX {
        return None;
    }
    let path = normalize_path(path);
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            let target = std::fs::read_link(&path).ok()?;
            let joined = if target.is_absolute() {
                target
            } else {
                path.parent()
                    .unwrap_or_else(|| std::path::Path::new(""))
                    .join(target)
            };
            resolve_sandbox_path(&joined, depth + 1)
        }
        Ok(_) => path.canonicalize().ok(),
        Err(_) => {
            let parent = path.parent().filter(|p| !p.as_os_str().is_empty())?;
            let name = path.file_name()?;
            let parent_resolved = resolve_sandbox_path(parent, depth + 1)?;
            Some(parent_resolved.join(name))
        }
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
        Ok(read_path_paged(path, offset, limit).await)
    }
}

/// 先 `metadata` 再读：图片整文件（有上限）；文本按行跳过 offset、只收 limit，
/// 50MB 文件不再 `read_to_string` 进内存。小文件（≤2MB）会扫剩余行数以报 total。
pub const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;
/// 超过此大小不再为了 “of N lines” 扫完全文，只报字节数。
const READ_COUNT_REMAINING_MAX: u64 = 2 * 1024 * 1024;

async fn read_path_paged(path: &str, offset: usize, limit: usize) -> ToolOutput {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};
    if path.is_empty() {
        return ToolOutput::err("read requires path");
    }
    let meta = match tokio::fs::metadata(path).await {
        Ok(m) => m,
        Err(e) => return ToolOutput::err(format!("read {path} failed: {e}")),
    };
    if !meta.is_file() {
        return ToolOutput::err(format!("read {path} failed: not a file"));
    }
    let file_len = meta.len();
    if let Some(media) = rupi_core::image_media_type(std::path::Path::new(path)) {
        if file_len > MAX_IMAGE_BYTES {
            return ToolOutput::err(format!(
                "read {path}: image {media} is {file_len} bytes (max {MAX_IMAGE_BYTES})"
            ));
        }
        let bytes = match tokio::fs::read(path).await {
            Ok(b) => b,
            Err(e) => return ToolOutput::err(format!("read {path} failed: {e}")),
        };
        let caption = format!("[image {media} · {file_len} bytes · attached to this tool result]");
        return ToolOutput::ok(caption).with_images(vec![ToolImage {
            media_type: media.to_string(),
            data: rupi_core::encode_base64(&bytes),
        }]);
    }
    let file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(e) => return ToolOutput::err(format!("read {path} failed: {e}")),
    };
    let mut reader = tokio::io::BufReader::new(file);
    let mut skipped = 0usize;
    while skipped < offset {
        let mut line = String::new();
        match reader.read_line(&mut line).await {
            Ok(0) => {
                return ToolOutput::ok(format!(
                    "[read {path}: offset {offset} past end ({skipped} lines)]"
                ));
            }
            Ok(_) => skipped += 1,
            Err(e) => return ToolOutput::err(format!("read {path} failed: {e}")),
        }
    }
    let mut page: Vec<String> = Vec::new();
    let mut page_bytes = 0usize;
    let mut hit_eof = false;
    for _ in 0..limit {
        let mut line = String::new();
        match reader.read_line(&mut line).await {
            Ok(0) => {
                hit_eof = true;
                break;
            }
            Ok(_) => {
                if line.ends_with('\n') {
                    line.pop();
                    if line.ends_with('\r') {
                        line.pop();
                    }
                }
                page_bytes += line.len();
                page.push(line);
                if page_bytes >= MAX_TOOL_OUTPUT {
                    break;
                }
            }
            Err(e) => return ToolOutput::err(format!("read {path} failed: {e}")),
        }
    }
    if page.is_empty() && hit_eof {
        return ToolOutput::ok(format!(
            "[read {path}: offset {offset} past end ({offset} lines)]"
        ));
    }
    let start = offset + 1;
    let end = offset + page.len();
    let mut more = false;
    let mut total: Option<usize> = None;
    if !hit_eof {
        if file_len <= READ_COUNT_REMAINING_MAX {
            let mut rem = 0usize;
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => rem += 1,
                    Err(e) => return ToolOutput::err(format!("read {path} failed: {e}")),
                }
            }
            more = rem > 0;
            total = Some(end + rem);
        } else {
            let mut peek = [0u8; 1];
            match reader.read(&mut peek).await {
                Ok(n) if n > 0 => more = true,
                _ => more = false,
            }
        }
    }
    let mut s = page.join("\n");
    if more {
        match total {
            Some(n) => s.push_str(&format!(
                "\n…[truncated: lines {start}-{end} of {n} — pass offset={end} for more]"
            )),
            None => s.push_str(&format!(
                "\n…[truncated: lines {start}-{end} · {file_len} bytes — pass offset={end} for more]"
            )),
        }
    } else if offset > 0 {
        s.push_str(&format!("\n[end of file: {} lines]", total.unwrap_or(end)));
    }
    ToolOutput::ok(truncate_middle(&s, MAX_TOOL_OUTPUT))
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
                    Ok(()) => ToolOutput::ok(format!("wrote {path} ({} bytes)", content.len())),
                    Err(e) => ToolOutput::err(format!("write failed: {e}")),
                })
            })
            .await
    }
}

pub struct EditTool;

/// 一条替换（`old` 必须非空，且按原文唯一——`replace_all` 例外）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditSpec {
    pub old: String,
    pub new: String,
}

/// 解析 edit 参数：单条 `old_string`/`new_string`（+`replace_all`），或多条 `edits[]`
/// （每项 `old_string`/`new_string`，兼容 Pi 的 `oldText`/`newText`）。两者可同时给。
pub fn parse_edit_args(arguments: &serde_json::Value) -> Result<(Vec<EditSpec>, bool), String> {
    let get = |v: &serde_json::Value, a: &str, b: &str| -> Option<String> {
        v.get(a)
            .or_else(|| v.get(b))
            .and_then(|x| x.as_str())
            .map(str::to_owned)
    };
    let mut edits = vec![];
    if let Some(list) = arguments.get("edits") {
        // 部分模型把数组当字符串发；容错解析一层
        let items: Vec<serde_json::Value> = match list {
            serde_json::Value::Array(a) => a.clone(),
            serde_json::Value::String(s) => match serde_json::from_str::<serde_json::Value>(s) {
                Ok(serde_json::Value::Array(a)) => a,
                Ok(v) => vec![v],
                Err(e) => return Err(format!("edits is not valid JSON: {e}")),
            },
            serde_json::Value::Null => vec![],
            other => vec![other.clone()],
        };
        for (i, item) in items.iter().enumerate() {
            let (Some(old), Some(new)) = (
                get(item, "old_string", "oldText"),
                get(item, "new_string", "newText"),
            ) else {
                return Err(format!(
                    "edits[{i}] must have old_string and new_string (or oldText/newText)"
                ));
            };
            edits.push(EditSpec { old, new });
        }
    }
    if let (Some(old), Some(new)) = (
        get(arguments, "old_string", "oldText"),
        get(arguments, "new_string", "newText"),
    ) {
        edits.push(EditSpec { old, new });
    }
    let replace_all = arguments
        .get("replace_all")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if edits.is_empty() {
        return Err("edit needs old_string/new_string or a non-empty edits[] list".into());
    }
    if replace_all && edits.len() > 1 {
        return Err("replace_all only applies to a single old_string/new_string edit".into());
    }
    Ok((edits, replace_all))
}

/// 对原文一次性应用全部替换（对标 Pi edit：每条 old 都按**原文**匹配而非逐条累积；
/// 必须命中且唯一，区间不得重叠）。`replace_all=true` 时单条替换全部命中。
/// 此前实现是 `replacen(old, new, 1)`：多处命中静默改第一处、空 old 把新文本插到文件头。
pub fn apply_edits(
    original: &str,
    edits: &[EditSpec],
    replace_all: bool,
) -> Result<String, String> {
    if replace_all {
        let e = &edits[0];
        if e.old.is_empty() {
            return Err("old_string must not be empty".into());
        }
        let n = original.matches(e.old.as_str()).count();
        if n == 0 {
            return Err(
                "old_string not found in file. It must match exactly, including whitespace.".into(),
            );
        }
        return Ok(original.replace(e.old.as_str(), &e.new));
    }
    struct Span<'a> {
        start: usize,
        end: usize,
        new: &'a str,
    }
    let mut spans: Vec<Span> = Vec::with_capacity(edits.len());
    for (i, e) in edits.iter().enumerate() {
        if e.old.is_empty() {
            return Err(format!("edit[{i}]: old_string must not be empty"));
        }
        let hits: Vec<usize> = original
            .match_indices(e.old.as_str())
            .map(|(p, _)| p)
            .collect();
        match hits.len() {
            0 => {
                return Err(format!(
                    "edit[{i}]: old_string not found in file. It must match exactly, including whitespace."
                ))
            }
            1 => {}
            n => {
                return Err(format!(
                    "edit[{i}]: old_string matched {n} times. It must be unique — add surrounding context, or set replace_all=true to change every occurrence."
                ))
            }
        }
        spans.push(Span {
            start: hits[0],
            end: hits[0] + e.old.len(),
            new: &e.new,
        });
    }
    spans.sort_by_key(|s| s.start);
    for pair in spans.windows(2) {
        if pair[0].end > pair[1].start {
            return Err(
                "edits overlap or are nested. Each old_string is matched against the original file; merge nearby changes into one edit."
                    .into(),
            );
        }
    }
    let mut out = String::with_capacity(original.len());
    let mut cursor = 0usize;
    for s in spans {
        out.push_str(&original[cursor..s.start]);
        out.push_str(s.new);
        cursor = s.end;
    }
    out.push_str(&original[cursor..]);
    Ok(out)
}

/// 统一 diff（3 行上下文），供工具回包与 UI 展示；超长折叠保窗口。
pub fn line_diff(old: &str, new: &str, path: &str) -> String {
    let diff = similar::TextDiff::from_lines(old, new);
    let text = diff
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string();
    truncate_middle(&text, 8_000)
}
#[async_trait]
impl Tool for EditTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "edit".into(),
            description: "Exact string replacement in a file. old_string must match exactly once (add surrounding context to disambiguate, or set replace_all). For several disjoint changes in one file pass edits=[{old_string,new_string},...] — each is matched against the original file and must not overlap. Returns a unified diff.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_string": {"type": "string", "description": "exact text to replace; must be unique in the file"},
                    "new_string": {"type": "string"},
                    "replace_all": {"type": "boolean", "description": "replace every occurrence of old_string (default false)"},
                    "edits": {
                        "type": "array",
                        "description": "multiple disjoint replacements, each matched against the original file",
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_string": {"type": "string"},
                                "new_string": {"type": "string"}
                            },
                            "required": ["old_string", "new_string"]
                        }
                    }
                },
                "required": ["path"]
            }),
            prompt_snippet: Some(
                "edit(path, old_string, new_string | edits[]): exact, unique-match replacement (multi-edit supported)".into(),
            ),
        }
    }
    async fn execute(&self, arguments: serde_json::Value) -> anyhow::Result<ToolOutput> {
        let path = arguments.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let (edits, replace_all) = match parse_edit_args(&arguments) {
            Ok(x) => x,
            Err(e) => return Ok(ToolOutput::err(e)),
        };
        let key = mutation_key(path);
        global_mutation_queue()
            .with_queued(key, || async {
                let content = tokio::fs::read_to_string(path)
                    .await
                    .map_err(|e| anyhow::anyhow!("read failed: {e}"))?;
                let updated = match apply_edits(&content, &edits, replace_all) {
                    Ok(u) => u,
                    Err(e) => return Ok(ToolOutput::err(e)),
                };
                tokio::fs::write(path, &updated).await?;
                let diff = line_diff(&content, &updated, path);
                Ok(ToolOutput::ok(format!(
                    "edited {path} ({} replacement{})\n{diff}",
                    edits.len(),
                    if edits.len() == 1 { "" } else { "s" }
                )))
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
        // 与取消路径共用：超时杀进程组 + kill_on_drop，不丢 `.output()` future 留孤儿。
        self.execute_with_cancel(arguments, &CancelFlag::new())
            .await
    }
    /// 真抢占：取消置位即 kill 子进程并回收，已产出内容随取消错误一并返回。
    ///
    /// stdout/stderr 自 spawn 起由两个 reader task 并发排空（有界 50KB 尾部 ring），
    /// 避免子进程写满 ~64KB 管道缓冲后阻塞、`wait` 永远等不到（P0 死锁）。
    /// 超时/取消同样先保证排空在跑，再杀进程组，最后回收 reader。
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
            .kill_on_drop(true)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => return Ok(ToolOutput::err(format!("spawn failed: {e}"))),
        };
        // 先拿走管道再 wait：reader 与 wait/取消/超时并发，写满缓冲也不会堵死子进程。
        let out_task = tokio::spawn(Self::read_pipe_tail(child.stdout.take(), DEFAULT_MAX_BYTES));
        let err_task = tokio::spawn(Self::read_pipe_tail(child.stderr.take(), DEFAULT_MAX_BYTES));
        let timeout = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs));
        tokio::pin!(timeout);
        enum End {
            Done(std::process::ExitStatus),
            Cancelled,
            TimedOut,
            WaitFailed(String),
        }
        let end = tokio::select! {
            _ = cancel.cancelled() => End::Cancelled,
            _ = &mut timeout => End::TimedOut,
            res = child.wait() => match res {
                Ok(status) => End::Done(status),
                Err(e) => End::WaitFailed(e.to_string()),
            },
        };
        match end {
            End::Done(status) => {
                let (out_text, err_text) = Self::join_pipe_readers(out_task, err_task, None).await;
                let mut s = out_text;
                if !err_text.is_empty() {
                    s.push_str(&format!("\n[stderr]\n{err_text}"));
                }
                s = bound_bash_output(&s);
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
                let (out_text, _) = Self::join_pipe_readers(
                    out_task,
                    err_task,
                    Some(std::time::Duration::from_millis(500)),
                )
                .await;
                let partial = truncate_middle(&out_text, MAX_TOOL_OUTPUT);
                let mut msg = String::from("cancelled by user");
                if !partial.is_empty() {
                    msg.push_str(&format!("\n[partial output]\n{partial}"));
                }
                Ok(ToolOutput::err(msg))
            }
            End::TimedOut => {
                Self::kill_tree(&mut child).await;
                // 杀后再回收 reader：排空已入 ring 的尾部，避免管道/任务泄漏。
                let _ = Self::join_pipe_readers(
                    out_task,
                    err_task,
                    Some(std::time::Duration::from_millis(500)),
                )
                .await;
                Ok(ToolOutput::err(format!(
                    "command timed out after {timeout_secs}s"
                )))
            }
            End::WaitFailed(e) => {
                Self::kill_tree(&mut child).await;
                let _ = Self::join_pipe_readers(
                    out_task,
                    err_task,
                    Some(std::time::Duration::from_millis(500)),
                )
                .await;
                Ok(ToolOutput::err(format!("wait failed: {e}")))
            }
        }
    }
}

impl BashTool {
    fn parse_args(arguments: &serde_json::Value) -> (&str, u64) {
        let command = arguments
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

    /// 并发排空一条管道，只保留最后 `max_bytes`（对标 bash 50KB 尾部）。
    async fn read_pipe_tail<R: tokio::io::AsyncRead + Unpin + Send>(
        reader: Option<R>,
        max_bytes: usize,
    ) -> PipeTail {
        use tokio::io::AsyncReadExt as _;
        let mut tail = PipeTail::new(max_bytes);
        let Some(mut r) = reader else {
            return tail;
        };
        let mut chunk = [0u8; 8192];
        loop {
            match r.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => tail.push(&chunk[..n]),
            }
        }
        tail
    }

    /// 回收两个 reader。`limit` 用于取消/超时：孙进程占着写端时不能无限等。
    async fn join_pipe_readers(
        out_task: tokio::task::JoinHandle<PipeTail>,
        err_task: tokio::task::JoinHandle<PipeTail>,
        limit: Option<std::time::Duration>,
    ) -> (String, String) {
        let join = async {
            let (o, e) = tokio::join!(out_task, err_task);
            (
                o.unwrap_or_else(|_| PipeTail::new(0)).into_string(),
                e.unwrap_or_else(|_| PipeTail::new(0)).into_string(),
            )
        };
        match limit {
            None => join.await,
            Some(d) => tokio::time::timeout(d, join).await.unwrap_or_default(),
        }
    }
}

/// 有界尾部 ring：只留最后 `max` 字节，避免排空管道时把整段输出读进内存。
struct PipeTail {
    buf: VecDeque<u8>,
    max: usize,
}

impl PipeTail {
    fn new(max: usize) -> Self {
        Self {
            buf: VecDeque::new(),
            max,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        if self.max == 0 {
            return;
        }
        if chunk.len() >= self.max {
            self.buf.clear();
            self.buf
                .extend(chunk[chunk.len() - self.max..].iter().copied());
            return;
        }
        let next = self.buf.len() + chunk.len();
        if next > self.max {
            self.buf.drain(..next - self.max);
        }
        self.buf.extend(chunk.iter().copied());
    }

    fn into_string(self) -> String {
        let v: Vec<u8> = self.buf.into_iter().collect();
        decode_utf8(&v)
    }
}

/// 按 glob 列文件（`**/*.rs`）。`path` 为基准目录（沙箱改写到 root 内），
/// `pattern` 禁 `..` 与绝对路径：两者配合结果恒在基准目录下。
/// 遍历走 `ignore::WalkBuilder`（并行、遵守 .gitignore），阻塞 I/O 在 `spawn_blocking`。
pub struct GlobTool;
#[async_trait]
impl Tool for GlobTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "glob".into(),
            description:
                "List files matching a glob pattern under path (respects .gitignore; capped at 200 results)"
                    .into(),
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
        if Path::new(pattern).is_absolute() {
            return Ok(ToolOutput::err("glob pattern must be relative"));
        }
        if let Err(e) = glob::Pattern::new(pattern) {
            return Ok(ToolOutput::err(format!("bad glob pattern: {e}")));
        }
        let base = arguments
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(".")
            .to_owned();
        let pattern = pattern.to_owned();
        Ok(spawn_blocking_tool(move || glob_blocking(base, pattern)).await)
    }
}

/// 正则搜文件内容（`path` 文件或目录，目录递归；跳隐藏/二进制/超大文件，结果按行 capped）。
/// 目录遍历走 `ignore::WalkBuilder`（并行、遵守 .gitignore），不再用 2000 文件硬上限；
/// 读盘与 walk 全部在 `spawn_blocking`，避免卡住 tokio 运行时。
pub struct GrepTool;
#[async_trait]
impl Tool for GrepTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "grep".into(),
            description:
                "Search file contents by regex (path file or dir; respects .gitignore; capped at 50 hits)"
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
            .unwrap_or(".")
            .to_owned();
        let include = arguments
            .get("include")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        if let Some(g) = &include {
            if let Err(e) = glob::Pattern::new(g) {
                return Ok(ToolOutput::err(format!("bad include glob: {e}")));
            }
        }
        let max_results = arguments
            .get("max_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(50)
            .clamp(1, 200) as usize;
        Ok(spawn_blocking_tool(move || grep_blocking(re, base, include, max_results)).await)
    }
}

const GLOB_MAX_HITS: usize = 200;
const GREP_MAX_FILE_BYTES: u64 = 2 << 20;

async fn spawn_blocking_tool(f: impl FnOnce() -> ToolOutput + Send + 'static) -> ToolOutput {
    tokio::task::spawn_blocking(f)
        .await
        .unwrap_or_else(|e| ToolOutput::err(format!("search interrupted: {e}")))
}

/// 标准过滤器：隐藏文件、`.gitignore` / `.ignore`、全局 exclude。`require_git` 保持默认
/// true，只在 git 仓库内应用 gitignore（测试夹具建空 `.git` 即可隔离父目录规则）。
fn ignore_walker(root: impl AsRef<Path>) -> WalkBuilder {
    let mut b = WalkBuilder::new(root);
    b.standard_filters(true).follow_links(false);
    b
}

/// 相对路径匹配：`require_literal_separator` 让 `*` 不跨目录（`*.rs` ≠ `sub/c.rs`），
/// 同时 `**/*.rs` 仍命中根下 `a.rs`（与 `glob::glob("base/**/*.rs")` 一致）。
fn glob_rel_matches(pattern: &str, rel: &Path) -> bool {
    let Ok(p) = glob::Pattern::new(pattern) else {
        return false;
    };
    p.matches_path_with(
        rel,
        glob::MatchOptions {
            require_literal_separator: true,
            ..glob::MatchOptions::new()
        },
    )
}

/// `include: *.rs` 按文件名命中任意深度（对标 ripgrep `--glob`）；带目录的 pattern 走相对路径。
fn include_matches(pattern: &str, path: &Path, root: &Path) -> bool {
    let rel = path.strip_prefix(root).unwrap_or(path);
    if glob_rel_matches(pattern, rel) {
        return true;
    }
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| glob::Pattern::new(pattern).is_ok_and(|p| p.matches(n)))
}

fn glob_blocking(base: String, pattern: String) -> ToolOutput {
    let root = PathBuf::from(base);
    let hits = Mutex::new(Vec::new());
    ignore_walker(&root).build_parallel().run(|| {
        let hits = &hits;
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
            if g.len() >= GLOB_MAX_HITS {
                return WalkState::Quit;
            }
            g.push(path.to_string_lossy().into_owned());
            WalkState::Continue
        })
    });
    let mut hits = hits.into_inner().unwrap();
    hits.sort();
    hits.truncate(GLOB_MAX_HITS);
    ToolOutput::ok(hits.join("\n"))
}

fn format_grep_hit(path: &Path, line_idx: usize, line: &str) -> String {
    let mut l: String = line.chars().take(500).collect();
    if line.chars().count() > 500 {
        l.push('…');
    }
    format!("{}:{}:{l}", path.to_string_lossy(), line_idx + 1)
}

fn push_line_hits(
    re: &regex::Regex,
    path: &Path,
    body: &str,
    hits: &Mutex<Vec<String>>,
    extra: &AtomicUsize,
    max_results: usize,
) {
    let mut local = Vec::new();
    for (i, line) in body.lines().enumerate() {
        if re.is_match(line) {
            local.push(format_grep_hit(path, i, line));
        }
    }
    if local.is_empty() {
        return;
    }
    let mut g = hits.lock().unwrap();
    for h in local {
        if g.len() >= max_results {
            extra.fetch_add(1, Ordering::Relaxed);
        } else {
            g.push(h);
        }
    }
}

fn finish_grep_hits(mut hits: Vec<String>, truncated: usize) -> ToolOutput {
    hits.sort();
    let mut out = hits.join("\n");
    if truncated > 0 {
        out.push_str(&format!("\n...[truncated {truncated} more matches]"));
    }
    ToolOutput::ok(out)
}

fn grep_file(
    re: &regex::Regex,
    path: &Path,
    include: Option<&str>,
    max_results: usize,
) -> ToolOutput {
    if let Some(inc) = include {
        let root = path.parent().unwrap_or(path);
        if !include_matches(inc, path, root) {
            return ToolOutput::ok("");
        }
    }
    // 显式点名的单文件：不套 2MB / gitignore / 隐藏规则，读失败（二进制）当空。
    let body = match std::fs::read_to_string(path) {
        Ok(b) => b,
        Err(_) => return ToolOutput::ok(""),
    };
    let hits = Mutex::new(Vec::new());
    let extra = AtomicUsize::new(0);
    push_line_hits(re, path, &body, &hits, &extra, max_results);
    finish_grep_hits(hits.into_inner().unwrap(), extra.load(Ordering::Relaxed))
}

fn grep_blocking(
    re: regex::Regex,
    base: String,
    include: Option<String>,
    max_results: usize,
) -> ToolOutput {
    let root = PathBuf::from(&base);
    if root.is_file() {
        return grep_file(&re, &root, include.as_deref(), max_results);
    }
    if !root.is_dir() {
        return ToolOutput::err(format!("grep path not found: {base}"));
    }
    let hits = Mutex::new(Vec::new());
    let extra = AtomicUsize::new(0);
    ignore_walker(&root).build_parallel().run(|| {
        let hits = &hits;
        let extra = &extra;
        let re = &re;
        let include = &include;
        let root = &root;
        Box::new(move |res| {
            let Ok(ent) = res else {
                return WalkState::Continue;
            };
            let path = ent.path();
            if !path.is_file() {
                return WalkState::Continue;
            }
            if let Some(inc) = include.as_deref() {
                if !include_matches(inc, path, root) {
                    return WalkState::Continue;
                }
            }
            if path
                .metadata()
                .map(|m| m.len() > GREP_MAX_FILE_BYTES)
                .unwrap_or(true)
            {
                return WalkState::Continue;
            }
            if hits.lock().unwrap().len() >= max_results {
                return WalkState::Quit;
            }
            let body = match std::fs::read_to_string(path) {
                Ok(b) => b,
                Err(_) => return WalkState::Continue,
            };
            push_line_hits(re, path, &body, hits, extra, max_results);
            if hits.lock().unwrap().len() >= max_results {
                WalkState::Quit
            } else {
                WalkState::Continue
            }
        })
    });
    finish_grep_hits(hits.into_inner().unwrap(), extra.load(Ordering::Relaxed))
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

/// bash 输出有界化（对标 Pi coding-agent 默认：保留尾部 2000 行 / 50KB，先到先截）：
/// 命令末尾通常是错误与总结，尾部比头部更有信息量；截断时标注保留比例。
pub fn bound_bash_output(s: &str) -> String {
    let t = truncate_tail(s, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
    if !t.truncated {
        return t.content;
    }
    format!(
        "{}\n\n[truncated: kept last {} / {} lines, {} / {}]",
        t.content,
        t.output_lines,
        t.total_lines,
        format_size(t.output_bytes),
        format_size(t.total_bytes)
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
    async fn read_does_not_slurp_huge_file_and_attaches_images() {
        let dir = std::env::temp_dir().join(format!("rupi-huge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let r = ToolRegistry::with_builtins();
        // 大文件：>2MB 走 peek 而非扫完全文；只取 3 行，回包不含整文件。
        let big = dir.join("huge.txt");
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&big).unwrap();
            for i in 0..80_000 {
                writeln!(f, "row{i}").unwrap();
            }
        }
        let path = big.to_string_lossy().to_string();
        let out = r
            .execute("read", serde_json::json!({"path": path, "limit": 3}))
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.starts_with("row0\nrow1\nrow2"),
            "{}",
            out.content
        );
        assert!(out.content.contains("truncated"), "{}", out.content);
        assert!(!out.content.contains("row79999"));
        // 图片：metadata 后整读，base64 进 images
        let png = dir.join("dot.png");
        std::fs::write(
            &png,
            [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 9, 8, 7],
        )
        .unwrap();
        let img = r
            .execute("read", serde_json::json!({"path": png.to_string_lossy()}))
            .await
            .unwrap();
        assert!(!img.is_error, "{}", img.content);
        assert_eq!(img.images.len(), 1);
        assert_eq!(img.images[0].media_type, "image/png");
        assert!(!img.images[0].data.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn bash_truncates_huge_output_and_honors_timeout() {
        let r = ToolRegistry::with_builtins();
        // 大输出有界（对标 Pi：保留尾部 2000 行 / 50KB）：末尾在、头部被截、总长有界
        let big = r
            .execute("bash", serde_json::json!({"command": "seq 1 200000"}))
            .await
            .unwrap();
        assert!(!big.is_error);
        assert!(big.content.contains("[truncated: kept last "));
        assert!(big.content.len() <= DEFAULT_MAX_BYTES + 256);
        assert!(big.content.contains("\n200000\n"), "tail must be kept");
        assert!(!big.content.starts_with("1\n2\n"), "head must be dropped");
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
    async fn bash_cancel_path_drains_large_pipes_without_deadlock() {
        // P0：execute_with_cancel 若先 wait 再排空，stdout/stderr 超过 ~64KB 管道
        // 缓冲就会死锁到 timeout。seq 1 20000 ≈ 110KB，必须秒级成功并保尾。
        use rupi_core::CancelFlag;
        let tool = BashTool;
        let cancel = CancelFlag::new();
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(8),
            tool.execute_with_cancel(
                serde_json::json!({"command": "seq 1 20000", "timeout_secs": 30}),
                &cancel,
            ),
        )
        .await
        .expect("stdout pipe deadlock: execute_with_cancel hung")
        .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(
            !out.content.contains("timed out"),
            "large stdout must not hit timeout: {}",
            out.content
        );
        assert!(
            out.content.contains("20000"),
            "tail must be kept: {}",
            out.content
        );
        assert!(out.content.len() <= DEFAULT_MAX_BYTES + 256);

        let err = tokio::time::timeout(
            std::time::Duration::from_secs(8),
            tool.execute_with_cancel(
                serde_json::json!({"command": "seq 1 20000 >&2", "timeout_secs": 30}),
                &cancel,
            ),
        )
        .await
        .expect("stderr pipe deadlock: execute_with_cancel hung")
        .unwrap();
        assert!(
            !err.content.contains("timed out"),
            "large stderr must not hit timeout: {}",
            err.content
        );
        assert!(err.content.contains("20000"), "{}", err.content);
        assert!(err.content.len() <= DEFAULT_MAX_BYTES + 256);
    }

    #[tokio::test]
    async fn bash_cancel_path_timeout_still_kills() {
        use rupi_core::CancelFlag;
        let tool = BashTool;
        let start = std::time::Instant::now();
        let out = tool
            .execute_with_cancel(
                serde_json::json!({"command": "sleep 30", "timeout_secs": 1}),
                &CancelFlag::new(),
            )
            .await
            .unwrap();
        assert!(start.elapsed() < std::time::Duration::from_secs(8));
        assert!(out.is_error);
        assert!(
            out.content.contains("timed out after 1s"),
            "{}",
            out.content
        );
    }

    #[test]
    fn pipe_tail_keeps_only_last_bytes() {
        let mut t = PipeTail::new(8);
        t.push(b"abcdefghij");
        assert_eq!(t.into_string(), "cdefghij");
        let mut t = PipeTail::new(8);
        t.push(b"abcd");
        t.push(b"efghij");
        assert_eq!(t.into_string(), "cdefghij");
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
    async fn bash_execute_timeout_kills_process_group_no_orphans() {
        // 非取消路径：超时必须杀进程组，不能只 drop `.output()` 留孙进程。
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
        let probe = "sleep 59";
        let pre = tokio::process::Command::new("sh")
            .arg("-c")
            .arg("pgrep -f '[s]leep 59' || true")
            .output()
            .await
            .unwrap();
        assert!(pre.stdout.is_empty(), "probe polluted: {pre:?}");
        let out = tool
            .execute(serde_json::json!({"command": probe, "timeout_secs": 1}))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(
            out.content.contains("timed out after 1s"),
            "{}",
            out.content
        );
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let post = tokio::process::Command::new("sh")
            .arg("-c")
            .arg("pgrep -f '[s]leep 59' || true")
            .output()
            .await
            .unwrap();
        assert!(
            post.stdout.is_empty(),
            "orphan sleep survived execute() timeout: {}",
            String::from_utf8_lossy(&post.stdout)
        );
    }

    #[tokio::test]
    async fn bash_decodes_utf8_incrementally() {
        let r = ToolRegistry::with_builtins();
        let out = r
            .execute("bash", serde_json::json!({"command": "printf '%s' '你好'"}))
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("你好"), "{}", out.content);
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
        // 悬空符号链接：canonicalize 失败时旧逻辑会把链接名当新建文件放行，
        // write 跟随链接写到沙箱外。目标必须按链接指向判定。
        let outside =
            std::env::temp_dir().join(format!("rupi-sbx-dangle-out-{}", std::process::id()));
        let _ = std::fs::remove_file(&outside);
        symlink(&outside, root.join("sub/dangle")).unwrap();
        let dangle = r
            .execute(
                "write",
                serde_json::json!({"path": "sub/dangle", "content": "pwned"}),
            )
            .await
            .unwrap();
        assert!(dangle.is_error, "{}", dangle.content);
        assert!(dangle.content.contains("escapes workspace root"));
        assert!(
            !outside.exists(),
            "dangling symlink write escaped to {}",
            outside.display()
        );
        // 指向沙箱内的悬空链接：解析到 root 内目标，放行
        symlink("inside-new.txt", root.join("sub/oklink")).unwrap();
        let ok_link = r
            .execute(
                "write",
                serde_json::json!({"path": "sub/oklink", "content": "safe"}),
            )
            .await
            .unwrap();
        assert!(!ok_link.is_error, "{}", ok_link.content);
        assert_eq!(
            std::fs::read_to_string(root.join("sub/inside-new.txt")).unwrap(),
            "safe"
        );
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
        let one_level = r
            .execute("glob", serde_json::json!({"pattern": "*.rs", "path": base}))
            .await
            .unwrap();
        assert!(one_level.content.contains("a.rs"), "{}", one_level.content);
        assert!(
            !one_level.content.contains("c.rs"),
            "*.rs must not match nested files: {}",
            one_level.content
        );
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

    fn unique_tmp(prefix: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn init_git_fixture(dir: &std::path::Path) {
        std::fs::create_dir_all(dir.join(".git")).unwrap();
    }

    #[test]
    fn glob_rel_matches_double_star_and_one_level() {
        assert!(glob_rel_matches("**/*.rs", Path::new("a.rs")));
        assert!(glob_rel_matches("**/*.rs", Path::new("sub/c.rs")));
        assert!(!glob_rel_matches("**/*.rs", Path::new("b.txt")));
        assert!(glob_rel_matches("*.rs", Path::new("in.rs")));
        assert!(!glob_rel_matches("*.rs", Path::new("sub/c.rs")));
        assert!(glob_rel_matches("src/*.rs", Path::new("src/main.rs")));
        assert!(!glob_rel_matches("src/*.rs", Path::new("src/a/b.rs")));
        assert!(include_matches(
            "*.rs",
            Path::new("/tmp/x/src/a.rs"),
            Path::new("/tmp/x")
        ));
    }

    #[tokio::test]
    async fn grep_and_glob_honor_gitignore_and_skip_target() {
        let dir = unique_tmp("rupi-gg-ig");
        init_git_fixture(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("target/debug")).unwrap();
        std::fs::write(dir.join(".gitignore"), "target/\n*.log\n").unwrap();
        std::fs::write(dir.join("src/hit.rs"), "fn unique_source_marker() {}\n").unwrap();
        std::fs::write(
            dir.join("target/debug/hit.rs"),
            "fn unique_source_marker() {}\n",
        )
        .unwrap();
        std::fs::write(dir.join("noise.log"), "unique_source_marker\n").unwrap();

        let r = ToolRegistry::with_builtins();
        let base = dir.to_string_lossy().to_string();
        let g = r
            .execute(
                "grep",
                serde_json::json!({"pattern": "unique_source_marker", "path": base}),
            )
            .await
            .unwrap();
        assert!(!g.is_error, "{}", g.content);
        assert!(
            g.content.contains("src/hit.rs") || g.content.contains("hit.rs"),
            "{}",
            g.content
        );
        assert!(
            !g.content.contains("target"),
            "gitignored target/ must not crowd grep: {}",
            g.content
        );
        assert!(!g.content.contains("noise.log"), "{}", g.content);

        let listing = r
            .execute(
                "glob",
                serde_json::json!({"pattern": "**/*.rs", "path": base}),
            )
            .await
            .unwrap();
        assert!(!listing.is_error, "{}", listing.content);
        assert!(listing.content.contains("hit.rs"), "{}", listing.content);
        assert!(
            !listing.content.contains("target"),
            "gitignored target/ must not crowd glob: {}",
            listing.content
        );

        // 显式点名被 ignore 的文件仍可读（不走目录 walker）
        let explicit = r
            .execute(
                "grep",
                serde_json::json!({
                    "pattern": "unique_source_marker",
                    "path": dir.join("target/debug/hit.rs").to_string_lossy()
                }),
            )
            .await
            .unwrap();
        assert!(
            explicit.content.contains("unique_source_marker"),
            "{}",
            explicit.content
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn grep_finds_source_past_old_2000_file_cap() {
        // 旧实现先收 2000 个文件再搜：lex 靠前的目录能把源码挤掉。
        let dir = unique_tmp("rupi-grep-cap");
        std::fs::create_dir_all(dir.join("aaa")).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        for i in 0..2100 {
            std::fs::write(
                dir.join("aaa").join(format!("f{i:04}.txt")),
                format!("pad-{i}\n"),
            )
            .unwrap();
        }
        std::fs::write(dir.join("src/late.txt"), "needle-beyond-cap\n").unwrap();
        let r = ToolRegistry::with_builtins();
        let base = dir.to_string_lossy().to_string();
        let late = r
            .execute(
                "grep",
                serde_json::json!({"pattern": "needle-beyond-cap", "path": base}),
            )
            .await
            .unwrap();
        assert!(!late.is_error, "{}", late.content);
        assert!(late.content.contains("late.txt"), "{}", late.content);
        let pad = r
            .execute(
                "grep",
                serde_json::json!({"pattern": "pad-2099", "path": base}),
            )
            .await
            .unwrap();
        assert!(pad.content.contains("f2099.txt"), "{}", pad.content);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn grep_include_glob_filters_by_filename() {
        let dir = unique_tmp("rupi-grep-inc");
        std::fs::write(dir.join("a.rs"), "needle in rust\n").unwrap();
        std::fs::write(dir.join("a.txt"), "needle in text\n").unwrap();
        let r = ToolRegistry::with_builtins();
        let base = dir.to_string_lossy().to_string();
        let out = r
            .execute(
                "grep",
                serde_json::json!({"pattern": "needle", "path": base, "include": "*.rs"}),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("a.rs"), "{}", out.content);
        assert!(!out.content.contains("a.txt"), "{}", out.content);
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
                qq.with_queued(
                    std::path::PathBuf::from(format!("key-{i}")),
                    || async move { i * 10 },
                )
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
        let body: String = (0..20)
            .map(|i| format!("slot-{i}:0"))
            .collect::<Vec<_>>()
            .join("\n");
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
            assert!(
                final_body.contains(&format!("slot-{i}:1")),
                "slot-{i} 更新丢失"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod edit_tests {
    use super::*;

    fn spec(old: &str, new: &str) -> EditSpec {
        EditSpec {
            old: old.into(),
            new: new.into(),
        }
    }

    #[test]
    fn rejects_non_unique_and_empty_and_missing() {
        let err = apply_edits("foo foo", &[spec("foo", "bar")], false).unwrap_err();
        assert!(err.contains("matched 2 times"), "{err}");
        let err = apply_edits("foo", &[spec("", "bar")], false).unwrap_err();
        assert!(err.contains("must not be empty"), "{err}");
        let err = apply_edits("foo", &[spec("zzz", "bar")], false).unwrap_err();
        assert!(err.contains("not found"), "{err}");
    }

    #[test]
    fn replace_all_changes_every_occurrence() {
        assert_eq!(
            apply_edits("a-a-a", &[spec("a", "b")], true).unwrap(),
            "b-b-b"
        );
    }

    #[test]
    fn multi_edit_matches_against_original_and_rejects_overlap() {
        let src = "aaa\nbbb\nccc\n";
        let out = apply_edits(src, &[spec("aaa", "AAA"), spec("ccc", "CCC")], false).unwrap();
        assert_eq!(out, "AAA\nbbb\nCCC\n");
        // 后一条的 old 落在前一条区间内 → 重叠拒绝
        let err = apply_edits("abcdef", &[spec("abcd", "X"), spec("cd", "Y")], false).unwrap_err();
        assert!(err.contains("overlap"), "{err}");
    }

    #[test]
    fn parses_single_multi_and_pi_aliases() {
        let (e, all) = parse_edit_args(&serde_json::json!({
            "path": "f", "old_string": "a", "new_string": "b", "replace_all": true
        }))
        .unwrap();
        assert_eq!((e.len(), all), (1, true));
        let (e, _) = parse_edit_args(&serde_json::json!({
            "path": "f", "edits": [{"oldText": "a", "newText": "b"}, {"old_string": "c", "new_string": "d"}]
        }))
        .unwrap();
        assert_eq!(e.len(), 2);
        assert!(parse_edit_args(&serde_json::json!({"path": "f"})).is_err());
        assert!(parse_edit_args(&serde_json::json!({
            "path": "f", "replace_all": true, "edits": [{"old_string":"a","new_string":"b"},{"old_string":"c","new_string":"d"}]
        }))
        .is_err());
    }

    #[tokio::test]
    async fn edit_tool_end_to_end_reports_diff_and_refuses_ambiguity() {
        let dir =
            std::env::temp_dir().join(format!("rupi-edit-{}-{}", std::process::id(), line!()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.txt");
        std::fs::write(&p, "hello world\nhello again\n").unwrap();
        let r = ToolRegistry::with_builtins();
        let path = p.to_string_lossy().to_string();
        let amb = r
            .execute(
                "edit",
                serde_json::json!({"path": path, "old_string": "hello", "new_string": "bye"}),
            )
            .await
            .unwrap();
        assert!(amb.is_error, "{}", amb.content);
        assert!(amb.content.contains("matched 2 times"));
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "hello world\nhello again\n"
        );
        let ok = r
            .execute(
                "edit",
                serde_json::json!({"path": path, "old_string": "hello world", "new_string": "bye world"}),
            )
            .await
            .unwrap();
        assert!(!ok.is_error, "{}", ok.content);
        assert!(ok.content.contains("-hello world") && ok.content.contains("+bye world"));
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "bye world\nhello again\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
