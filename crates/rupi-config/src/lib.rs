//! rupi 配置：`~/.rupi/settings.json` + 项目 `.rupi/settings.json` 嵌套合并。
//!
//! 对标 Pi `settings.json`（model / thinking / compaction / tools / theme，
//! 以及 steeringMode / followUpMode / defaultProjectTrust / externalEditor / enabledModels）。
//! 可选只读合并 `~/.pi/agent/settings.json` 与 `.pi/settings.json`（同键 rupi 优先）。
//! 非法文件只 warning，回默认——主循环不因配置崩。

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 压实参数（对标上游 `compaction`：`reserveTokens` / `keepRecentTokens`）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CompactionSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reserve_tokens: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keep_recent_tokens: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<usize>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub model_overrides: HashMap<String, CompactionOverride>,
}

/// 单模型覆盖。字段缺省回全局 compaction。
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CompactionOverride {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reserve_tokens: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keep_recent_tokens: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<usize>,
}

/// 项目信任默认策略（对标 Pi `defaultProjectTrust`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ProjectTrust {
    #[default]
    Ask,
    Always,
    Never,
}

impl ProjectTrust {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "ask" => Some(Self::Ask),
            "always" | "yes" | "y" => Some(Self::Always),
            "never" | "no" | "n" => Some(Self::Never),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Always => "always",
            Self::Never => "never",
        }
    }
}

/// 已合并的 settings（pi 只读层 ← 全局 rupi ← 项目 rupi，嵌套对象字段级覆盖）。
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// 内建工具白名单（`defaultTools` 别名）。项目数组**整表替换**全局。
    #[serde(alias = "defaultTools", skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_tools: Vec<String>,
    #[serde(default, skip_serializing_if = "compaction_is_empty")]
    pub compaction: CompactionSettings,
    /// `all` | `one-at-a-time`（对标 Pi `steeringMode` / #11 `QueueMode`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steering_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub follow_up_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_project_trust: Option<ProjectTrust>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_editor: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enabled_models: Vec<String>,
}

fn compaction_is_empty(c: &CompactionSettings) -> bool {
    c.enabled.is_none()
        && c.reserve_tokens.is_none()
        && c.keep_recent_tokens.is_none()
        && c.context_window.is_none()
        && c.model_overrides.is_empty()
}

impl Settings {
    pub fn load(home: &Path, cwd: &Path) -> Self {
        let user_home = user_home();
        let pi_global = load_file(&user_home.join(".pi/agent/settings.json"));
        let rupi_global = load_file(&home.join("settings.json"));
        let pi_project = find_pi_settings(cwd)
            .map(|p| load_file(&p))
            .unwrap_or_default();
        let rupi_project = find_project_settings(cwd)
            .map(|p| load_file(&p))
            .unwrap_or_default();
        // 同键 rupi 优先：先叠 Pi 只读层，再用 rupi 全局/项目覆盖。
        merge(
            merge(pi_global, pi_project),
            merge(rupi_global, rupi_project),
        )
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

    pub fn project_trust(&self) -> ProjectTrust {
        self.default_project_trust.unwrap_or(ProjectTrust::Ask)
    }

    pub fn steering_mode_str(&self) -> &str {
        self.steering_mode.as_deref().unwrap_or("one-at-a-time")
    }

    pub fn follow_up_mode_str(&self) -> &str {
        self.follow_up_mode.as_deref().unwrap_or("one-at-a-time")
    }

    pub fn external_editor(&self) -> Option<&str> {
        self.external_editor
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }
}

/// `/settings` 斜杠：列出或改一项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsSlash {
    Show,
    Set { key: String, value: String },
}

/// 解析 `/settings` / `/settings key value`。非该命令返回 None。
pub fn parse_settings_slash(input: &str) -> Option<SettingsSlash> {
    let t = input.trim();
    if t == "/settings" {
        return Some(SettingsSlash::Show);
    }
    let rest = t.strip_prefix("/settings ")?.trim();
    if rest.is_empty() {
        return Some(SettingsSlash::Show);
    }
    let (key, value) = if let Some((k, v)) = rest.split_once('=') {
        (k.trim(), v.trim())
    } else if let Some((k, v)) = rest.split_once(char::is_whitespace) {
        (k.trim(), v.trim())
    } else {
        return Some(SettingsSlash::Set {
            key: rest.to_string(),
            value: String::new(),
        });
    };
    Some(SettingsSlash::Set {
        key: key.to_string(),
        value: value.to_string(),
    })
}

