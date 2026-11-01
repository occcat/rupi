//! rupi-tools: Tool trait + Pi 默认七件套 Read / Write / Edit / Bash / Glob / Grep / Think + 注册表。
//! 可选 `find` / `ls` 默认不注册，由 `--tools` / `settings.tools` 打开。

use async_trait::async_trait;
use ignore::{WalkBuilder, WalkState};
use rupi_core::{CancelFlag, ToolDefinition};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

mod optional;
mod shell;
mod truncate;
mod utf8;
pub use optional::{optional_builtins_requested, FindTool, LsTool, OPTIONAL_BUILTIN_NAMES};
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
    /// `find` / `ls` 默认不注册，见 [`Self::register_optional`]。
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

    /// 按名打开可选内建（`find` / `ls`）。未知名忽略；已注册则覆盖。
    pub fn register_optional<I, S>(&mut self, names: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for n in names {
            match n.as_ref() {
                "find" => self.register(Arc::new(FindTool)),
                "ls" => self.register(Arc::new(LsTool)),
                _ => {}
            }
        }
    }

    /// 沙箱版可选内建：`path` 约束在 `root` 内（与 read/glob 同箍）。
    pub fn register_sandboxed_optional<I, S>(&mut self, root: &std::path::Path, names: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        for n in names {
            match n.as_ref() {
                "find" => self.register(Arc::new(SandboxedTool::new(
                    Arc::new(FindTool),
                    root.clone(),
                ))),
                "ls" => self.register(Arc::new(SandboxedTool::new(Arc::new(LsTool), root.clone()))),
                _ => {}
            }
        }
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
            description: "Read a file from disk. Text pages stop at 2000 lines or 50KB (whichever comes first) on a whole line; continue with offset. Images are attached.".into(),
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

/// 读一行（含换行符）：最多把 `cap` 字节放进 `buf`，其余部分只消费不保存，
/// 返回整行字节数（0 = EOF）。跳过 offset、数剩余行时 `cap=0`，单行巨型文件也不占内存。
async fn read_line_capped<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    cap: usize,
    buf: &mut Vec<u8>,
) -> std::io::Result<usize> {
    use tokio::io::AsyncBufReadExt;
    let mut total = 0usize;
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            return Ok(total);
        }
        let (n, done) = match chunk.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (chunk.len(), false),
        };
        let room = cap.saturating_sub(buf.len());
        buf.extend_from_slice(&chunk[..n.min(room)]);
        reader.consume(n);
        total += n;
        if done {
            return Ok(total);
        }
    }
}

