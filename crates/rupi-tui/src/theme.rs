//! TUI 主题：内建 `dark`/`light` + 发现 `themes/*.json`（对标 Pi themes 包）。
//!
//! 扫描：`themes/builtin`、`~/.rupi/themes`、`~/.pi/agent/themes`、项目
//! `.rupi/themes` / `.pi/themes`、`settings.themes[]` 路径、包物化文件。
//! `settings.theme` 与 `/reload` 走同一套目录；`RUPI_THEME` 覆盖名称。

use ratatui::style::Color;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Theme {
    pub name: String,
    pub id: u64,
    pub user: Color,
    pub assistant: Color,
    pub tool: Color,
    pub system: Color,
    pub thinking: Color,
    pub prompt: Color,
    pub border: Color,
    pub footer: Color,
    pub heading: Color,
    pub code: Color,
    pub accent: Color,
    pub syntax_comment: Color,
    pub syntax_keyword: Color,
    pub syntax_function: Color,
    pub syntax_variable: Color,
    pub syntax_string: Color,
    pub syntax_number: Color,
    pub syntax_type: Color,
    pub syntax_operator: Color,
    pub syntax_punctuation: Color,
}

impl Theme {
    pub fn dark() -> Self {
        Self {
            name: "dark".into(),
            id: 0,
            user: Color::Cyan,
            assistant: Color::White,
            tool: Color::Yellow,
            system: Color::DarkGray,
            thinking: Color::Magenta,
            prompt: Color::Green,
            border: Color::Gray,
            footer: Color::DarkGray,
            heading: Color::LightBlue,
            code: Color::LightGreen,
            accent: Color::LightYellow,
            syntax_comment: Color::DarkGray,
            syntax_keyword: Color::LightBlue,
            syntax_function: Color::Cyan,
            syntax_variable: Color::White,
            syntax_string: Color::LightGreen,
            syntax_number: Color::LightYellow,
            syntax_type: Color::LightMagenta,
            syntax_operator: Color::Gray,
            syntax_punctuation: Color::DarkGray,
        }
    }

    pub fn light() -> Self {
        Self {
            name: "light".into(),
            id: 1,
            user: Color::Blue,
            assistant: Color::Black,
            tool: Color::Rgb(160, 100, 0),
            system: Color::Gray,
            thinking: Color::Magenta,
            prompt: Color::Green,
            border: Color::DarkGray,
            footer: Color::Gray,
            heading: Color::Blue,
            code: Color::Rgb(0, 100, 40),
            accent: Color::Rgb(140, 80, 0),
            syntax_comment: Color::Gray,
            syntax_keyword: Color::Blue,
            syntax_function: Color::Rgb(0, 90, 140),
            syntax_variable: Color::Black,
            syntax_string: Color::Rgb(0, 100, 40),
            syntax_number: Color::Rgb(140, 80, 0),
            syntax_type: Color::Magenta,
            syntax_operator: Color::DarkGray,
            syntax_punctuation: Color::Gray,
        }
    }

    pub fn from_name(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "light" => Self::light(),
            _ => Self::dark(),
        }
    }

    pub fn from_env() -> Self {
        match std::env::var("RUPI_THEME") {
            Ok(v) if !v.is_empty() => Self::from_name(&v),
            _ => Self::dark(),
        }
    }

    /// 启动用：`RUPI_THEME` 覆盖 `settings.theme`。
    pub fn resolve(settings_theme: &str) -> Self {
        match std::env::var("RUPI_THEME") {
            Ok(v) if !v.trim().is_empty() => Self::from_name(&v),
            _ => Self::from_name(settings_theme),
        }
    }

    /// 发现目录后解析；未知名回落 `dark`/`light`。
    pub fn resolve_at(
        settings_theme: &str,
        home: &Path,
        cwd: &Path,
        load_project: bool,
        extra: Option<&[String]>,
    ) -> Self {
        let catalog = ThemeCatalog::discover(home, cwd, load_project, extra);
        let filter = rupi_config::ResourceFilter::from_specs(extra);
        let name = match std::env::var("RUPI_THEME") {
            Ok(v) if !v.trim().is_empty() => v,
            _ => settings_theme.to_string(),
        };
        catalog.resolve_filtered(&name, &filter)
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::from_env()
    }
}

/// 已发现主题（后扫覆盖同名）。
#[derive(Debug, Clone)]
pub struct ThemeEntry {
    pub name: String,
    pub source: &'static str,
    pub path: Option<PathBuf>,
    pub theme: Theme,
}

