//! rupi-ext: 外部进程扩展（对标 Pi 的 extension + hot reload）。
//!
//! Pi 哲学："No MCP. Build CLI tools with READMEs, or build an extension"。
//! 本 crate 落的是前半句：每个扩展 = 一个 manifest（`*.json`）+ 任意可执行命令。
//!
//! 契约：调用时把 `arguments` JSON 写进子进程 stdin，stdout 即工具结果；
//! 非零退出码 → tool error（`stderr` 并入），绝不崩主循环。
//! 热重载：`ExtensionSet::refresh()` 按 mtime 增量重载，agent 写新工具后
//! `/reload`（或每轮自动检查）即刻可用；修工具开 side-quest branch，修完 rewind 回来。

use rupi_core::ToolDefinition;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

/// 扩展 manifest（`extensions/*.json`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionManifest {
    /// 工具名：小写-数字-下划线，注册后 agent 可见。
    pub name: String,
    pub description: String,
    /// JSON Schema object（参数表）。
    pub input_schema: serde_json::Value,
    /// 注入 "Available tools" 的短文本；缺失则回退到 description。
    #[serde(default)]
    pub prompt_snippet: Option<String>,
    /// 要执行的命令（如 `sh` / `python3` / 自写脚本路径）。
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// 超时秒数，默认 30。
    /// 显式 0 视为“未指定”回默认值：钳到 1s 会让冷启动的解释器（python 等）在负载下
    /// 必现误杀，1s 墙钟对子进程 spawn 本就是竞态。
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_timeout() -> u64 {
    30
}

/// 生效超时：0 回默认值，其余至少 1s（防 `timeout(0)` 瞬杀）。
fn effective_timeout_secs(manifest_secs: u64) -> u64 {
    if manifest_secs == 0 {
        default_timeout()
    } else {
        manifest_secs.max(1)
    }
}

impl ExtensionManifest {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.name.is_empty()
            || !self
                .name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        {
            anyhow::bail!("extension name must be [a-z0-9_-]+");
        }
        if !self.input_schema.is_object() {
            anyhow::bail!("input_schema must be a JSON object");
        }
        if self.command.is_empty() {
            anyhow::bail!("command required");
        }
        Ok(())
    }

    pub fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.clone(),
            description: self.description.clone(),
            input_schema: self.input_schema.clone(),
            prompt_snippet: Some(
                self.prompt_snippet
                    .clone()
                    .unwrap_or_else(|| self.description.clone()),
            ),
        }
    }
}

/// 单个外部工具的执行器。
pub struct ExternalTool {
    manifest: ExtensionManifest,
}

impl ExternalTool {
    pub fn new(manifest: ExtensionManifest) -> Self {
        Self { manifest }
    }
}

#[async_trait::async_trait]
impl rupi_tools::Tool for ExternalTool {
    fn definition(&self) -> ToolDefinition {
        self.manifest.definition()
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
    ) -> anyhow::Result<rupi_tools::ToolOutput> {
        let m = &self.manifest;
        let child = match Self::spawn_with_input(m, &arguments).await {
            Ok(c) => c,
            Err(out) => return Ok(out),
        };
        let timeout_secs = effective_timeout_secs(m.timeout_secs);
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            child.wait_with_output(),
        )
        .await;
        // 超时：timeout 会丢弃 wait future，连带 kill_on_drop(true) 干掉子进程，无僵尸
        Ok(Self::finish(out, timeout_secs))
    }

    /// 取消即返回：wait future 被丢弃，连带 kill_on_drop(true) 干掉子进程，
    /// 调用方不再等外部进程收尾（与超时同语义）。
    async fn execute_with_cancel(
        &self,
        arguments: serde_json::Value,
        cancel: &rupi_core::CancelFlag,
    ) -> anyhow::Result<rupi_tools::ToolOutput> {
        let m = &self.manifest;
        let child = match Self::spawn_with_input(m, &arguments).await {
            Ok(c) => c,
            Err(out) => return Ok(out),
        };
        let timeout_secs = effective_timeout_secs(m.timeout_secs);
        let out = tokio::select! {
            _ = cancel.cancelled() => None,
            r = tokio::time::timeout(
                std::time::Duration::from_secs(timeout_secs),
                child.wait_with_output(),
            ) => Some(r),
        };
        match out {
            None => Ok(rupi_tools::ToolOutput::err("cancelled by user")),
            Some(r) => Ok(Self::finish(r, timeout_secs)),
        }
    }
}

