//! 资源启停（对标 `pi config`：`!name` 禁用、`+name` 强制启用，不涉及 OAuth）。

use serde_json::json;
use std::path::{Path, PathBuf};

use crate::{load_file, persist_patch, Settings};

/// `rupi config` 可启停的资源类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigKind {
    Packages,
    Extensions,
    Skills,
    Prompts,
    Themes,
}

impl ConfigKind {
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "package" | "packages" | "pkg" => Ok(Self::Packages),
            "extension" | "extensions" | "ext" => Ok(Self::Extensions),
            "skill" | "skills" => Ok(Self::Skills),
            "prompt" | "prompts" | "command" | "commands" => Ok(Self::Prompts),
            "theme" | "themes" => Ok(Self::Themes),
            other => {
                anyhow::bail!("unknown kind `{other}` (packages|extensions|skills|prompts|themes)")
            }
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::Packages => "packages",
            Self::Extensions => "extensions",
            Self::Skills => "skills",
            Self::Prompts => "prompts",
            Self::Themes => "themes",
        }
    }

    pub fn as_str(self) -> &'static str {
        self.key()
    }

    pub fn all() -> [Self; 5] {
        [
            Self::Packages,
            Self::Extensions,
            Self::Skills,
            Self::Prompts,
            Self::Themes,
        ]
    }
}

/// 发现目录 + `!`/`-` 排除 + `+` 强制启用。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResourceFilter {
    pub extra_paths: Vec<PathBuf>,
    pub deny: Vec<String>,
    pub force: Vec<String>,
}

impl ResourceFilter {
    pub fn from_specs(specs: Option<&[String]>) -> Self {
        let Some(specs) = specs else {
            return Self::default();
        };
        let mut out = Self::default();
        for raw in specs {
            let s = raw.trim();
            if s.is_empty() || token_on(s) || token_off(s) {
                continue;
            }
            if let Some(rest) = s.strip_prefix('!').or_else(|| s.strip_prefix('-')) {
                if !rest.is_empty() {
                    out.deny.push(rest.to_string());
                }
                continue;
            }
            if let Some(rest) = s.strip_prefix('+') {
                if !rest.is_empty() {
                    if looks_like_path(rest) {
                        out.extra_paths.push(PathBuf::from(rest));
                    }
                    out.force.push(rest.to_string());
                }
                continue;
            }
            if looks_like_path(s) {
                out.extra_paths.push(PathBuf::from(s));
            } else {
                out.force.push(s.to_string());
            }
        }
        out
    }

    pub fn allows(&self, name: &str) -> bool {
        if self.force.iter().any(|f| names_match(f, name)) {
            return true;
        }
        !self.deny.iter().any(|d| names_match(d, name))
    }
}

/// `!name` / `+name` / 裸名是否指向同一资源。
pub fn names_match(pattern: &str, name: &str) -> bool {
    let p = pattern.trim().trim_start_matches(['!', '+', '-']);
    let n = name.trim().trim_start_matches(['!', '+', '-']);
    if p.is_empty() || n.is_empty() {
        return false;
    }
    if p == n || p.eq_ignore_ascii_case(n) {
        return true;
    }
    let pl = p.to_ascii_lowercase();
    let nl = n.to_ascii_lowercase();
    nl.ends_with(&format!(":{pl}"))
        || nl.ends_with(&format!("/{pl}"))
        || pl.ends_with(&format!(":{nl}"))
        || pl.ends_with(&format!("/{nl}"))
}

/// 改内存中的启停表：去掉该名旧模式，再写入 `+name` 或 `!name`。
pub fn toggle_spec_list(list: &mut Vec<String>, name: &str, enable: bool) {
    let key = name.trim();
    list.retain(|p| !names_match(p, key));
    if enable {
        list.push(format!("+{key}"));
    } else {
        list.push(format!("!{key}"));
    }
}