#[derive(Debug, Clone, Default)]
pub struct ThemeCatalog {
    pub entries: Vec<ThemeEntry>,
}

impl ThemeCatalog {
    pub fn discover(home: &Path, cwd: &Path, load_project: bool, extra: Option<&[String]>) -> Self {
        let mut cat = Self::default();
        cat.push_builtin(Theme::dark(), "builtin", None);
        cat.push_builtin(Theme::light(), "builtin", None);

        let user_home = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));
        let filter = rupi_config::ResourceFilter::from_specs(extra);

        for dir in theme_search_dirs(home, &user_home, cwd, load_project) {
            let source = source_of(&dir, home, &user_home);
            cat.scan_dir(&dir, source);
        }
        for p in &filter.extra_paths {
            let expanded = expand_user_path(p);
            if expanded.is_dir() {
                cat.scan_dir(&expanded, "settings");
            } else if expanded.is_file() {
                cat.scan_file(&expanded, "settings");
            }
        }
        cat
    }

    fn push_builtin(&mut self, theme: Theme, source: &'static str, path: Option<PathBuf>) {
        self.upsert(ThemeEntry {
            name: theme.name.clone(),
            source,
            path,
            theme,
        });
    }

    fn scan_dir(&mut self, dir: &Path, source: &'static str) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        let mut files: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        files.sort();
        for p in files {
            self.scan_file(&p, source);
        }
    }

    fn scan_file(&mut self, path: &Path, source: &'static str) {
        if path.extension().and_then(|s| s.to_str()) != Some("json") || !path.is_file() {
            return;
        }
        match load_theme_file(path) {
            Ok(theme) => self.upsert(ThemeEntry {
                name: theme.name.clone(),
                source,
                path: Some(path.to_path_buf()),
                theme,
            }),
            Err(e) => tracing::warn!("skip theme {}: {e:#}", path.display()),
        }
    }

    fn upsert(&mut self, entry: ThemeEntry) {
        if let Some(i) = self
            .entries
            .iter()
            .position(|e| e.name.eq_ignore_ascii_case(&entry.name))
        {
            self.entries[i] = entry;
        } else {
            self.entries.push(entry);
        }
    }

    pub fn get(&self, name: &str) -> Option<&ThemeEntry> {
        self.entries
            .iter()
            .rev()
            .find(|e| e.name.eq_ignore_ascii_case(name.trim()))
    }

    pub fn resolve(&self, name: &str) -> Theme {
        self.get(name)
            .map(|e| e.theme.clone())
            .unwrap_or_else(|| Theme::from_name(name))
    }

    /// 禁用的自定义主题不参与解析（内建 dark/light 始终可用）。
    pub fn resolve_filtered(&self, name: &str, filter: &rupi_config::ResourceFilter) -> Theme {
        if let Some(e) = self.get(name) {
            if e.source == "builtin" || filter.allows(&e.name) {
                return e.theme.clone();
            }
        }
        Theme::from_name(name)
    }

    pub fn names(&self) -> Vec<String> {
        self.entries.iter().map(|e| e.name.clone()).collect()
    }
}

fn source_of(dir: &Path, home: &Path, user_home: &Path) -> &'static str {
    if dir.starts_with(home) {
        "user"
    } else if dir.starts_with(&user_home.join(".pi")) {
        "user"
    } else if dir
        .file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|n| n == "builtin")
    {
        "builtin"
    } else {
        "project"
    }
}

fn theme_search_dirs(
    home: &Path,
    user_home: &Path,
    cwd: &Path,
    load_project: bool,
) -> Vec<PathBuf> {
    let mut dirs = vec![
        builtin_themes_dir(),
        home.join("themes"),
        user_home.join(".pi/agent/themes"),
    ];
    if load_project {
        let mut dir = Some(cwd.to_path_buf());
        while let Some(d) = dir {
            dirs.push(d.join(".pi/themes"));
            dirs.push(d.join(".rupi/themes"));
            if d.join(".git").exists() {
                break;
            }
            dir = d.parent().map(Path::to_path_buf);
        }
    }
    dirs
}