impl ExternalTool {
    /// 起外部进程并把 arguments 灌进 stdin。失败直接给 tool error；
    /// stdin 写失败时先 drop child（kill_on_drop 杀进程），不留孤儿。
    async fn spawn_with_input(
        m: &ExtensionManifest,
        arguments: &serde_json::Value,
    ) -> Result<tokio::process::Child, rupi_tools::ToolOutput> {
        use tokio::io::AsyncWriteExt as _;
        let child = tokio::process::Command::new(&m.command)
            .args(&m.args)
            .envs(&m.env)
            .kill_on_drop(true)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                return Err(rupi_tools::ToolOutput::err(format!(
                    "spawn {} failed: {e}",
                    m.command
                )));
            }
        };
        if let Some(mut stdin) = child.stdin.take() {
            let body = arguments.to_string();
            if stdin.write_all(body.as_bytes()).await.is_err() {
                drop(child);
                return Err(rupi_tools::ToolOutput::err("write stdin failed"));
            }
        }
        Ok(child)
    }

    /// wait/timeout 收尾（纯函数，取消/非取消路径共用）。
    fn finish(
        out: Result<
            Result<std::process::Output, std::io::Error>,
            tokio::time::error::Elapsed,
        >,
        timeout_secs: u64,
    ) -> rupi_tools::ToolOutput {
        match out {
            Ok(Ok(out)) => {
                // 外部进程输出同样有界：与内置 bash 同口径折叠，保上下文窗口
                let text = rupi_tools::truncate_middle(
                    &String::from_utf8_lossy(&out.stdout),
                    rupi_tools::MAX_TOOL_OUTPUT,
                )
                .trim()
                .to_string();
                if out.status.success() {
                    rupi_tools::ToolOutput::ok(text)
                } else {
                    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
                    rupi_tools::ToolOutput::err(format!(
                        "exit {}: {err}",
                        out.status
                    ))
                }
            }
            Ok(Err(e)) => rupi_tools::ToolOutput::err(format!("wait failed: {e}")),
            Err(_) => rupi_tools::ToolOutput::err(format!(
                "extension timed out after {timeout_secs}s"
            )),
        }
    }
}

/// 一组扩展 + mtime 快照：`refresh()` 增量重载（新增/修改/删除）。
pub struct ExtensionSet {
    pub dir: PathBuf,
    /// 文件 → (mtime, 工具名)
    snapshot: HashMap<String, (SystemTime, String)>,
}

impl ExtensionSet {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            snapshot: HashMap::new(),
        }
    }

    fn manifests(&self) -> Vec<(String, PathBuf, SystemTime)> {
        let mut out = vec![];
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return out;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let mtime = e
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            out.push((p.to_string_lossy().to_string(), p, mtime));
        }
        out.sort();
        out
    }

    /// 全量加载（启动时）：坏 manifest 只 warning 跳过。
    pub fn load_all(&mut self) -> Vec<ExtensionManifest> {
        let mut manifests = vec![];
        for (key, path, mtime) in self.manifests() {
            match load_one(&path) {
                Ok(m) => {
                    self.snapshot.insert(key, (mtime, m.name.clone()));
                    manifests.push(m);
                }
                Err(e) => tracing::warn!("skip extension {}: {e:#}", path.display()),
            }
        }
        manifests
    }

    /// 增量重载：返回 (新增/修改 manifests, 删除的工具名)。无变化返回空。
    /// manifest 改名视为“删旧名 + 加新名”，调用方先注销再注册，无 stale 工具。
    pub fn refresh(&mut self) -> (Vec<ExtensionManifest>, Vec<String>) {
        let current = self.manifests();
        let mut current_map = HashMap::new();
        for (key, path, mtime) in &current {
            current_map.insert(key.clone(), (*mtime, path.clone()));
        }
        let mut changed = vec![];
        let mut removed_later = vec![];
        for (key, (mtime, path)) in &current_map {
            let stale = self
                .snapshot
                .get(key)
                .map(|(t, _)| t != mtime)
                .unwrap_or(true);
            if !stale {
                continue;
            }
            match load_one(path) {
                Ok(m) => {
                    // 改名：旧工具名必须注销，否则 registry 里永远残留
                    if let Some((_, old_name)) = self.snapshot.get(key) {
                        if *old_name != m.name {
                            removed_later.push(old_name.clone());
                        }
                    }
                    self.snapshot.insert(key.clone(), (*mtime, m.name.clone()));
                    changed.push(m);
                }
                Err(e) => tracing::warn!("skip extension {}: {e:#}", path.display()),
            }
        }
        let mut removed = removed_later;
        let known: Vec<String> = self.snapshot.keys().cloned().collect();
        for key in known {
            if !current_map.contains_key(&key) {
                if let Some((_, tool_name)) = self.snapshot.remove(&key) {
                    removed.push(tool_name);
                }
            }
        }
        (changed, removed)
    }
}

fn load_one(path: &Path) -> anyhow::Result<ExtensionManifest> {
    let raw = std::fs::read_to_string(path)?;
    let m: ExtensionManifest = serde_json::from_str(&raw)?;
    m.validate()?;
    Ok(m)
}