pub fn settings_specs(settings: &Settings, kind: ConfigKind) -> Option<&[String]> {
    match kind {
        ConfigKind::Packages => settings.packages.as_deref(),
        ConfigKind::Extensions => settings.extensions.as_deref(),
        ConfigKind::Skills => settings.skills.as_deref(),
        ConfigKind::Prompts => settings.prompts.as_deref(),
        ConfigKind::Themes => settings.themes.as_deref(),
    }
}

pub fn toggle_in_settings(
    settings: &mut Settings,
    kind: ConfigKind,
    name: &str,
    enable: bool,
) -> Vec<String> {
    let slot = match kind {
        ConfigKind::Packages => &mut settings.packages,
        ConfigKind::Extensions => &mut settings.extensions,
        ConfigKind::Skills => &mut settings.skills,
        ConfigKind::Prompts => &mut settings.prompts,
        ConfigKind::Themes => &mut settings.themes,
    };
    let mut v = slot.take().unwrap_or_default();
    toggle_spec_list(&mut v, name, enable);
    *slot = Some(v.clone());
    v
}

/// 写 `settings.json` 的对应数组（保留文件里其它键）。
pub fn persist_toggle(
    path: &Path,
    kind: ConfigKind,
    name: &str,
    enable: bool,
) -> anyhow::Result<Vec<String>> {
    let mut settings = load_file(path);
    let arr = toggle_in_settings(&mut settings, kind, name, enable);
    persist_patch(path, kind.key(), json!(arr))?;
    Ok(arr)
}

/// `rupi config` 写入目标：默认家目录；`-l` 写项目 `.rupi/settings.json`。
pub fn config_write_target(home: &Path, cwd: &Path, local: bool) -> PathBuf {
    if local {
        cwd.join(".rupi").join("settings.json")
    } else {
        home.join("settings.json")
    }
}

fn looks_like_path(s: &str) -> bool {
    s.contains('/')
        || s.contains('\\')
        || s.starts_with('.')
        || s.starts_with('~')
        || Path::new(s).exists()
}

fn token_off(s: &str) -> bool {
    matches!(
        s.to_ascii_lowercase().as_str(),
        "off" | "false" | "none" | "disable" | "disabled"
    )
}

fn token_on(s: &str) -> bool {
    matches!(
        s.to_ascii_lowercase().as_str(),
        "on" | "true" | "default" | "enable" | "enabled"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_deny_and_force() {
        let f = ResourceFilter::from_specs(Some(&["!neon".into(), "+demo-skill".into()]));
        assert!(!f.allows("neon"));
        assert!(f.allows("dark"));
        assert!(f.allows("demo-skill"));
        let f = ResourceFilter::from_specs(Some(&["!npm:foo".into()]));
        assert!(!f.allows("npm:foo"));
        assert!(!f.allows("foo"));
        assert!(f.allows("bar"));
    }

    #[test]
    fn toggle_roundtrip() {
        let mut s = Settings::default();
        toggle_in_settings(&mut s, ConfigKind::Themes, "neon", false);
        assert_eq!(s.themes.as_deref(), Some(&["!neon".to_string()][..]));
        assert!(!ResourceFilter::from_specs(s.themes.as_deref()).allows("neon"));
        toggle_in_settings(&mut s, ConfigKind::Themes, "neon", true);
        assert_eq!(s.themes.as_deref(), Some(&["+neon".to_string()][..]));
        assert!(ResourceFilter::from_specs(s.themes.as_deref()).allows("neon"));
    }

    #[test]
    fn persist_toggle_keeps_other_keys() {
        let dir = std::env::temp_dir().join(format!(
            "rupi-cfg-toggle-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, r#"{"theme":"dark","model":"x"}"#).unwrap();
        persist_toggle(&path, ConfigKind::Skills, "commit-helper", false).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("commit-helper"), "{raw}");
        assert!(raw.contains("\"theme\""), "{raw}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
