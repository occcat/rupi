//! TUI / REPL 斜杠：`/copy` `/hotkeys` `/trust`（纯逻辑，可单测）。

use crate::keybindings::KeyTable;
use crate::view::Line;
use rupi_core::trust::TrustStore;
use rupi_core::{Role, SessionTree};
use std::path::Path;

/// 视图里最后一条非空助手文本。
pub fn last_assistant_from_lines(lines: &[Line]) -> Option<&str> {
    lines.iter().rev().find_map(|l| match l {
        Line::AssistantText(s) if !s.is_empty() => Some(s.as_str()),
        _ => None,
    })
}

/// 会话树里最后一条非空助手正文。
pub fn last_assistant_from_session(session: &SessionTree) -> Option<String> {
    session.history().into_iter().rev().find_map(|m| {
        if !matches!(m.role, Role::Assistant) {
            return None;
        }
        let t = m.full_text();
        if t.is_empty() {
            None
        } else {
            Some(t)
        }
    })
}

/// `/copy`：把文本写入系统剪贴板。
pub fn copy_text(text: Option<&str>) -> String {
    let Some(t) = text.filter(|s| !s.is_empty()) else {
        return "[copy] no assistant text".into();
    };
    match clipboard_set_text(t) {
        Ok(()) => format!("[copy] {} chars", t.chars().count()),
        Err(e) => format!("[copy] clipboard failed: {e}"),
    }
}

pub fn clipboard_set_text(text: &str) -> Result<(), String> {
    let attempts: &[(&str, &[&str])] = &[
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("pbcopy", &[]),
    ];
    let mut last = "no wl-copy/xclip/pbcopy".to_string();
    for (cmd, args) in attempts {
        match std::process::Command::new(cmd)
            .args(*args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                use std::io::Write as _;
                if let Some(mut stdin) = child.stdin.take() {
                    if stdin.write_all(text.as_bytes()).is_err() {
                        last = format!("{cmd}: write stdin failed");
                        let _ = child.wait();
                        continue;
                    }
                }
                match child.wait() {
                    Ok(st) if st.success() => return Ok(()),
                    Ok(st) => last = format!("{cmd} exited {st}"),
                    Err(e) => last = format!("{cmd}: {e}"),
                }
            }
            Err(e) => last = format!("{cmd}: {e}"),
        }
    }
    Err(last)
}

pub fn hotkeys_block(keys: &KeyTable) -> String {
    keys.describe()
}

/// `/trust`：展示或记住当前工作区。
pub fn trust_slash(args: &str, home: &Path, cwd: &Path, policy: &str) -> String {
    let root = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let mut store = TrustStore::open(home.join("trusted_projects"));
    match args.trim() {
        "" => format!(
            "[trust] {}\n  defaultProjectTrust  {policy}\n  remembered           {}",
            root.display(),
            if store.contains(&root) { "yes" } else { "no" }
        ),
        "always" | "remember" | "yes" | "y" => match store.add(&root) {
            Ok(()) => format!("[trust] remembered {}", root.display()),
            Err(e) => format!("[trust] remember failed: {e}"),
        },
        other => format!("[trust] usage: /trust [always] (got {other})"),
    }
}

/// 解析本模块三斜杠；非此三项返回 None。
pub fn parse_local_slash(input: &str) -> Option<(&str, &str)> {
    let (name, args) = rupi_core::commands::split(input)?;
    match name {
        "copy" | "hotkeys" | "trust" => Some((name, args)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::ChatView;
    use rupi_core::Message;

    #[test]
    fn last_assistant_skips_empty_and_system() {
        let mut v = ChatView::default();
        v.push_system("hi".into());
        assert!(last_assistant_from_lines(&v.lines).is_none());
        v.lines.push(Line::AssistantText(String::new()));
        assert!(last_assistant_from_lines(&v.lines).is_none());
        v.lines.push(Line::AssistantText("hello".into()));
        v.push_system("after".into());
        assert_eq!(last_assistant_from_lines(&v.lines), Some("hello"));
        assert_eq!(copy_text(None), "[copy] no assistant text");
        assert_eq!(copy_text(Some("")), "[copy] no assistant text");
    }

    #[test]
    fn last_assistant_from_session_tree() {
        let mut s = SessionTree::new();
        s.push(Message::text(Role::User, "q"));
        s.push(Message::text(Role::Assistant, "ans"));
        s.push(Message::text(Role::User, "more"));
        assert_eq!(last_assistant_from_session(&s).as_deref(), Some("ans"));
        assert!(last_assistant_from_session(&SessionTree::new()).is_none());
    }

    #[test]
    fn trust_show_and_remember() {
        let dir = std::env::temp_dir().join(format!(
            "rupi-slash-trust-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let home = dir.join("home");
        let cwd = dir.join("proj");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        let shown = trust_slash("", &home, &cwd, "ask");
        assert!(shown.contains("remembered           no"), "{shown}");
        assert!(shown.contains("ask"), "{shown}");
        let ok = trust_slash("always", &home, &cwd, "ask");
        assert!(ok.contains("remembered"), "{ok}");
        let again = trust_slash("", &home, &cwd, "never");
        assert!(again.contains("remembered           yes"), "{again}");
        assert!(trust_slash("nope", &home, &cwd, "ask").contains("usage"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_only_three_slashes() {
        assert_eq!(parse_local_slash("/copy"), Some(("copy", "")));
        assert_eq!(parse_local_slash("/hotkeys"), Some(("hotkeys", "")));
        assert_eq!(
            parse_local_slash("/trust always"),
            Some(("trust", "always"))
        );
        assert!(parse_local_slash("/settings").is_none());
        assert!(parse_local_slash("copy").is_none());
    }
}
