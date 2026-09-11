//! rupi 配置：`~/.rupi/settings.json` + 项目 `.rupi/settings.json` 嵌套合并。
//!
//! 对标 Pi `settings.json`（model / thinking / compaction / tools / theme）。
//! 非法文件只 warning，回默认——主循环不因配置崩。

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 压实参数（对标上游 `compaction`：`reserveTokens` / `keepRecentTokens`）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CompactionSettings {
    pub enabled: Option<bool>,
    pub reserve_tokens: Option<usize>,
    pub keep_recent_tokens: Option<usize>,
    pub context_window: Option<usize>,
    #[serde(default)]
    pub model_overrides: HashMap<String, CompactionOverride>,
}

/// 单模型覆盖。字段缺省回全局 compaction。
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CompactionOverride {
    pub reserve_tokens: Option<usize>,
    pub keep_recent_tokens: Option<usize>,
    pub context_window: Option<usize>,
}

/// 已合并的 settings（全局 ← 项目，嵌套对象字段级覆盖）。
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub theme: Option<String>,
    /// 内建工具白名单（`defaultTools` 别名）。项目数组**整表替换**全局。
    #[serde(alias = "defaultTools")]
    pub tools: Option<Vec<String>>,
    pub exclude_tools: Vec<String>,
    pub compaction: CompactionSettings,
}

impl Settings {
    pub fn load(home: &Path, cwd: &Path) -> Self {
        let global = load_file(&home.join("settings.json"));
        let project = find_project_settings(cwd)
            .map(|p| load_file(&p))
            .unwrap_or_default();
        merge(global, project)
    }

    pub fn theme(&self) -> &str {
        self.theme.as_deref().unwrap_or("dark")
    }

    pub fn compaction_enabled(&self) -> bool {
        self.compaction.enabled.unwrap_or(true)
    }

    pub fn reserve_tokens(&self) -> usize {
        self.compaction.reserve_tokens.unwrap_or(16_384)
    }

    pub fn keep_recent_tokens(&self) -> usize {
        self.compaction.keep_recent_tokens.unwrap_or(20_000)
    }
}

/// 解析 `--tools` / `--exclude-tools` 逗号或空白分隔列表。
pub fn parse_tool_list(raw: &str) -> Vec<String> {
    raw.split(|c: char| c == ',' || c.is_whitespace())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

/// `allow` 为 `Some` 时是全工具白名单（空表 = 全关）；再扣 `exclude`。
pub fn tool_allowed(name: &str, allow: Option<&[String]>, exclude: &[String]) -> bool {
    if exclude.iter().any(|e| e == name) {
        return false;
    }
    match allow {
        None => true,
        Some(a) if a.is_empty() => false,
        Some(a) => a.iter().any(|t| t == name),
    }
}

/// SYSTEM.md / APPEND_SYSTEM.md + CLI 覆盖。
///
/// 替换优先级：`--system-prompt` > 项目 `.rupi/SYSTEM.md` > `~/.rupi/SYSTEM.md`。
/// 追加顺序：全局 APPEND → 项目 APPEND → `--append-system-prompt`。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SystemPromptFiles {
    pub replace: Option<String>,
    pub append: String,
}

pub fn load_system_prompt_files(
    home: &Path,
    cwd: &Path,
    cli_system: Option<&str>,
    cli_append: Option<&str>,
    load_project: bool,
) -> SystemPromptFiles {
    let mut replace = read_optional(&home.join("SYSTEM.md"));
    let mut append = read_optional(&home.join("APPEND_SYSTEM.md")).unwrap_or_default();
    if load_project {
        if let Some(dir) = find_rupi_dir(cwd) {
            if let Some(s) = read_optional(&dir.join("SYSTEM.md")) {
                replace = Some(s);
            }
            if let Some(s) = read_optional(&dir.join("APPEND_SYSTEM.md")) {
                if !append.is_empty() && !append.ends_with('\n') {
                    append.push('\n');
                }
                append.push_str(&s);
            }
        }
    }
    if let Some(s) = cli_system.map(str::trim).filter(|s| !s.is_empty()) {
        replace = Some(s.to_string());
    }
    if let Some(s) = cli_append.map(str::trim).filter(|s| !s.is_empty()) {
        if !append.is_empty() && !append.ends_with('\n') {
            append.push('\n');
        }
        append.push_str(s);
    }
    SystemPromptFiles { replace, append }
}

fn find_project_settings(cwd: &Path) -> Option<PathBuf> {
    find_rupi_dir(cwd)
        .map(|d| d.join("settings.json"))
        .filter(|p| p.is_file())
}

