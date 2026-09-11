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

/// `@path` 引用展开：用户消息里的 `@相对路径` 内联文件内容（对标 Pi 的 @ 附件）。
/// 规则：`@` 前须是行首/空白（邮件地址不误伤）；路径取到空白或行尾，剥尾部标点
/// `,.;:)]}!?`；只收 root 内的普通文件（canonical 校验，与 read 沙箱同口径），
/// 目录/越界/不存在/读失败一律保留原文（静默，用户可改走 read 工具）。
/// 超 `AT_MAX_BYTES`（64K）截断并标注；展开为围栏块，模型可定位来源。
/// 图片（png/jpg/gif/webp/bmp/svg）：先 `metadata` 判大小，再整文件读成
/// [`crate::ContentBlock::Image`]（上限 [`AT_MAX_IMAGE_BYTES`]），不走文本围栏。
pub const AT_MAX_BYTES: u64 = 64 * 1024;
/// @ 附件图片上限：先 metadata 再读，超限保留原文（改走 read 工具）。
pub const AT_MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

/// 纯文本展开（测试与不关心图片块的调用方）。图片写成 `[image mime]` 占位。
pub fn expand_at_mentions(text: &str, root: &Path) -> String {
    crate::Message::from_blocks(crate::Role::User, expand_at_mentions_blocks(text, root)).full_text()
}

/// 图文混排展开：文本围栏 + 图片块按原文顺序交错。
pub fn expand_at_mentions_blocks(text: &str, root: &Path) -> Vec<crate::ContentBlock> {
    let root_canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let bytes = text.as_bytes();
    let mut blocks = Vec::new();
    let mut text_buf = String::with_capacity(text.len());
    let mut i = 0;
    let flush_text = |buf: &mut String, blocks: &mut Vec<crate::ContentBlock>| {
        if !buf.is_empty() {
            blocks.push(crate::ContentBlock::Text {
                text: std::mem::take(buf),
            });
        }
    };
    while i < bytes.len() {
        if bytes[i] == b'@'
            && (i == 0 || bytes[i - 1].is_ascii_whitespace())
            && i + 1 < bytes.len()
            && !bytes[i + 1].is_ascii_whitespace()
        {
            let mut j = i + 1;
            while j < bytes.len() && !bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            let mut raw = &text[i + 1..j];
            raw = raw.trim_end_matches([',', '.', ';', ':', ')', ']', '}', '!', '?']);
            let tail = &text[i + 1 + raw.len()..j];
            if let Some(part) = read_at_file(raw, &root_canon) {
                match part {
                    AtPart::Text(s) => {
                        text_buf.push_str(&s);
                        text_buf.push_str(tail);
                    }
                    AtPart::Image {
                        caption,
                        media_type,
                        data,
                    } => {
                        text_buf.push_str(&caption);
                        text_buf.push_str(tail);
                        flush_text(&mut text_buf, &mut blocks);
                        blocks.push(crate::ContentBlock::Image { media_type, data });
                    }
                }
                i = j;
                continue;
            }
        }
        let ch = text[i..].chars().next().expect("非空剩余必有字符");
        text_buf.push(ch);
        i += ch.len_utf8();
    }
    flush_text(&mut text_buf, &mut blocks);
    if blocks.is_empty() {
        blocks.push(crate::ContentBlock::Text {
            text: String::new(),
        });
    }
    blocks
}

enum AtPart {
    Text(String),
    Image {
        caption: String,
        media_type: String,
        data: String,
    },
}