/// 人类可读列表（含写入目标）。
pub fn format_settings(settings: &Settings, write_path: &Path) -> String {
    let models = if settings.enabled_models.is_empty() {
        "(all catalog)".to_string()
    } else {
        settings.enabled_models.join(", ")
    };
    let editor = settings.external_editor().unwrap_or("(VISUAL/EDITOR)");
    format!(
        "settings (write → {}):\n  steeringMode           {}\n  followUpMode           {}\n  defaultProjectTrust    {}\n  externalEditor         {editor}\n  enabledModels          {models}\n  model                  {}\n  thinking               {}",
        write_path.display(),
        settings.steering_mode_str(),
        settings.follow_up_mode_str(),
        settings.project_trust().as_str(),
        settings.model.as_deref().unwrap_or("(unset)"),
        settings.thinking.as_deref().unwrap_or("(unset)"),
    )
}

/// 可热改的核心键。
pub fn is_core_key(key: &str) -> bool {
    matches!(
        normalize_key(key).as_str(),
        "steeringmode"
            | "followupmode"
            | "defaultprojecttrust"
            | "externaleditor"
            | "enabledmodels"
            | "model"
            | "thinking"
    )
}

/// 改内存中的一项；返回写入 JSON 用的 camelCase 键与值。
pub fn apply_setting(
    settings: &mut Settings,
    key: &str,
    value: &str,
) -> anyhow::Result<(String, Value)> {
    match normalize_key(key).as_str() {
        "steeringmode" => {
            let v = parse_queue_mode(value)?;
            settings.steering_mode = Some(v.to_string());
            Ok(("steeringMode".into(), json!(v)))
        }
        "followupmode" => {
            let v = parse_queue_mode(value)?;
            settings.follow_up_mode = Some(v.to_string());
            Ok(("followUpMode".into(), json!(v)))
        }
        "defaultprojecttrust" => {
            let t = ProjectTrust::parse(value).ok_or_else(|| {
                anyhow::anyhow!("defaultProjectTrust must be ask|always|never")
            })?;
            settings.default_project_trust = Some(t);
            Ok(("defaultProjectTrust".into(), json!(t.as_str())))
        }
        "externaleditor" => {
            let v = value.trim();
            settings.external_editor = if v.is_empty() {
                None
            } else {
                Some(v.to_string())
            };
            Ok(("externalEditor".into(), json!(v)))
        }
        "enabledmodels" => {
            let list = parse_model_list(value);
            settings.enabled_models = list.clone();
            Ok(("enabledModels".into(), json!(list)))
        }
        "model" => {
            let v = value.trim();
            settings.model = if v.is_empty() {
                None
            } else {
                Some(v.to_string())
            };
            Ok(("model".into(), json!(v)))
        }
        "thinking" => {
            let v = value.trim();
            settings.thinking = if v.is_empty() {
                None
            } else {
                Some(v.to_string())
            };
            Ok(("thinking".into(), json!(v)))
        }
        _ => anyhow::bail!(
            "unknown setting '{key}' (steeringMode|followUpMode|defaultProjectTrust|externalEditor|enabledModels)"
        ),
    }
}

/// 项目层已有 `.rupi/settings.json` 则写那里，否则写 `~/.rupi/settings.json`。
pub fn write_target(home: &Path, cwd: &Path) -> PathBuf {
    if let Some(p) = find_project_settings(cwd) {
        return p;
    }
    home.join("settings.json")
}

/// 把一项补丁写进目标 JSON（保留文件里其它键）。
pub fn persist_patch(path: &Path, key: &str, value: Value) -> anyhow::Result<()> {
    let mut root = match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str::<Value>(raw.trim_start_matches('\u{FEFF}'))
            .unwrap_or_else(|_| json!({})),
        Err(_) => json!({}),
    };
    if !root.is_object() {
        root = json!({});
    }
    root.as_object_mut()
        .expect("object")
        .insert(key.to_string(), value);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(&root)?;
    std::fs::write(path, format!("{body}\n"))?;
    Ok(())
}