/// 内建 JSON：从 exe 向上找 `themes/builtin`（与 skills/builtin 同款）。
pub fn builtin_themes_dir() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent().map(|p| p.to_path_buf());
        for _ in 0..5 {
            match dir {
                Some(d) => {
                    let cand = d.join("themes/builtin");
                    if cand.is_dir() {
                        return cand;
                    }
                    dir = d.parent().map(|p| p.to_path_buf());
                }
                None => break,
            }
        }
    }
    PathBuf::from("themes/builtin")
}

fn expand_user_path(p: &Path) -> PathBuf {
    let raw = p.to_string_lossy();
    if raw == "~" {
        return std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(rest);
    }
    p.to_path_buf()
}

fn load_theme_file(path: &Path) -> anyhow::Result<Theme> {
    let raw = std::fs::read_to_string(path)?;
    let v: Value = serde_json::from_str(raw.trim_start_matches('\u{FEFF}'))?;
    let name = v
        .get("name")
        .and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.contains('/'))
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("theme")
                .to_string()
        });
    let mut vars: HashMap<String, Color> = HashMap::new();
    if let Some(obj) = v.get("vars").and_then(|x| x.as_object()) {
        for (k, val) in obj {
            if let Some(c) = parse_color_value(val, &vars, 0) {
                vars.insert(k.clone(), c);
            }
        }
    }
    let mut colors: HashMap<String, Color> = HashMap::new();
    if let Some(obj) = v.get("colors").and_then(|x| x.as_object()) {
        for (k, val) in obj {
            if let Some(c) = parse_color_value(val, &vars, 0) {
                colors.insert(k.clone(), c);
            }
        }
    }
    Ok(apply_colors(&name, colors))
}

fn apply_colors(name: &str, colors: HashMap<String, Color>) -> Theme {
    let mut theme = if name.eq_ignore_ascii_case("light") {
        Theme::light()
    } else {
        Theme::dark()
    };
    theme.name = name.to_string();
    let pick = |keys: &[&str]| {
        keys.iter().find_map(|k| {
            colors.get(*k).copied().and_then(|c| {
                if matches!(c, Color::Reset) {
                    None
                } else {
                    Some(c)
                }
            })
        })
    };
    if let Some(c) = pick(&["user", "userMessageText"]) {
        theme.user = c;
    }
    if let Some(c) = pick(&["assistant", "text"]) {
        theme.assistant = c;
    }
    if let Some(c) = pick(&["tool", "toolTitle", "warning"]) {
        theme.tool = c;
    }
    if let Some(c) = pick(&["system", "muted", "dim"]) {
        theme.system = c;
    }
    if let Some(c) = pick(&["thinking", "thinkingText"]) {
        theme.thinking = c;
    }
    if let Some(c) = pick(&["prompt", "accent", "bashMode"]) {
        theme.prompt = c;
    }
    if let Some(c) = pick(&["border"]) {
        theme.border = c;
    }
    if let Some(c) = pick(&["footer", "dim", "muted"]) {
        theme.footer = c;
    }
    if let Some(c) = pick(&["heading", "mdHeading"]) {
        theme.heading = c;
    }
    if let Some(c) = pick(&["code", "mdCode"]) {
        theme.code = c;
    }
    if let Some(c) = pick(&["accent"]) {
        theme.accent = c;
    }
    if let Some(c) = pick(&["syntaxComment", "syntax_comment"]) {
        theme.syntax_comment = c;
    }
    if let Some(c) = pick(&["syntaxKeyword", "syntax_keyword"]) {
        theme.syntax_keyword = c;
    }
    if let Some(c) = pick(&["syntaxFunction", "syntax_function"]) {
        theme.syntax_function = c;
    }
    if let Some(c) = pick(&["syntaxVariable", "syntax_variable"]) {
        theme.syntax_variable = c;
    }
    if let Some(c) = pick(&["syntaxString", "syntax_string"]) {
        theme.syntax_string = c;
    }
    if let Some(c) = pick(&["syntaxNumber", "syntax_number"]) {
        theme.syntax_number = c;
    }
    if let Some(c) = pick(&["syntaxType", "syntax_type"]) {
        theme.syntax_type = c;
    }
    if let Some(c) = pick(&["syntaxOperator", "syntax_operator"]) {
        theme.syntax_operator = c;
    }
    if let Some(c) = pick(&["syntaxPunctuation", "syntax_punctuation"]) {
        theme.syntax_punctuation = c;
    }
    theme.id = theme_id(name, &theme);
    theme
}

