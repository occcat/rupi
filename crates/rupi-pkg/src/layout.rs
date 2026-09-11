//! 扫描 Pi 包布局：`package.json#pi` 或 `skills/` `prompts/` `commands/` `extensions/`。

use serde_json::Value;
use std::path::{Path, PathBuf};

/// 包内待安装资源。
#[derive(Debug, Default, Clone)]
pub struct PackageResources {
    pub skills: Vec<PathBuf>,
    pub commands: Vec<PathBuf>,
    pub extensions: Vec<PathBuf>,
    pub skipped_ts_extensions: usize,
}

impl PackageResources {
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty() && self.commands.is_empty() && self.extensions.is_empty()
    }
}

/// 只装一类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    Auto,
    Skill,
    Command,
    Extension,
}

impl ResourceKind {
    pub fn parse(s: Option<&str>) -> anyhow::Result<Self> {
        match s.map(str::trim).filter(|x| !x.is_empty()) {
            None => Ok(Self::Auto),
            Some("skill") | Some("skills") => Ok(Self::Skill),
            Some("command") | Some("commands") | Some("prompt") | Some("prompts") => {
                Ok(Self::Command)
            }
            Some("extension") | Some("ext") | Some("extensions") => Ok(Self::Extension),
            Some(other) => anyhow::bail!("unknown --kind `{other}` (skill|command|extension)"),
        }
    }

    pub fn filter(self, mut r: PackageResources) -> PackageResources {
        match self {
            Self::Auto => r,
            Self::Skill => {
                r.commands.clear();
                r.extensions.clear();
                r
            }
            Self::Command => {
                r.skills.clear();
                r.extensions.clear();
                r
            }
            Self::Extension => {
                r.skills.clear();
                r.commands.clear();
                r
            }
        }
    }
}

/// 发现 `root` 下的 skill / command / extension（不执行包内脚本）。
pub fn discover(root: &Path) -> anyhow::Result<PackageResources> {
    if root.is_file() {
        return Ok(discover_file(root));
    }
    if !root.is_dir() {
        anyhow::bail!(
            "package path is not a file or directory: {}",
            root.display()
        );
    }
    let mut res = if let Some(from_pi) = discover_from_package_json(root) {
        from_pi
    } else {
        discover_convention(root)
    };
    if res.is_empty() && root.join("SKILL.md").is_file() {
        res.skills.push(root.to_path_buf());
    }
    Ok(res)
}

fn discover_file(path: &Path) -> PackageResources {
    let mut res = PackageResources::default();
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if name.eq_ignore_ascii_case("SKILL.md") {
        if let Some(parent) = path.parent() {
            res.skills.push(parent.to_path_buf());
        }
    } else if ext == "md" {
        res.commands.push(path.to_path_buf());
    } else if ext == "json" {
        res.extensions.push(path.to_path_buf());
    }
    res
}

fn discover_from_package_json(root: &Path) -> Option<PackageResources> {
    let text = std::fs::read_to_string(root.join("package.json")).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let pi = v.get("pi")?;
    let mut res = PackageResources::default();
    collect_skill_entries(root, pi.get("skills"), &mut res.skills);
    collect_md_entries(root, pi.get("prompts"), &mut res.commands);
    collect_md_entries(root, pi.get("commands"), &mut res.commands);
    collect_ext_entries(root, pi.get("extensions"), &mut res);
    if res.is_empty() && res.skipped_ts_extensions == 0 {
        return None;
    }
    Some(res)
}

fn discover_convention(root: &Path) -> PackageResources {
    let mut res = PackageResources::default();
    let skills_dir = root.join("skills");
    if skills_dir.is_dir() {
        find_skill_dirs(&skills_dir, &mut res.skills, 0);
    }
    for name in ["prompts", "commands"] {
        let dir = root.join(name);
        if dir.is_dir() {
            collect_md_dir(&dir, &mut res.commands);
        }
    }
    let ext_dir = root.join("extensions");
    if ext_dir.is_dir() {
        collect_ext_dir(&ext_dir, &mut res);
    }
    res
}

fn path_list(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|x| x.as_str())
            .filter(|s| !s.starts_with('!'))
            .map(|s| s.trim_start_matches('+').to_string())
            .collect(),
        _ => vec![],
    }
}

fn resolve_entry(root: &Path, rel: &str) -> PathBuf {
    let p = Path::new(rel);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    }
}

fn collect_skill_entries(root: &Path, v: Option<&Value>, out: &mut Vec<PathBuf>) {
    for rel in path_list(v) {
        let p = resolve_entry(root, &rel);
        if p.join("SKILL.md").is_file() {
            out.push(p);
        } else if p.is_dir() {
            find_skill_dirs(&p, out, 0);
        } else if p.file_name().and_then(|s| s.to_str()) == Some("SKILL.md") && p.is_file() {
            if let Some(parent) = p.parent() {
                out.push(parent.to_path_buf());
            }
        }
    }
}

fn collect_md_entries(root: &Path, v: Option<&Value>, out: &mut Vec<PathBuf>) {
    for rel in path_list(v) {
        let p = resolve_entry(root, &rel);
        if p.is_file() && p.extension().and_then(|s| s.to_str()) == Some("md") {
            out.push(p);
        } else if p.is_dir() {
            collect_md_dir(&p, out);
        }
    }
}

fn collect_ext_entries(root: &Path, v: Option<&Value>, res: &mut PackageResources) {
    for rel in path_list(v) {
        let p = resolve_entry(root, &rel);
        if p.is_file() {
            push_ext_file(&p, res);
        } else if p.is_dir() {
            collect_ext_dir(&p, res);
        }
    }
}