/// 文本分页（对标 Pi read：2000 行 / 50KB 先到先停，只在整行处停）。页内不再做中间截断，
/// 按 `pass offset=` 提示翻页逐页拼接即原文（CRLF 归一为 LF）。非 UTF-8 字节按 U+FFFD
/// 有损解码（Pi `toString("utf-8")` 同语义），不整文件报错。
async fn read_path_paged(path: &str, offset: usize, limit: usize) -> ToolOutput {
    use tokio::io::AsyncReadExt;
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
    let mut buf = Vec::new();
    let mut skipped = 0usize;
    while skipped < offset {
        match read_line_capped(&mut reader, 0, &mut buf).await {
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
    // 已读出但放不进本页的那一行：算作剩余行，下一页从它开始。
    let mut overflow = false;
    while page.len() < limit {
        buf.clear();
        // 留出 `\r\n` 两字节：内容恰为 50KB 的行也能完整放进 buf。
        let total = match read_line_capped(&mut reader, DEFAULT_MAX_BYTES + 2, &mut buf).await {
            Ok(0) => {
                hit_eof = true;
                break;
            }
            Ok(n) => n,
            Err(e) => return ToolOutput::err(format!("read {path} failed: {e}")),
        };
        if buf.last() == Some(&b'\n') {
            buf.pop();
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
        }
        // 按解码后的长度计：非法字节变 U+FFFD（3 字节），页大小仍守 50KB。
        // 被 cap 截断的超长行解码后必然超过上限，走下面的超限分支。
        let line = decode_utf8(&buf);
        let sep = usize::from(!page.is_empty());
        if page_bytes + sep + line.len() > DEFAULT_MAX_BYTES {
            if page.is_empty() {
                // 首行本身超限（对标 Pi firstLineExceedsLimit）：给 bash 读法，并允许跳过这一行。
                let line_no = offset + 1;
                return ToolOutput::ok(format!(
                    "[line {line_no} is {}, exceeds the {} read limit. Use bash: sed -n '{line_no}p' {path} | head -c {DEFAULT_MAX_BYTES} — or pass offset={line_no} to skip it]",
                    format_size(total),
                    format_size(DEFAULT_MAX_BYTES)
                ));
            }
            overflow = true;
            break;
        }
        page_bytes += sep + line.len();
        page.push(line);
    }
    if page.is_empty() && hit_eof {
        return ToolOutput::ok(format!(
            "[read {path}: offset {offset} past end ({offset} lines)]"
        ));
    }
    let start = offset + 1;
    let end = offset + page.len();
    let mut more = overflow;
    let mut total: Option<usize> = None;
    if !hit_eof {
        if file_len <= READ_COUNT_REMAINING_MAX {
            let mut rem = usize::from(overflow);
            loop {
                match read_line_capped(&mut reader, 0, &mut buf).await {
                    Ok(0) => break,
                    Ok(_) => rem += 1,
                    Err(e) => return ToolOutput::err(format!("read {path} failed: {e}")),
                }
            }
            more = rem > 0;
            total = Some(end + rem);
        } else if !overflow {
            let mut peek = [0u8; 1];
            more = matches!(reader.read(&mut peek).await, Ok(n) if n > 0);
        }
    }
    let mut s = page.join("\n");
    if more {
        let cap = if overflow {
            format!(" ({} limit)", format_size(DEFAULT_MAX_BYTES))
        } else {
            String::new()
        };
        match total {
            Some(n) => s.push_str(&format!(
                "\n…[truncated: lines {start}-{end} of {n}{cap} — pass offset={end} for more]"
            )),
            None => s.push_str(&format!(
                "\n…[truncated: lines {start}-{end} · {file_len} bytes{cap} — pass offset={end} for more]"
            )),
        }
    } else if offset > 0 {
        s.push_str(&format!("\n[end of file: {} lines]", total.unwrap_or(end)));
    }
    ToolOutput::ok(s)
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
            description: "Execute a shell command (bounded output; default 30s timeout). Unix: sh -c. Windows: PowerShell (pwsh.exe if present, else powershell.exe) with -NoProfile -NonInteractive -ExecutionPolicy Bypass; commands are not translated from bash. Inherits RUPI_SESSION_ID / RUPI_SESSION_FILE / RUPI_PROVIDER / RUPI_MODEL / RUPI_REASONING_LEVEL (Pi: PI_SESSION_ID / PI_SESSION_FILE / PI_PROVIDER / PI_MODEL / PI_REASONING_LEVEL).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "timeout_secs": {"type": "integer", "description": "1-300, default 30"}
                },
                "required": ["command"]
            }),
            prompt_snippet: Some(
                "bash(command, timeout_secs?): run shell command (PowerShell on Windows)".into(),
            ),
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
    /// stdin 接 `/dev/null`（对标 Pi `stdio: ["ignore", ...]`）：读 stdin 的命令立即见 EOF，
    /// 不挂到超时，也不和 RPC/TUI 宿主抢输入。
    async fn execute_with_cancel(
        &self,
        arguments: serde_json::Value,
        cancel: &CancelFlag,
    ) -> anyhow::Result<ToolOutput> {
        let (command, timeout_secs) = Self::parse_args(&arguments);
        let spec = crate::shell::ShellSpec::current(command);
        let mut cmd = tokio::process::Command::new(&spec.program);
        cmd.args(&spec.args);
        for (k, v) in current_session_env() {
            cmd.env(k, v);
        }
        // Unix：自成进程组，取消/超时 killpg。Windows：CREATE_NEW_PROCESS_GROUP
        // + CREATE_NO_WINDOW，杀树走 taskkill /T。
        crate::shell::configure_isolation(cmd.as_std_mut());
        let mut child = match cmd
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => return Ok(ToolOutput::err(format!("spawn failed: {e}"))),
        };
        // wait 之后 `child.id()` 为 None；进程组 id 即 shell pid，留着给后台进程收尾用。
        let pid = child.id();
        // 先拿走管道再 wait：reader 与 wait/取消/超时并发，写满缓冲也不会堵死子进程。
        // 尾部 ring 共享出来：reader 被放弃（孙进程占着写端）时已收内容照样可取。
        let activity = Arc::new(tokio::sync::Notify::new());
        let out_tail = Arc::new(Mutex::new(PipeTail::new(DEFAULT_MAX_BYTES)));
        let err_tail = Arc::new(Mutex::new(PipeTail::new(DEFAULT_MAX_BYTES)));
        let mut out_task = tokio::spawn(read_pipe_tail(
            child.stdout.take(),
            out_tail.clone(),
            activity.clone(),
        ));
        let mut err_task = tokio::spawn(read_pipe_tail(
            child.stderr.take(),
            err_tail.clone(),
            activity.clone(),
        ));
        let timeout = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs));
        tokio::pin!(timeout);
        enum End {
            Done(std::process::ExitStatus),
            Cancelled,
            TimedOut,
            WaitFailed(String),
        }
        let mut end = tokio::select! {
            _ = cancel.cancelled() => End::Cancelled,
            _ = &mut timeout => End::TimedOut,
            res = child.wait() => match res {
                Ok(status) => End::Done(status),
                Err(e) => End::WaitFailed(e.to_string()),
            },
        };
        if matches!(end, End::Done(_)) {
            // shell 已退出，但后台孙进程（`sleep 9 &`、`npm run dev &`）可能继承了
            // 管道写端，读到 EOF 要等它们退出。对标 Pi waitForChildProcess：等管道关闭，
            // 或静默满 100ms（每来一段输出重新计时）即收尾，后台进程照旧运行；
            // 这期间取消或超时则杀掉整组。
            let mut readers = std::pin::pin!(async {
                let _ = (&mut out_task).await;
                let _ = (&mut err_task).await;
            });
            loop {
                tokio::select! {
                    _ = &mut readers => break,
                    idle = tokio::time::timeout(EXIT_STDIO_GRACE, activity.notified()) => {
                        if idle.is_err() {
                            break;
                        }
                    }
                    _ = cancel.cancelled() => {
                        end = End::Cancelled;
                        break;
                    }
                    _ = &mut timeout => {
                        end = End::TimedOut;
                        break;
                    }
                }
            }
            if !matches!(end, End::Done(_)) {
                if let Some(pid) = pid {
                    crate::shell::kill_pid_tree(pid);
                }
            }
        } else {
            crate::shell::kill_tree(&mut child).await;
            // 杀组后残余只再等 500ms 排空管道缓冲，不无限等。
            let _ = tokio::time::timeout(std::time::Duration::from_millis(500), async {
                let _ = (&mut out_task).await;
                let _ = (&mut err_task).await;
            })
            .await;
        }
        // 放弃仍阻塞在孙进程管道上的 reader（对标 Pi 收尾时 destroy 流）。
        out_task.abort();
        err_task.abort();
        let mut s = out_tail.lock().unwrap().take_string();
        let err_text = err_tail.lock().unwrap().take_string();
        if !err_text.is_empty() {
            s.push_str(&format!("\n[stderr]\n{err_text}"));
        }
        let s = bound_bash_output(&s);
        let with_partial = |head: String| {
            if s.is_empty() {
                head
            } else {
                format!("{head}\n[partial output]\n{s}")
            }
        };
        Ok(match end {
            End::Done(status) if status.success() => ToolOutput::ok(s),
            End::Done(status) => ToolOutput::err(format!("{}: {s}", exit_status_text(status))),
            End::Cancelled => ToolOutput::err(with_partial("cancelled by user".into())),
            End::TimedOut => ToolOutput::err(with_partial(format!(
                "command timed out after {timeout_secs}s"
            ))),
            End::WaitFailed(e) => ToolOutput::err(with_partial(format!("wait failed: {e}"))),
        })
    }
}

