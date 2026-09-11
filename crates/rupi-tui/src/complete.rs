//! TUI 斜杠补全：纯函数（可单测），对标 Pi TUI 的 `/` 命令提示。
//!
//! 候选 = 内建命令（与 REPL 同构）+ 自定义 `commands/*.md`。
//! 只在补全首 token 时生效（含 `/` 的路径等不触发，由 `commands::split` 语义保证一致）。
//!
//! 另有 `@` 路径补全（与 `rupi_core::commands::expand_at_mentions` 同口径，
//! 同沙箱规则）：`@` 前须行首/空白，只补 root 内相对路径，目录候选带尾 `/`。

use std::path::Path;

/// 与 REPL 同构的内建斜杠命令（`/quit` 单独处理，此处仅用于提示）。
pub const BUILTINS: &[&str] = &[
    "commands", "compact", "goto", "model", "plan", "quit", "reload", "resume", "rewind",
    "sessions", "skills", "thinking", "tree",
];

/// 计算候选：`input` 以 `/` 开头且首 token 无空白时，按前缀过滤并排序去重；否则空。
pub fn candidates(input: &str, customs: &[String]) -> Vec<String> {
    let rest = match input.strip_prefix('/') {
        Some(r) if !r.is_empty() => r,
        _ => return Vec::new(),
    };
    if rest.contains(char::is_whitespace) || rest.contains('/') {
        return Vec::new();
    }
    let mut out: Vec<String> = BUILTINS
        .iter()
        .filter(|b| b.starts_with(rest))
        .map(|b| b.to_string())
        .chain(customs.iter().filter(|c| c.starts_with(rest)).cloned())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// 候选公共前缀（长于当前已输入部分才有补全价值，调用方判断）。
pub fn common_prefix(cs: &[String]) -> String {
    let mut it = cs.iter();
    let Some(first) = it.next() else {
        return String::new();
    };
    let mut len = first.len();
    for s in it {
        len = first
            .char_indices()
            .zip(s.char_indices())
            .take_while(|((_, a), (_, b))| a == b)
            .count();
        if len == 0 {
            break;
        }
    }
    first.chars().take(len).collect()
}

/// Tab 应用：单候选补全名 + 空格；多候选补公共前缀（无扩展则不动，靠弹窗展示）。
/// 返回新输入（调用方写回 `InputBuffer`），无候选返回 `None`。
pub fn apply_tab(input: &str, customs: &[String]) -> Option<String> {
    let cs = candidates(input, customs);
    match cs.as_slice() {
        [] => None,
        [one] => Some(format!("/{one} ")),
        many => {
            let prefix = common_prefix(many);
            let typed = &input[1..];
            if prefix.len() > typed.len() {
                Some(format!("/{prefix}"))
            } else {
                None
            }
        }
    }
}

/// 弹窗/单轮扫描上限：大目录只取前 N 个（排序后截断，输出稳定）。
const AT_MAX_CANDIDATES: usize = 100;

/// 光标所在的 @token 内容区间 [start, end)（字符位，不含 `@`，end == cursor）。
/// 从光标往回扫到 `@`（中间无空白，`@` 前为行首/空白）；邮件地址（@ 前非空白）
/// 与无 `@` 返回 `None`。空 token（`@` 后即光标）也命中，意为列根目录。
pub fn at_token_span(text: &str, cursor: usize) -> Option<(usize, usize)> {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    let mut at = None;
    for idx in (0..cursor).rev() {
        let c = chars[idx];
        if c.is_whitespace() {
            break;
        }
        if c == '@' {
            if idx == 0 || chars[idx - 1].is_whitespace() {
                at = Some(idx);
            }
            break;
        }
    }
    at.map(|a| (a + 1, cursor))
}

/// @路径候选：token 按最后 `/` 拆父目录 + 前缀，列父目录条目并按前缀过滤。
/// 返回相对 root 的完整相对路径（目录带尾 `/`），排序截断。越界/非法/不可读
/// 一律空（静默；发送时展开侧同样保留原文，用户可改走 read 工具）。
pub fn at_candidates(text: &str, cursor: usize, root: &Path) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    let Some((start, _)) = at_token_span(text, cursor) else {
        return Vec::new();
    };
    let token: String = chars[start..cursor].iter().collect();
    if token.starts_with('/') || token.contains('\0') {
        return Vec::new();
    }
    let (parent, prefix) = match token.rfind('/') {
        Some(i) => (token[..i].to_string(), token[i + 1..].to_string()),
        None => (String::new(), token.clone()),
    };
    let parent_path = Path::new(&parent);
    if parent_path.is_absolute()
        || parent_path
            .components()
            .any(|c| c == std::path::Component::ParentDir)
    {
        return Vec::new();
    }
    let root_canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let dir_canon = match root_canon.join(parent_path).canonicalize() {
        Ok(p) if p.starts_with(&root_canon) => p,
        _ => return Vec::new(),
    };
    let entries = match std::fs::read_dir(&dir_canon) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut out: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(&prefix) {
            continue;
        }
        let rel = if parent.is_empty() {
            name.clone()
        } else {
            format!("{parent}/{name}")
        };
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        out.push(if is_dir { format!("{rel}/") } else { rel });
    }
    out.sort();
    out.dedup();
    out.truncate(AT_MAX_CANDIDATES);
    out
}