fn find_rupi_dir(cwd: &Path) -> Option<PathBuf> {
    let mut dir = Some(cwd.to_path_buf());
    while let Some(d) = dir {
        let cand = d.join(".rupi");
        if cand.is_dir() {
            return Some(cand);
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    None
}

fn load_file(path: &Path) -> Settings {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Settings::default();
    };
    match serde_json::from_str::<Settings>(raw.trim_start_matches('\u{FEFF}')) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("settings {} 解析失败，忽略: {e}", path.display());
            Settings::default()
        }
    }
}

fn merge(mut base: Settings, over: Settings) -> Settings {
    if over.model.is_some() {
        base.model = over.model;
    }
    if over.thinking.is_some() {
        base.thinking = over.thinking;
    }
    if over.theme.is_some() {
        base.theme = over.theme;
    }
    if over.tools.is_some() {
        base.tools = over.tools;
    }
    if !over.exclude_tools.is_empty() {
        base.exclude_tools = over.exclude_tools;
    }
    base.compaction = merge_compaction(base.compaction, over.compaction);
    base
}

fn merge_compaction(mut base: CompactionSettings, over: CompactionSettings) -> CompactionSettings {
    if over.enabled.is_some() {
        base.enabled = over.enabled;
    }
    if over.reserve_tokens.is_some() {
        base.reserve_tokens = over.reserve_tokens;
    }
    if over.keep_recent_tokens.is_some() {
        base.keep_recent_tokens = over.keep_recent_tokens;
    }
    if over.context_window.is_some() {
        base.context_window = over.context_window;
    }
    for (k, v) in over.model_overrides {
        let e = base.model_overrides.entry(k).or_default();
        if v.reserve_tokens.is_some() {
            e.reserve_tokens = v.reserve_tokens;
        }
        if v.keep_recent_tokens.is_some() {
            e.keep_recent_tokens = v.keep_recent_tokens;
        }
        if v.context_window.is_some() {
            e.context_window = v.context_window;
        }
    }
    base
}

fn read_optional(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let t = raw.strip_prefix('\u{FEFF}').unwrap_or(&raw);
    let t = t.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn project_overrides_global_nested_compaction() {
        let base = std::env::temp_dir().join(format!("rupi-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let home = base.join("home");
        let proj = base.join("proj");
        write(
            &home,
            "settings.json",
            r#"{"theme":"dark","model":"gpt-4o-mini","compaction":{"enabled":true,"reserveTokens":16384}}"#,
        );
        write(
            &proj.join(".rupi"),
            "settings.json",
            r#"{"compaction":{"reserveTokens":8192},"tools":["read","bash"]}"#,
        );
        let s = Settings::load(&home, &proj);
        assert_eq!(s.theme(), "dark");
        assert_eq!(s.model.as_deref(), Some("gpt-4o-mini"));
        assert_eq!(s.reserve_tokens(), 8192);
        assert_eq!(s.compaction_enabled(), true);
        assert_eq!(
            s.tools.as_deref(),
            Some(&["read".into(), "bash".into()][..])
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn system_md_project_replaces_global_append_stacks() {
        let base = std::env::temp_dir().join(format!("rupi-sys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let home = base.join("home");
        let proj = base.join("proj");
        write(&home, "SYSTEM.md", "GLOBAL SYS");
        write(&home, "APPEND_SYSTEM.md", "GLOBAL APP");
        write(&proj.join(".rupi"), "SYSTEM.md", "PROJ SYS");
        write(&proj.join(".rupi"), "APPEND_SYSTEM.md", "PROJ APP");
        let files = load_system_prompt_files(&home, &proj, None, Some("CLI APP"), true);
        assert_eq!(files.replace.as_deref(), Some("PROJ SYS"));
        assert!(files.append.contains("GLOBAL APP"));
        assert!(files.append.contains("PROJ APP"));
        assert!(files.append.contains("CLI APP"));
        let cli = load_system_prompt_files(&home, &proj, Some("CLI SYS"), None, true);
        assert_eq!(cli.replace.as_deref(), Some("CLI SYS"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn tool_list_and_allow() {
        assert_eq!(
            parse_tool_list("read, bash  edit"),
            vec!["read", "bash", "edit"]
        );
        assert!(tool_allowed("read", None, &[]));
        assert!(!tool_allowed("bash", Some(&["read".into()]), &[]));
        assert!(!tool_allowed(
            "read",
            Some(&["read".into()]),
            &["read".into()]
        ));
        assert!(!tool_allowed("bash", Some(&[]), &[]));
    }

    #[test]
    fn bad_json_is_ignored() {
        let base = std::env::temp_dir().join(format!("rupi-cfg-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        write(&base, "settings.json", "{nope");
        let s = Settings::load(&base, &base);
        assert_eq!(s, Settings::default());
        let _ = std::fs::remove_dir_all(&base);
    }
}