fn read_at_file(rel: &str, root_canon: &Path) -> Option<AtPart> {
    if rel.is_empty() || rel.contains('\0') {
        return None;
    }
    let p = Path::new(rel);
    if p.is_absolute() || p.components().any(|c| c == std::path::Component::ParentDir) {
        return None;
    }
    let full = root_canon.join(p);
    let meta = std::fs::symlink_metadata(&full).ok()?;
    if !meta.is_file() {
        return None;
    }
    let canon = full.canonicalize().ok()?;
    if !canon.starts_with(root_canon) {
        return None;
    }
    if let Some(media) = crate::image_media_type(&canon) {
        if meta.len() > AT_MAX_IMAGE_BYTES {
            return Some(AtPart::Text(format!(
                "`@{rel}` 是图片（{media}，{} bytes，超过 @ 附件上限 {}）——请用 read 工具",
                meta.len(),
                AT_MAX_IMAGE_BYTES
            )));
        }
        let bytes = std::fs::read(&canon).ok()?;
        return Some(AtPart::Image {
            caption: format!("`@{rel}` 的图片（{media}，{} bytes）：\n", bytes.len()),
            media_type: media.to_string(),
            data: crate::encode_base64(&bytes),
        });
    }
    let truncated = meta.len() > AT_MAX_BYTES;
    let file = std::fs::File::open(&canon).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut buf = vec![0u8; AT_MAX_BYTES as usize];
    let n = std::io::Read::read(&mut reader, &mut buf).ok()?;
    buf.truncate(n);
    let body = String::from_utf8_lossy(&buf).into_owned();
    Some(AtPart::Text(if truncated {
        format!("`@{rel}` 的内容（已截断前 64K）：\n```\n{body}\n```")
    } else {
        format!("`@{rel}` 的内容：\n```\n{body}\n```")
    }))
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

    fn at_root(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("rupi-at-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn at_mention_expands_file_content() {
        let base = at_root("hit");
        write(&base, "hello.txt", "world");
        let out = expand_at_mentions("看下 @hello.txt", &base);
        assert!(out.contains("`@hello.txt` 的内容"), "展开块缺来源：{out}");
        assert!(out.contains("world"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn at_mention_ignores_email_and_keeps_unresolvable() {
        let base = at_root("miss");
        write(&base, "hello.txt", "world");
        // 邮件地址 @ 前非空白，不误伤
        assert_eq!(
            expand_at_mentions("联系 foo@bar.com", &base),
            "联系 foo@bar.com"
        );
        // 不存在 / 越界 / 绝对路径 / 目录一律保留原文
        assert_eq!(
            expand_at_mentions("读 @missing.txt", &base),
            "读 @missing.txt"
        );
        assert_eq!(
            expand_at_mentions("读 @../secret", &base),
            "读 @../secret"
        );
        assert_eq!(expand_at_mentions("读 @/abs", &base), "读 @/abs");
        std::fs::create_dir_all(base.join("sub")).unwrap();
        assert_eq!(expand_at_mentions("读 @sub", &base), "读 @sub");
        // 中文透传不损坏
        assert_eq!(
            expand_at_mentions("你好世界", &base),
            "你好世界"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn at_mention_strips_trailing_punct_and_truncates_large() {
        let base = at_root("edge");
        write(&base, "hello.txt", "world");
        // 尾标点剥离后展开，标点保留原文位置
        let out = expand_at_mentions("看 @hello.txt, 好", &base);
        assert!(out.contains("world") && out.ends_with(", 好"), "{out}");
        // 超 64K 截断并标注
        let big = "x".repeat(AT_MAX_BYTES as usize + 10);
        write(&base, "big.txt", &big);
        let out = expand_at_mentions("@big.txt", &base);
        assert!(out.contains("已截断前 64K"), "{out}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn at_mention_image_becomes_image_block() {
        let base = at_root("img");
        // 最小 PNG 头 + 一点载荷，足够走图片分支（不校验解码）
        let png = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3, 4];
        std::fs::write(base.join("pic.png"), png).unwrap();
        let blocks = expand_at_mentions_blocks("看 @pic.png", &base);
        assert!(
            blocks.iter().any(|b| matches!(
                b,
                crate::ContentBlock::Image {
                    media_type,
                    data
                } if media_type == "image/png" && !data.is_empty()
            )),
            "{blocks:?}"
        );
        let text = expand_at_mentions("看 @pic.png", &base);
        assert!(text.contains("[image image/png]"), "{text}");
        let _ = std::fs::remove_dir_all(&base);
    }
}