fn collect_md_dir(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("md") && p.is_file())
        .collect();
    files.sort();
    out.extend(files);
}

fn collect_ext_dir(dir: &Path, res: &mut PackageResources) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    files.sort();
    for p in files {
        if p.is_file() {
            push_ext_file(&p, res);
        }
    }
}

fn push_ext_file(p: &Path, res: &mut PackageResources) {
    match p.extension().and_then(|s| s.to_str()) {
        Some("json") => res.extensions.push(p.to_path_buf()),
        Some("ts") | Some("js") | Some("mts") | Some("cts") | Some("mjs") => {
            res.skipped_ts_extensions += 1;
        }
        _ => {}
    }
}

fn find_skill_dirs(dir: &Path, out: &mut Vec<PathBuf>, depth: u32) {
    if depth > 8 {
        return;
    }
    if dir.join("SKILL.md").is_file() {
        out.push(dir.to_path_buf());
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut kids: Vec<PathBuf> = rd
        .flatten()
        .filter(|e| {
            e.file_type().map(|t| t.is_dir()).unwrap_or(false)
                && e.file_name() != ".git"
                && e.file_name() != "node_modules"
        })
        .map(|e| e.path())
        .collect();
    kids.sort();
    for k in kids {
        find_skill_dirs(&k, out, depth + 1);
    }
}

/// 安全拷贝目录（跳过 `.git` 与符号链接，防逃逸）。
pub fn copy_tree(src: &Path, dst: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == ".git" || name == "node_modules" {
            continue;
        }
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            continue;
        }
        let to = dst.join(&name);
        if ft.is_dir() {
            copy_tree(&entry.path(), &to)?;
        } else if ft.is_file() {
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

/// skill 目录名：优先 SKILL.md frontmatter `name:`，否则目录名。
pub fn skill_install_name(dir: &Path) -> String {
    if let Ok(raw) = std::fs::read_to_string(dir.join("SKILL.md")) {
        if let Some(n) = frontmatter_field(&raw, "name") {
            if is_skill_name(&n) {
                return n;
            }
        }
    }
    sanitize_file_stem(dir.file_name().and_then(|s| s.to_str()).unwrap_or("skill"))
}

/// 命令文件名（小写 stem）。
pub fn command_install_name(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let name = stem.to_ascii_lowercase();
    if is_command_name(&name) {
        Some(name)
    } else {
        Some(sanitize_file_stem(&name))
    }
}

/// 扩展名：manifest `name` 或文件 stem。
pub fn extension_install_name(path: &Path) -> String {
    if let Ok(raw) = std::fs::read_to_string(path) {
        if let Ok(v) = serde_json::from_str::<Value>(&raw) {
            if let Some(n) = v.get("name").and_then(|x| x.as_str()) {
                if is_ext_name(n) {
                    return n.to_string();
                }
            }
        }
    }
    sanitize_file_stem(path.file_stem().and_then(|s| s.to_str()).unwrap_or("ext"))
}

fn frontmatter_field(raw: &str, key: &str) -> Option<String> {
    let t = raw.trim_start();
    let rest = t.strip_prefix("---")?;
    let end = rest.find("---")?;
    rest[..end].lines().find_map(|l| {
        let l = l.trim();
        let v = l.strip_prefix(key)?.trim_start();
        let v = v.strip_prefix(':')?.trim();
        let v = v.trim_matches(|c| c == '"' || c == '\'');
        if v.is_empty() {
            None
        } else {
            Some(v.to_owned())
        }
    })
}

fn is_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn is_command_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

fn is_ext_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

fn sanitize_file_stem(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_uppercase() {
                c.to_ascii_lowercase()
            } else if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "pkg".into()
    } else {
        out.chars().take(64).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn convention_and_manifest() {
        let base = std::env::temp_dir().join(format!("rupi-pkg-layout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        write(
            &base,
            "skills/foo/SKILL.md",
            "---\nname: foo\ndescription: d\n---\nbody\n",
        );
        write(&base, "prompts/review.md", "Review {{focus}}\n");
        write(
            &base,
            "extensions/upper.json",
            r#"{"name":"upper","description":"d","input_schema":{},"command":"true"}"#,
        );
        write(&base, "extensions/legacy.ts", "export default {}");
        let r = discover(&base).unwrap();
        assert_eq!(r.skills.len(), 1);
        assert_eq!(skill_install_name(&r.skills[0]), "foo");
        assert_eq!(r.commands.len(), 1);
        assert_eq!(r.extensions.len(), 1);
        assert_eq!(r.skipped_ts_extensions, 1);

        write(
            &base,
            "package.json",
            r#"{"name":"x","pi":{"skills":["./skills"],"prompts":["./prompts"],"extensions":[]}}"#,
        );
        let r = discover(&base).unwrap();
        assert_eq!(r.skills.len(), 1);
        assert_eq!(r.commands.len(), 1);
        assert!(r.extensions.is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn single_skill_root() {
        let base = std::env::temp_dir().join(format!("rupi-pkg-oneskill-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        write(
            &base,
            "SKILL.md",
            "---\nname: solo\ndescription: one\n---\n",
        );
        let r = discover(&base).unwrap();
        assert_eq!(r.skills.len(), 1);
        assert_eq!(skill_install_name(&r.skills[0]), "solo");
        let _ = std::fs::remove_dir_all(&base);
    }
}