/// 把 manifests 注册进 `ToolRegistry`（同名：后加载覆盖，便于热更新）。
pub fn register_all(registry: &mut rupi_tools::ToolRegistry, manifests: Vec<ExtensionManifest>) {
    for m in manifests {
        registry.register(Arc::new(ExternalTool::new(m)));
    }
}

/// 增量热重载：新增/修改重注册，删除注销。返回反馈行（空=无变化，
/// 调用方按需展示：REPL 印 stderr，TUI 进视图）。
pub fn refresh_extensions(
    tools: &mut rupi_tools::ToolRegistry,
    set: &mut ExtensionSet,
) -> Vec<String> {
    let (changed, removed) = set.refresh();
    let mut lines = Vec::new();
    for name in removed {
        tools.unregister(&name);
        lines.push(format!("[ext] removed {name}"));
    }
    if !changed.is_empty() {
        let names: Vec<String> = changed.iter().map(|m| m.name.clone()).collect();
        register_all(tools, changed);
        lines.push(format!("[ext] reloaded: {}", names.join(", ")));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use rupi_tools::Tool as _;

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), body).unwrap();
    }

    const ECHO_MANIFEST: &str = r#"{
        "name": "upper", "description": "uppercase stdin",
        "input_schema": {"type": "object"},
        "command": "sh", "args": ["-c", "tr a-z A-Z"]
    }"#;

    #[test]
    fn manifest_validation_rejects_bad_names() {
        let m: ExtensionManifest = serde_json::from_str(
            r#"{"name": "Bad Name", "description": "x", "input_schema": {}, "command": "sh"}"#,
        )
        .unwrap();
        assert!(m.validate().is_err());
    }

    #[tokio::test]
    async fn external_tool_pipes_stdin_to_stdout() {
        let m: ExtensionManifest = serde_json::from_str(ECHO_MANIFEST).unwrap();
        let t = ExternalTool::new(m);
        // stdin 收到 arguments JSON，tr 转大写后回显
        let out = t.execute(serde_json::json!({"text": "hi"})).await.unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("TEXT"));
    }

    #[tokio::test]
    async fn failing_command_becomes_tool_error() {
        let m: ExtensionManifest = serde_json::from_str(
            r#"{"name": "nope", "description": "x", "input_schema": {}, "command": "sh", "args": ["-c", "exit 3"]}"#,
        )
        .unwrap();
        let out = ExternalTool::new(m)
            .execute(serde_json::json!({}))
            .await
            .unwrap();
        assert!(out.is_error);
    }

    #[test]
    fn refresh_detects_add_change_remove() {
        let dir = std::env::temp_dir().join(format!("rupi-ext-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write(&dir, "a.json", ECHO_MANIFEST);
        let mut set = ExtensionSet::new(dir.clone());
        assert_eq!(set.load_all().len(), 1);
        let (changed, removed) = set.refresh();
        assert!(changed.is_empty() && removed.is_empty());

        // 新增
        write(&dir, "b.json", &ECHO_MANIFEST.replace("upper", "lower"));
        let (changed, _) = set.refresh();
        assert_eq!(changed.len(), 1);

        // 删除（返回的是 manifest 里的工具名，不是文件名）
        std::fs::remove_file(dir.join("b.json")).unwrap();
        let (changed, removed) = set.refresh();
        assert!(changed.is_empty());
        assert_eq!(removed, vec!["lower".to_string()]);

        // 修改（内容变 + mtime 变）
        std::thread::sleep(std::time::Duration::from_millis(20));
        write(
            &dir,
            "a.json",
            &ECHO_MANIFEST.replace("uppercase stdin", "UPPER v2"),
        );
        let (changed, _) = set.refresh();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].description, "UPPER v2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refresh_rename_unregisters_old_tool_name() {
        let dir = std::env::temp_dir().join(format!("rupi-ext-rename-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write(&dir, "a.json", ECHO_MANIFEST);
        let mut set = ExtensionSet::new(dir.clone());
        assert_eq!(set.load_all().len(), 1);
        // 同文件改名：旧工具名必须出现在 removed，否则 registry 残留 stale 工具
        std::thread::sleep(std::time::Duration::from_millis(20));
        write(
            &dir,
            "a.json",
            &ECHO_MANIFEST.replace("\"upper\"", "\"shout\""),
        );
        let (changed, removed) = set.refresh();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].name, "shout");
        assert_eq!(removed, vec!["upper".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn zero_means_default_not_one_second() {
        assert_eq!(effective_timeout_secs(0), default_timeout());
        assert_eq!(effective_timeout_secs(5), 5);
    }

    #[tokio::test]
    async fn zero_timeout_is_clamped_not_instant() {
        let m: ExtensionManifest = serde_json::from_str(
            r#"{"name": "z", "description": "x", "input_schema": {}, "command": "echo", "args": ["hi"], "timeout_secs": 0}"#,
        )
        .unwrap();
        let out = ExternalTool::new(m)
            .execute(serde_json::json!({}))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("hi"));
    }
}
