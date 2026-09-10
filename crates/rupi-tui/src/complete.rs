//! TUI 斜杠补全：纯函数（可单测），对标 Pi TUI 的 `/` 命令提示。
//!
//! 候选 = 内建命令（与 REPL 同构）+ 自定义 `commands/*.md`。
//! 只在补全首 token 时生效（含 `/` 的路径等不触发，由 `commands::split` 语义保证一致）。

/// 与 REPL 同构的内建斜杠命令（`/quit` 单独处理，此处仅用于提示）。
pub const BUILTINS: &[&str] = &[
    "commands", "compact", "goto", "model", "plan", "quit", "reload", "rewind", "skills",
    "thinking", "tree",
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
}