/// Tab 应用 @补全：单候选替换 token（文件追空格、目录不追，方便继续深入）；
/// 多候选补公共前缀（无扩展不动，靠弹窗展示）。返回 (新全文, 新光标字符位)。
pub fn apply_at_tab(text: &str, cursor: usize, root: &Path) -> Option<(String, usize)> {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    let (start, _) = at_token_span(text, cursor)?;
    let cs = at_candidates(text, cursor, root);
    match cs.as_slice() {
        [] => None,
        [one] => {
            let mut new: String = chars[..start].iter().collect();
            new.push_str(one);
            let mut new_cursor = start + one.chars().count();
            if !one.ends_with('/') {
                new.push(' ');
                new_cursor += 1;
            }
            new.extend(chars[cursor..].iter());
            Some((new, new_cursor))
        }
        many => {
            let prefix = common_prefix(many);
            let token: String = chars[start..cursor].iter().collect();
            if prefix.len() > token.len() {
                let mut new: String = chars[..start].iter().collect();
                new.push_str(&prefix);
                new.extend(chars[cursor..].iter());
                Some((new, start + prefix.chars().count()))
            } else {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn customs() -> Vec<String> {
        vec!["fix".into(), "format".into(), "review".into()]
    }

    #[test]
    fn prefix_filters_builtins_and_customs() {
        assert_eq!(candidates("/sk", &[]), vec!["skills".to_string()]);
        assert_eq!(candidates("/th", &[]), vec!["thinking".to_string()]);
        assert_eq!(candidates("/co", &[]), vec!["commands".to_string(), "compact".to_string()]);
        assert_eq!(candidates("/sess", &[]), vec!["sessions".to_string()]);
        assert_eq!(candidates("/res", &[]), vec!["resume".to_string()]);
        assert_eq!(
            candidates("/f", &customs()),
            vec!["fix".to_string(), "format".to_string()]
        );
        // 自定义与内建同名去重
        let mut c = customs();
        c.push("tree".into());
        assert_eq!(candidates("/tree", &c), vec!["tree".to_string()]);
    }

    #[test]
    fn non_command_inputs_yield_nothing() {
        assert!(candidates("", &customs()).is_empty());
        assert!(candidates("/", &customs()).is_empty());
        assert!(candidates("no slash", &customs()).is_empty());
        assert!(candidates("/fix args here", &customs()).is_empty());
        assert!(candidates("/tmp/x", &customs()).is_empty());
    }

    #[test]
    fn tab_applies_single_or_common_prefix() {
        assert_eq!(apply_tab("/sk", &[]).as_deref(), Some("/skills "));
        assert_eq!(apply_tab("/r", &customs()).as_deref(), Some("/re"));
        // 无公共扩展 → None（弹窗展示候选）
        assert_eq!(apply_tab("/f", &customs()), None);
        assert_eq!(apply_tab("/quit", &[]).as_deref(), Some("/quit "));
        assert_eq!(apply_tab("hi", &customs()), None);
    }

    fn at_root(tag: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!("rupi-at-tab-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn at_seed(base: &std::path::Path) {
        std::fs::write(base.join("hello.txt"), "hi").unwrap();
        std::fs::write(base.join("help.md"), "help").unwrap();
        let src = base.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("main.rs"), "fn main(){}").unwrap();
    }

    #[test]
    fn at_token_span_hits_mention_not_email() {
        let text = "看下 @hello.txt 好";
        // 光标在 token 末尾（字符位 9：「看下 @hello.txt」共 9 字符）
        let end = text.chars().count() - " 好".chars().count();
        assert_eq!(at_token_span(text, end), Some((4, end)));
        // 邮件地址不命中
        assert_eq!(at_token_span("联系 foo@bar.com", 100), None);
        // 无 @ 不命中
        assert_eq!(at_token_span("plain text", 100), None);
        // 空 token（@ 后即光标）命中，意为列根
        assert_eq!(at_token_span("读 @", 3), Some((3, 3)));
        // 光标钳制越界
        assert_eq!(at_token_span("读 @", 99), Some((3, 3)));
    }

    #[test]
    fn at_candidates_lists_files_and_subdirs() {
        let base = at_root("list");
        at_seed(&base);
        assert_eq!(
            at_candidates("看 @he", 5, &base),
            vec!["hello.txt".to_string(), "help.md".to_string()]
        );
        // 子目录深入：token 含 / 时列父目录
        assert_eq!(
            at_candidates("看 @src/ma", 9, &base),
            vec!["src/main.rs".to_string()]
        );
        // 空 token 列根：文件 + 目录（带尾 /）
        let root_list = at_candidates("@", 1, &base);
        assert!(
            root_list.contains(&"hello.txt".to_string()),
            "{root_list:?}"
        );
        assert!(root_list.contains(&"src/".to_string()), "{root_list:?}");
        // 越界 / 绝对路径 / 邮件一律空
        assert!(at_candidates("读 @../x", 7, &base).is_empty());
        assert!(at_candidates("读 @/abs", 7, &base).is_empty());
        assert!(at_candidates("a@b", 3, &base).is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn apply_at_tab_replaces_token_and_places_cursor() {
        let base = at_root("apply");
        at_seed(&base);
        // 多候选 → 公共前缀扩展（@he → @hel）
        assert_eq!(
            apply_at_tab("看 @he", 5, &base),
            Some(("看 @hel".to_string(), 6))
        );
        // 单候选文件 → 追空格
        assert_eq!(
            apply_at_tab("看 @hello", 8, &base),
            Some(("看 @hello.txt ".to_string(), 13))
        );
        // 单候选目录 → 尾 / 不追空格，方便继续深入
        assert_eq!(
            apply_at_tab("看 @sr", 5, &base),
            Some(("看 @src/".to_string(), 7))
        );
        // 公共前缀无扩展 → None（弹窗展示）
        assert_eq!(apply_at_tab("看 @hel", 6, &base), None);
        let _ = std::fs::remove_dir_all(&base);
    }
}
