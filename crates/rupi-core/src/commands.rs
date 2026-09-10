//! 自定义斜杠命令：`commands/*.md` 即 `/name args`（对标 Claude Code slash commands）。
//!
//! 文件即命令：`<name>.md` 正文为提示模板，`$ARGUMENTS` 替换为用户参数；
//! 无占位符则把参数拼到末尾。可选 YAML frontmatter（description 等）只做元信息，解析时剥离。
//! 内建命令（/quit、/tree 等）优先；未知 `/foo` 先查自定义命令，查不到才当普通消息发送。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 扫描目录集，返回 命令名 → 文件。先扫描者胜（用户级覆盖项目级请自行排序）。
pub fn discover(dirs: &[PathBuf]) -> HashMap<String, PathBuf> {
    let mut out = HashMap::new();
    for base in dirs {
        let Ok(entries) = std::fs::read_dir(base) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                let name = stem.to_lowercase();
                if is_valid_name(&name) {
                    out.entry(name).or_insert(p);
                }
            }
        }
    }
    out
}

fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// 解析 `/name args`：返回 (name, args)。含 `/` 的首 token（如文件路径）返回 None。
pub fn split(input: &str) -> Option<(&str, &str)> {
    let rest = input.strip_prefix('/')?;
    if rest.is_empty() {
        return None;
    }
    let mut it = rest.splitn(2, char::is_whitespace);
    let name = it.next().unwrap_or("");
    if name.is_empty() || name.contains('/') {
        return None;
    }
    let args = it.next().unwrap_or("").trim();
    Some((name, args))
}

/// 展开命令：读文件、剥 frontmatter、替换 `$ARGUMENTS`。文件缺失/非法返回 None。
pub fn expand(dirs: &[PathBuf], name: &str, args: &str) -> Option<String> {
    let table = discover(dirs);
    let path = table.get(&name.to_lowercase())?;
    expand_file(path, args)
}

fn expand_file(path: &Path, args: &str) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let body = strip_frontmatter(&raw).trim().to_string();
    if body.is_empty() {
        return None;
    }
    if body.contains("$ARGUMENTS") {
        Some(body.replace("$ARGUMENTS", args))
    } else if args.is_empty() {
        Some(body)
    } else {
        Some(format!("{body}\n\n{args}"))
    }
}

/// 剥可选 YAML frontmatter（`---` 开头到下一个 `---`），无则原文。
fn strip_frontmatter(raw: &str) -> &str {
    let t = raw.trim_start();
    if !t.starts_with("---") {
        return raw;
    }
    let mut parts = t.splitn(3, "---");
    parts.next();
    parts.next(); // front
    match parts.next() {
        Some(body) => body,
        None => raw,
    }
}

/// 命令目录：用户级 + 项目级（cwd 下 `.rupi/commands`）。
pub fn command_dirs(home: &Path) -> Vec<PathBuf> {
    vec![home.join("commands"), PathBuf::from(".rupi/commands")]
}

/// 列出命令：按名称排序的 (name, description)。
/// description 取 frontmatter `description:`，无则取正文首个非空行（压单行、截 80 字符）。
pub fn list(dirs: &[PathBuf]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = discover(dirs)
        .iter()
        .map(|(name, path)| (name.clone(), describe_file(path)))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// 渲染 `/commands` 列表块：无命令时给一句提示（含目录指引）。
pub fn index_block(dirs: &[PathBuf]) -> String {
    let items = list(dirs);
    if items.is_empty() {
        return "no custom commands. drop `<name>.md` into one of:\n".to_owned()
            + &dirs
                .iter()
                .map(|d| format!("  - {}", d.display()))
                .collect::<Vec<_>>()
                .join("\n");
    }
    let mut s = String::from("custom commands:");
    for (name, desc) in items {
        s.push_str(&format!("\n  /{name}  {desc}"));
    }
    s
}

fn describe_file(path: &Path) -> String {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return String::new();
    };
    if let Some(d) = frontmatter_description(&raw) {
        return one_line(&d, 80);
    }
    let body = strip_frontmatter(&raw);
    let first = body.lines().map(str::trim).find(|l| !l.is_empty());
    one_line(first.unwrap_or(""), 80)
}

/// 从可选 YAML frontmatter 取 `description:` 值（单行 `key: value` 解析，不引入 YAML 依赖）。
fn frontmatter_description(raw: &str) -> Option<String> {
    let t = raw.trim_start();
    let rest = t.strip_prefix("---")?;
    let end = rest.find("---")?;
    rest[..end].lines().find_map(|l| {
        let l = l.trim();
        let v = l.strip_prefix("description")?.trim_start();
        let v = v.strip_prefix(':')?.trim();
        let v = v.trim_matches(|c| c == '"' || c == '\'');
        if v.is_empty() {
            None
        } else {
            Some(v.to_owned())
        }
    })
}

fn one_line(s: &str, limit: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= limit {
        flat
    } else {
        let cut: String = flat.chars().take(limit - 1).collect();
        format!("{cut}…")
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
    fn split_rejects_paths_and_bare_slash() {
        assert_eq!(split("/fix typo"), Some(("fix", "typo")));
        assert_eq!(split("/fix"), Some(("fix", "")));
        assert_eq!(split("/tmp/x"), None);
        assert_eq!(split("/"), None);
        assert_eq!(split("no slash"), None);
    }

    #[test]
    fn expand_substitutes_arguments_and_strips_frontmatter() {
        let base = std::env::temp_dir().join(format!("rupi-cmd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        write(
            &base,
            "fix.md",
            "---\ndescription: fix it\n---\n\nFix this: $ARGUMENTS\n",
        );
        write(&base, "review.md", "Review the diff carefully.\n");
        write(&base, "empty.md", "---\ndescription: x\n---\n");
        let dirs = vec![base.clone()];
        assert_eq!(
            expand(&dirs, "fix", "null pointer").unwrap(),
            "Fix this: null pointer"
        );
        assert_eq!(
            expand(&dirs, "review", "all").unwrap(),
            "Review the diff carefully.\n\nall"
        );
        assert_eq!(
            expand(&dirs, "review", "").unwrap(),
            "Review the diff carefully."
        );
        assert!(expand(&dirs, "empty", "").is_none());
        assert!(expand(&dirs, "missing", "").is_none());
        // 非法文件名不收录
        write(&base, "Bad Name.md", "x");
        assert!(!discover(&dirs).contains_key("bad name"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn list_prefers_frontmatter_description_and_sorts() {
        let base = std::env::temp_dir().join(format!("rupi-cmd-list-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        write(
            &base,
            "zebra.md",
            "---\ndescription: \"strip quotes\"\n---\n\nZebra body.\n",
        );
        write(&base, "apple.md", "\n\nFirst line here.\nSecond.\n");
        let dirs = vec![base.clone()];
        let items = list(&dirs);
        assert_eq!(
            items,
            vec![
                ("apple".to_owned(), "First line here.".to_owned()),
                ("zebra".to_owned(), "strip quotes".to_owned()),
            ]
        );
        let block = index_block(&dirs);
        assert!(block.contains("/apple") && block.contains("/zebra"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn index_block_empty_dirs_hints_paths() {
        let block = index_block(&[PathBuf::from("/nonexistent-rupi-cmd")]);
        assert!(block.contains("no custom commands"));
    }
}