/// shell 退出后等后台孙进程释放管道的静默宽限（对标 Pi `EXIT_STDIO_GRACE_MS`）。
const EXIT_STDIO_GRACE: std::time::Duration = std::time::Duration::from_millis(100);

/// 非零退出的前缀：`exit code N`；Unix 被信号杀死时 `killed by signal N`。
/// `ExitStatus` 的 Display 自带 `exit status:`，直接拼会变成 `exit exit status: 3`。
fn exit_status_text(status: std::process::ExitStatus) -> String {
    if let Some(code) = status.code() {
        return format!("exit code {code}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(sig) = status.signal() {
            return format!("killed by signal {sig}");
        }
    }
    format!("exit {status}")
}

/// 并发排空一条管道进共享尾部 ring（对标 bash 50KB 尾部），每段输出唤醒一次 `activity`。
async fn read_pipe_tail<R: tokio::io::AsyncRead + Unpin + Send>(
    reader: Option<R>,
    tail: Arc<Mutex<PipeTail>>,
    activity: Arc<tokio::sync::Notify>,
) {
    use tokio::io::AsyncReadExt as _;
    let Some(mut r) = reader else {
        return;
    };
    let mut chunk = [0u8; 8192];
    loop {
        match r.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                tail.lock().unwrap().push(&chunk[..n]);
                activity.notify_one();
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

    fn take_string(&mut self) -> String {
        let v: Vec<u8> = self.buf.drain(..).collect();
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

pub(crate) async fn spawn_blocking_tool(
    f: impl FnOnce() -> ToolOutput + Send + 'static,
) -> ToolOutput {
    tokio::task::spawn_blocking(f)
        .await
        .unwrap_or_else(|e| ToolOutput::err(format!("search interrupted: {e}")))
}

/// 标准过滤器：隐藏文件、`.gitignore` / `.ignore`、全局 exclude。`require_git` 保持默认
/// true，只在 git 仓库内应用 gitignore（测试夹具建空 `.git` 即可隔离父目录规则）。
pub(crate) fn ignore_walker(root: impl AsRef<Path>) -> WalkBuilder {
    let mut b = WalkBuilder::new(root);
    b.standard_filters(true).follow_links(false);
    b
}

/// 相对路径匹配：`require_literal_separator` 让 `*` 不跨目录（`*.rs` ≠ `sub/c.rs`），
/// 同时 `**/*.rs` 仍命中根下 `a.rs`（与 `glob::glob("base/**/*.rs")` 一致）。
pub(crate) fn glob_rel_matches(pattern: &str, rel: &Path) -> bool {
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
/// 对标 `PI_SESSION_FILE`；SQLite 主存时可空。
pub const SESSION_FILE_ENV: &str = "RUPI_SESSION_FILE";
/// 对标 `PI_PROVIDER`。
pub const PROVIDER_ENV: &str = "RUPI_PROVIDER";
/// 对标 `PI_MODEL`。
pub const MODEL_ENV: &str = "RUPI_MODEL";
/// 对标 `PI_REASONING_LEVEL`。
pub const REASONING_ENV: &str = "RUPI_REASONING_LEVEL";

#[derive(Debug, Clone, Default)]
pub struct SessionContext {
    pub session_id: String,
    pub session_file: String,
    pub provider: String,
    pub model: String,
    pub reasoning_level: String,
}

fn session_ctx() -> &'static std::sync::Mutex<SessionContext> {
    static CTX: std::sync::OnceLock<std::sync::Mutex<SessionContext>> = std::sync::OnceLock::new();
    CTX.get_or_init(|| std::sync::Mutex::new(SessionContext::default()))
}

fn apply_session_env(ctx: &SessionContext) {
    unsafe {
        std::env::set_var(SESSION_ENV_VAR, &ctx.session_id);
        std::env::set_var(SESSION_FILE_ENV, &ctx.session_file);
        std::env::set_var(PROVIDER_ENV, &ctx.provider);
        std::env::set_var(MODEL_ENV, &ctx.model);
        std::env::set_var(REASONING_ENV, &ctx.reasoning_level);
    }
}

/// 导出当前会话 id 到进程环境（子进程继承；resume 沿用 db 会话 id，跨进程稳定）。
pub fn export_session_id(id: &str) {
    let mut g = session_ctx().lock().unwrap();
    g.session_id = id.to_string();
    apply_session_env(&g);
}

pub fn export_session_file(path: &str) {
    let mut g = session_ctx().lock().unwrap();
    g.session_file = path.to_string();
    apply_session_env(&g);
}

pub fn export_model(provider: &str, model: &str) {
    let mut g = session_ctx().lock().unwrap();
    g.provider = provider.to_string();
    g.model = model.to_string();
    apply_session_env(&g);
}

pub fn export_reasoning_level(level: &str) {
    let mut g = session_ctx().lock().unwrap();
    g.reasoning_level = level.to_string();
    apply_session_env(&g);
}

pub fn export_session_context(ctx: SessionContext) {
    *session_ctx().lock().unwrap() = ctx.clone();
    apply_session_env(&ctx);
}

pub fn current_session_env() -> Vec<(String, String)> {
    let g = session_ctx().lock().unwrap();
    vec![
        (SESSION_ENV_VAR.into(), g.session_id.clone()),
        (SESSION_FILE_ENV.into(), g.session_file.clone()),
        (PROVIDER_ENV.into(), g.provider.clone()),
        (MODEL_ENV.into(), g.model.clone()),
        (REASONING_ENV.into(), g.reasoning_level.clone()),
    ]
}

/// 超过 `limit` 字节时保留首尾各约 `limit/2` 字节（向内对齐到字符边界），中部折叠并标注
/// 截掉的字节数。判断与截取统一按字节：此前按字节判断、按字符截取，多字节文本
/// 头尾两段会重叠，内容重复且比原文更长。
pub fn truncate_middle(s: &str, limit: usize) -> String {
    if s.len() <= limit {
        return s.to_owned();
    }
    let keep = limit / 2;
    let mut head_end = keep;
    while !s.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = s.len() - keep;
    while !s.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    format!(
        "{}\n…[truncated {} bytes]…\n{}",
        &s[..head_end],
        tail_start - head_end,
        &s[tail_start..]
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
        let names = r.names();
        assert!(!names.iter().any(|n| n == "find" || n == "ls"));
        let out = r.execute("nope", serde_json::json!({})).await.unwrap();
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn optional_find_ls_off_until_registered() {
        let mut r = ToolRegistry::with_builtins();
        let missing = r
            .execute("find", serde_json::json!({"pattern": "*.rs"}))
            .await
            .unwrap();
        assert!(missing.is_error);
        assert!(missing.content.contains("unknown tool"));
        r.register_optional(["find", "ls", "nope"]);
        assert!(r.names().iter().any(|n| n == "find"));
        assert!(r.names().iter().any(|n| n == "ls"));
        assert_eq!(r.definitions().len(), 9);
    }

    #[tokio::test]
    async fn find_lists_relative_and_rejects_escapes() {
        let dir = unique_tmp("rupi-find");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.rs"), "x").unwrap();
        std::fs::write(dir.join("b.txt"), "x").unwrap();
        std::fs::write(dir.join("sub/c.rs"), "x").unwrap();
        let mut r = ToolRegistry::with_builtins();
        r.register_optional(["find"]);
        let base = dir.to_string_lossy().to_string();
        let out = r
            .execute(
                "find",
                serde_json::json!({"pattern": "**/*.rs", "path": base}),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("a.rs"), "{}", out.content);
        assert!(out.content.contains("sub/c.rs"), "{}", out.content);
        assert!(!out.content.contains("b.txt"));
        assert!(
            !out.content.contains(&base),
            "find must return relative paths: {}",
            out.content
        );
        let esc = r
            .execute("find", serde_json::json!({"pattern": "../x", "path": base}))
            .await
            .unwrap();
        assert!(esc.is_error);
        let empty = r
            .execute("find", serde_json::json!({"pattern": "", "path": base}))
            .await
            .unwrap();
        assert!(empty.is_error);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn ls_lists_dotfiles_and_dirs() {
        let dir = unique_tmp("rupi-ls");
        std::fs::write(dir.join("z.txt"), "x").unwrap();
        std::fs::write(dir.join(".hidden"), "x").unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let mut r = ToolRegistry::with_builtins();
        r.register_optional(["ls"]);
        let base = dir.to_string_lossy().to_string();
        let out = r
            .execute("ls", serde_json::json!({"path": base}))
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains(".hidden"), "{}", out.content);
        assert!(out.content.contains("sub/"), "{}", out.content);
        assert!(out.content.contains("z.txt"), "{}", out.content);
        let file = dir.join("z.txt").to_string_lossy().to_string();
        let not_dir = r
            .execute("ls", serde_json::json!({"path": file}))
            .await
            .unwrap();
        assert!(not_dir.is_error);
        assert!(not_dir.content.contains("Not a directory"));
        let missing = r
            .execute("ls", serde_json::json!({"path": format!("{base}/nope")}))
            .await
            .unwrap();
        assert!(missing.is_error);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn sandboxed_optional_find_ls_stay_inside_root() {
        let root = unique_tmp("rupi-opt-sbx");
        std::fs::write(root.join("in.rs"), "x").unwrap();
        let mut r = ToolRegistry::with_sandboxed_builtins(&root);
        r.register_sandboxed_optional(&root, ["find", "ls"]);
        let listing = r
            .execute("ls", serde_json::json!({"path": "."}))
            .await
            .unwrap();
        assert!(!listing.is_error, "{}", listing.content);
        assert!(listing.content.contains("in.rs"), "{}", listing.content);
        let found = r
            .execute("find", serde_json::json!({"pattern": "*.rs", "path": "."}))
            .await
            .unwrap();
        assert!(!found.is_error, "{}", found.content);
        assert!(found.content.contains("in.rs"), "{}", found.content);
        let esc = r
            .execute("ls", serde_json::json!({"path": "/tmp"}))
            .await
            .unwrap();
        assert!(esc.is_error);
        assert!(esc.content.contains("escapes workspace root"));
        let _ = std::fs::remove_dir_all(&root);
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

    /// 按 `pass offset=N` 提示一直翻页，把各页正文拼回原文。
    async fn read_all_pages(r: &ToolRegistry, path: &str) -> (String, usize) {
        let mut offset = 0usize;
        let mut pages = Vec::new();
        loop {
            let out = r
                .execute("read", serde_json::json!({"path": path, "offset": offset}))
                .await
                .unwrap();
            assert!(!out.is_error, "{}", out.content);
            assert!(out.content.len() <= DEFAULT_MAX_BYTES + 200, "page too big");
            assert!(
                !out.content.contains("…[truncated "),
                "page cut in the middle"
            );
            if let Some((body, marker)) = out.content.rsplit_once("\n…[truncated: lines ") {
                let next = marker
                    .split("pass offset=")
                    .nth(1)
                    .and_then(|s| s.split(' ').next())
                    .unwrap()
                    .parse::<usize>()
                    .unwrap();
                assert!(next > offset, "offset must advance");
                pages.push(body.to_string());
                offset = next;
            } else {
                let body = out
                    .content
                    .rsplit_once("\n[end of file: ")
                    .map(|(b, _)| b.to_string())
                    .unwrap_or(out.content);
                pages.push(body);
                return (pages.join("\n"), pages.len());
            }
        }
    }

    #[tokio::test]
    async fn read_pages_concatenate_to_original_without_dropped_lines() {
        // 旧实现在 12KB 停页后又整体中间截断：L0060/L0061 这类页中行被静默剪掉，翻页也看不到。
        let dir = unique_tmp("rupi-read-pages");
        let r = ToolRegistry::with_builtins();
        let ascii: String = (0..3000)
            .map(|i| format!("L{i:04} {}", "x".repeat(94)))
            .collect::<Vec<_>>()
            .join("\n");
        let cjk: String = (0..3000)
            .map(|i| format!("第{i:04}行 中文内容与表情😀"))
            .collect::<Vec<_>>()
            .join("\n");
        for (name, body) in [("ascii.txt", &ascii), ("cjk.txt", &cjk)] {
            let p = dir.join(name);
            std::fs::write(&p, format!("{body}\n")).unwrap();
            let (joined, n) = read_all_pages(&r, &p.to_string_lossy()).await;
            assert!(n > 1, "{name}: should take several pages");
            assert_eq!(&joined, body, "{name}: pages must rebuild the file");
        }
        let first = r
            .execute(
                "read",
                serde_json::json!({"path": dir.join("ascii.txt").to_string_lossy()}),
            )
            .await
            .unwrap();
        assert!(first.content.contains("L0060") && first.content.contains("L0061"));
        assert!(
            first.content.contains("of 3000 (50.0KB limit)"),
            "{}",
            first.content
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_huge_single_line_points_to_bash_and_lossy_decodes_latin1() {
        let dir = unique_tmp("rupi-read-edge");
        let r = ToolRegistry::with_builtins();
        let wide = dir.join("wide.txt");
        std::fs::write(&wide, format!("{}\nnext\n", "y".repeat(80_000))).unwrap();
        let out = r
            .execute("read", serde_json::json!({"path": wide.to_string_lossy()}))
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("line 1 is"), "{}", out.content);
        assert!(out.content.contains("sed -n '1p'"), "{}", out.content);
        assert!(out.content.len() < 1000);
        let skip = r
            .execute(
                "read",
                serde_json::json!({"path": wide.to_string_lossy(), "offset": 1}),
            )
            .await
            .unwrap();
        assert!(skip.content.starts_with("next"), "{}", skip.content);
        // latin1：旧实现 read_line 遇非法 UTF-8 整文件报错，Pi 是有损解码。
        let latin = dir.join("latin1.txt");
        std::fs::write(&latin, b"caf\xe9\nok\n").unwrap();
        let out = r
            .execute("read", serde_json::json!({"path": latin.to_string_lossy()}))
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(out.content, "caf\u{FFFD}\nok");
        let _ = std::fs::remove_dir_all(&dir);
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

    #[test]
    fn bash_definition_documents_windows_powershell() {
        let d = BashTool.definition();
        assert!(d.description.contains("sh -c"), "{}", d.description);
        assert!(d.description.contains("PowerShell"), "{}", d.description);
        assert!(
            d.description.contains("not translated"),
            "{}",
            d.description
        );
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
    fn truncate_middle_counts_bytes_and_never_duplicates_multibyte_text() {
        // 中文 3 字节/字、emoji 4 字节：旧实现按字节判断、按字符截取，头尾重叠后比原文还长。
        let line = |i: usize| format!("第{i:04}行：中文内容😀\n");
        let s: String = (0..600).map(line).collect();
        assert!(s.len() > 12_000 && s.chars().count() < 12_000);
        let out = truncate_middle(&s, 12_000);
        assert!(out.len() < 12_000 + 64, "len {}", out.len());
        for i in 0..600 {
            assert!(out.matches(&line(i)).count() <= 1, "line {i} duplicated");
        }
        assert!(out.starts_with(&line(0)) && out.ends_with(&line(599)));
        let (head, rest) = out.split_once("\n…[truncated ").unwrap();
        let (count, tail) = rest.split_once(" bytes]…\n").unwrap();
        assert!(s.starts_with(head) && s.ends_with(tail));
        assert_eq!(
            head.len() + count.parse::<usize>().unwrap() + tail.len(),
            s.len()
        );
        assert_eq!(truncate_middle("短文本", 100), "短文本");
    }

    #[test]
    fn pipe_tail_keeps_only_last_bytes() {
        let mut t = PipeTail::new(8);
        t.push(b"abcdefghij");
        assert_eq!(t.take_string(), "cdefghij");
        let mut t = PipeTail::new(8);
        t.push(b"abcd");
        t.push(b"efghij");
        assert_eq!(t.take_string(), "cdefghij");
    }

    #[tokio::test]
    async fn bash_background_child_does_not_hold_the_call() {
        // shell 退出后孙进程仍持有管道写端：旧实现无限等 EOF，6s 才返回、超时与取消都失效。
        let tool = BashTool;
        let start = std::time::Instant::now();
        let out = tool
            .execute(serde_json::json!({"command": "sleep 5 & echo hi", "timeout_secs": 30}))
            .await
            .unwrap();
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "took {:?}",
            start.elapsed()
        );
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(out.content.trim(), "hi");
        // 后台进程持续输出时继续收（宽限按静默计时），停下后收尾。
        let start = std::time::Instant::now();
        let out = tool
            .execute(serde_json::json!({
                "command": "(for i in 1 2 3; do echo tick$i; sleep 0.05; done; sleep 5) & echo go",
                "timeout_secs": 30
            }))
            .await
            .unwrap();
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
        assert!(out.content.contains("tick3"), "{}", out.content);
    }

    #[tokio::test]
    async fn bash_stdin_is_null_so_readers_see_eof() {
        // 旧实现继承宿主 stdin：`cat` / `read` 挂到超时，RPC 下还和 JSONL 通道抢输入。
        // 测试进程自身 stdin 可能已是 /dev/null，所以重启本测试二进制、喂一根不关闭的
        // 管道当 stdin，在子进程里跑探针（复现 `rupi --mode rpc` 的宿主环境）。
        if std::env::var_os("RUPI_BASH_STDIN_PROBE").is_some() {
            let start = std::time::Instant::now();
            let out = BashTool
                .execute(serde_json::json!({
                    "command": "cat; read x; echo got:$x",
                    "timeout_secs": 5
                }))
                .await
                .unwrap();
            assert!(start.elapsed() < std::time::Duration::from_secs(3));
            assert!(!out.is_error, "{}", out.content);
            assert_eq!(out.content.trim(), "got:");
            return;
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::bash_stdin_is_null_so_readers_see_eof",
                "--test-threads=1",
            ])
            .env("RUPI_BASH_STDIN_PROBE", "1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let _held_open = child.stdin.take();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "probe failed:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8_lossy(&out.stdout).contains("1 passed"));
    }

    #[tokio::test]
    async fn bash_timeout_keeps_partial_output_and_exit_text_is_clean() {
        let out = BashTool
            .execute(serde_json::json!({
                "command": "echo partial; echo warn >&2; sleep 5",
                "timeout_secs": 1
            }))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(
            out.content.starts_with("command timed out after 1s"),
            "{}",
            out.content
        );
        assert!(
            out.content.contains("[partial output]\npartial"),
            "{}",
            out.content
        );
        assert!(out.content.contains("[stderr]\nwarn"), "{}", out.content);
        let fail = BashTool
            .execute(serde_json::json!({"command": "echo boom; exit 3"}))
            .await
            .unwrap();
        assert!(fail.is_error);
        assert_eq!(fail.content, "exit code 3: boom");
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
    async fn bash_sees_rupi_model_env() {
        export_session_context(SessionContext {
            session_id: "s1".into(),
            session_file: String::new(),
            provider: "openai".into(),
            model: "gpt-4o-mini".into(),
            reasoning_level: "high".into(),
        });
        let r = ToolRegistry::with_builtins();
        let out = r
            .execute(
                "bash",
                serde_json::json!({
                    "command": "printf '%s %s %s' \"$RUPI_PROVIDER\" \"$RUPI_MODEL\" \"$RUPI_REASONING_LEVEL\""
                }),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(out.content, "openai gpt-4o-mini high");
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