fn parse_queue_mode(value: &str) -> anyhow::Result<&'static str> {
    match value.trim() {
        "all" => Ok("all"),
        "one-at-a-time" | "one_at_a_time" | "oneatatime" => Ok("one-at-a-time"),
        other => anyhow::bail!("queue mode must be all|one-at-a-time, got '{other}'"),
    }
}

fn parse_model_list(raw: &str) -> Vec<String> {
    raw.split(|c: char| c == ',' || c.is_whitespace())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(|c| *c != '_' && *c != '-')
        .flat_map(|c| c.to_lowercase())
        .collect()
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

fn find_pi_settings(cwd: &Path) -> Option<PathBuf> {
    let mut dir = Some(cwd.to_path_buf());
    while let Some(d) = dir {
        let cand = d.join(".pi").join("settings.json");
        if cand.is_file() {
            return Some(cand);
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    None
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

fn user_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
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
    if over.steering_mode.is_some() {
        base.steering_mode = over.steering_mode;
    }
    if over.follow_up_mode.is_some() {
        base.follow_up_mode = over.follow_up_mode;
    }
    if over.default_project_trust.is_some() {
        base.default_project_trust = over.default_project_trust;
    }
    if over.external_editor.is_some() {
        base.external_editor = over.external_editor;
    }
    if !over.enabled_models.is_empty() {
        base.enabled_models = over.enabled_models;
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
    fn rupi_keys_win_over_pi_and_core_keys_roundtrip() {
        let base = std::env::temp_dir().join(format!("rupi-cfg-pi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let home = base.join("rupi-home");
        let user = base.join("user");
        let proj = base.join("proj");
        write(
            &user.join(".pi/agent"),
            "settings.json",
            r#"{"steeringMode":"all","model":"from-pi","defaultProjectTrust":"always"}"#,
        );
        write(
            &home,
            "settings.json",
            r#"{"steeringMode":"one-at-a-time","externalEditor":"hx"}"#,
        );
        write(
            &proj.join(".pi"),
            "settings.json",
            r#"{"followUpMode":"all","enabledModels":["pi/m"]}"#,
        );
        write(
            &proj.join(".rupi"),
            "settings.json",
            r#"{"enabledModels":["openai/gpt-4o-mini"],"defaultProjectTrust":"never"}"#,
        );
        let saved = std::env::var("HOME").ok();
        unsafe { std::env::set_var("HOME", user.as_os_str()) };
        let s = Settings::load(&home, &proj);
        if let Some(v) = saved {
            unsafe { std::env::set_var("HOME", v) };
        } else {
            unsafe { std::env::remove_var("HOME") };
        }
        assert_eq!(s.steering_mode_str(), "one-at-a-time");
        assert_eq!(s.follow_up_mode_str(), "all");
        assert_eq!(s.project_trust(), ProjectTrust::Never);
        assert_eq!(s.external_editor(), Some("hx"));
        assert_eq!(s.enabled_models, vec!["openai/gpt-4o-mini".to_string()]);
        assert_eq!(s.model.as_deref(), Some("from-pi"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn settings_slash_and_persist_patch() {
        let mut s = Settings::default();
        let cmd = parse_settings_slash("/settings steeringMode all").unwrap();
        let SettingsSlash::Set { key, value } = cmd else {
            panic!("expected set");
        };
        let (jk, jv) = apply_setting(&mut s, &key, &value).unwrap();
        assert_eq!(jk, "steeringMode");
        assert_eq!(s.steering_mode_str(), "all");
        let dir = std::env::temp_dir().join(format!("rupi-cfg-write-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        persist_patch(&path, &jk, jv).unwrap();
        persist_patch(&path, "theme", json!("dark")).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("steeringMode"));
        assert!(raw.contains("dark"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_target_prefers_project_file() {
        let base = std::env::temp_dir().join(format!("rupi-cfg-tgt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let home = base.join("home");
        let proj = base.join("proj");
        write(&proj.join(".rupi"), "settings.json", r#"{}"#);
        let t = write_target(&home, &proj);
        assert!(t.ends_with(".rupi/settings.json"));
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