fn theme_id(name: &str, theme: &Theme) -> u64 {
    if name == "dark" && theme.user == Color::Cyan {
        return 0;
    }
    if name == "light" && theme.user == Color::Blue {
        return 1;
    }
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut h);
    color_key(theme.user).hash(&mut h);
    color_key(theme.accent).hash(&mut h);
    color_key(theme.assistant).hash(&mut h);
    color_key(theme.heading).hash(&mut h);
    let id = h.finish();
    if id < 2 {
        id.wrapping_add(2)
    } else {
        id
    }
}

fn color_key(c: Color) -> u32 {
    match c {
        Color::Rgb(r, g, b) => {
            0x0100_0000 | (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
        }
        Color::Indexed(i) => 0x0200_0000 | u32::from(i),
        other => format!("{other:?}")
            .bytes()
            .fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(u32::from(b))),
    }
}

fn parse_color_value(v: &Value, vars: &HashMap<String, Color>, depth: u8) -> Option<Color> {
    if depth > 8 {
        return None;
    }
    match v {
        Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                return Some(Color::Reset);
            }
            if let Some(c) = parse_hex(s) {
                return Some(c);
            }
            if let Ok(n) = s.parse::<u8>() {
                return Some(Color::Indexed(n));
            }
            vars.get(s).copied()
        }
        Value::Number(n) => n
            .as_u64()
            .and_then(|x| u8::try_from(x).ok())
            .map(Color::Indexed),
        _ => None,
    }
}

fn parse_hex(s: &str) -> Option<Color> {
    let h = s.strip_prefix('#')?;
    if h.len() == 3 {
        let r = u8::from_str_radix(&h[0..1].repeat(2), 16).ok()?;
        let g = u8::from_str_radix(&h[1..2].repeat(2), 16).ok()?;
        let b = u8::from_str_radix(&h[2..3].repeat(2), 16).ok()?;
        return Some(Color::Rgb(r, g, b));
    }
    if h.len() == 6 {
        let r = u8::from_str_radix(&h[0..2], 16).ok()?;
        let g = u8::from_str_radix(&h[2..4], 16).ok()?;
        let b = u8::from_str_radix(&h[4..6], 16).ok()?;
        return Some(Color::Rgb(r, g, b));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_differ() {
        assert_ne!(Theme::dark().id, Theme::light().id);
    }

    #[test]
    fn from_name_light_and_unknown() {
        assert_eq!(Theme::from_name("light").id, Theme::light().id);
        assert_eq!(Theme::from_name("LIGHT").name, "light");
        assert_eq!(Theme::from_name("nope").name, "dark");
        assert_eq!(Theme::from_name("").name, "dark");
    }

    #[test]
    fn json_theme_vars_and_discovery() {
        let base =
            std::env::temp_dir().join(format!("rupi-theme-{}-{}", std::process::id(), line!()));
        let _ = std::fs::remove_dir_all(&base);
        let themes = base.join("themes");
        std::fs::create_dir_all(&themes).unwrap();
        std::fs::write(
            themes.join("neon.json"),
            r##"{
              "name": "neon",
              "vars": { "pink": "#ff00aa" },
              "colors": {
                "accent": "pink",
                "user": "#00ffff",
                "text": "#eeeeee",
                "syntaxKeyword": 39
              }
            }"##,
        )
        .unwrap();
        let cat = ThemeCatalog::discover(&base, &base, true, None);
        let names = cat.names();
        assert!(names.iter().any(|n| n == "dark"), "{names:?}");
        assert!(names.iter().any(|n| n == "light"), "{names:?}");
        assert!(names.iter().any(|n| n == "neon"), "{names:?}");
        let neon = cat.resolve("neon");
        assert_eq!(neon.name, "neon");
        assert_eq!(neon.accent, Color::Rgb(255, 0, 170));
        assert_eq!(neon.user, Color::Rgb(0, 255, 255));
        assert_eq!(neon.assistant, Color::Rgb(238, 238, 238));
        assert_eq!(neon.syntax_keyword, Color::Indexed(39));
        assert_ne!(neon.id, Theme::dark().id);

        let denied = rupi_config::ResourceFilter::from_specs(Some(&["!neon".into()]));
        let fallback = cat.resolve_filtered("neon", &denied);
        assert_eq!(fallback.name, "dark");

        let applied = Theme::resolve_at("neon", &base, &base, true, None);
        assert_eq!(applied.name, "neon");
        let _ = std::fs::remove_dir_all(&base);
    }
}
