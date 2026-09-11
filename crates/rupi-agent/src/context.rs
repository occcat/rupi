//! 项目上下文文件（对标上游 `loadProjectContextFiles`）：`AGENTS.md` 系分层注入。
//!
//! 语义（与上游一致）：每目录候选 `AGENTS.override.md > AGENTS.md > AGENTS.MD >
//! CLAUDE.md > CLAUDE.MD` 取首个命中的文件；全局 agentDir（`~/.rupi`）在前，
//! 祖先链（cwd→根）随后，根在前 cwd 在后。读失败/非文件静默跳过（warn，不炸主循环）。
//! 每 turn 重读（改完即生效；文件小，IO 可忽略）。
//! 有意不同：上游的 git worktree shadow 去重不做（嵌套 worktree 边角；
//! 同一文件多路径到达时按 canonical 去重已覆盖大半）。

use std::path::{Path, PathBuf};

/// 单个上下文文件：绝对路径 + 去 BOM 正文。
#[derive(Debug, Clone, Default)]
pub struct ContextFile {
    pub path: String,
    pub content: String,
}

/// 每目录候选优先级（上游同序）。
const CANDIDATES: [&str; 5] = [
    "AGENTS.override.md",
    "AGENTS.md",
    "AGENTS.MD",
    "CLAUDE.md",
    "CLAUDE.MD",
];

fn load_from_dir(dir: &Path) -> Option<ContextFile> {
    for name in CANDIDATES {
        let p = dir.join(name);
        let Ok(meta) = std::fs::symlink_metadata(&p) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        match std::fs::read_to_string(&p) {
            Ok(raw) => {
                return Some(ContextFile {
                    path: p.to_string_lossy().into_owned(),
                    content: raw.strip_prefix('\u{FEFF}').unwrap_or(&raw).to_string(),
                });
            }
            Err(e) => {
                tracing::warn!("context file {} 读失败已跳过: {e}", p.display());
                continue;
            }
        }
    }
    None
}

/// 加载分层上下文：`agent_dir`（全局）在前，`cwd` 祖先链随后（根→cwd）。
/// 路径 canonical 去重（symlink 多路径到达同一文件只收一次）。
pub fn load_context_files(cwd: &Path, agent_dir: &Path) -> Vec<ContextFile> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push = |f: ContextFile| {
        let canon = std::path::PathBuf::from(&f.path)
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(&f.path));
        if seen.insert(canon) {
            out.push(f);
        }
    };
    if let Some(f) = load_from_dir(agent_dir) {
        push(f);
    }
    // 祖先链：cwd→根收集后反转，根在前 cwd 在后（越近优先级越高，模型越先看到远的）。
    let mut chain = Vec::new();
    let mut dir = Some(cwd.to_path_buf());
    while let Some(d) = dir {
        if let Some(f) = load_from_dir(&d) {
            chain.push(f);
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    for f in chain.into_iter().rev() {
        push(f);
    }
    out
}

/// 拼 `<project_context>` 块（上游 `buildSystemPrompt` 同格式，位置在 skills 索引前）。
pub fn format_context_block(files: &[ContextFile]) -> String {
    if files.is_empty() {
        return String::new();
    }
    let mut s =
        String::from("\n<project_context>\n\nProject-specific instructions and guidelines:\n\n");
    for f in files {
        s.push_str(&format!(
            "<project_instructions path=\"{}\">\n{}\n</project_instructions>\n\n",
            f.path, f.content
        ));
    }
    s.push_str("</project_context>\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn candidate_priority_override_wins() {
        let base = std::env::temp_dir().join(format!("rupi-ctx-prio-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        write(&base, "CLAUDE.md", "claude");
        write(&base, "AGENTS.md", "agents");
        let f = load_from_dir(&base).expect("hit");
        assert!(f.content.contains("agents"), "{}", f.content);
        write(&base, "AGENTS.override.md", "override");
        let f = load_from_dir(&base).expect("hit");
        assert!(f.content.contains("override"), "{}", f.content);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn ancestor_chain_global_first_root_before_cwd() {
        let base = std::env::temp_dir().join(format!("rupi-ctx-chain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let global = base.join("home");
        let proj = base.join("proj");
        let sub = proj.join("sub");
        write(&global, "AGENTS.md", "GLOBAL");
        write(&proj, "AGENTS.md", "PROJ");
        write(&sub, "AGENTS.md", "SUB");
        let files = load_context_files(&sub, &global);
        let bodies: Vec<_> = files.iter().map(|f| f.content.clone()).collect();
        assert_eq!(bodies, vec!["GLOBAL", "PROJ", "SUB"], "{bodies:?}");
        // 拼块格式对标上游 project_context
        let block = format_context_block(&files);
        assert!(block.contains("<project_context>"));
        assert!(block.contains("<project_instructions path="));
        assert!(block.find("GLOBAL").unwrap() < block.find("SUB").unwrap());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn unreadable_and_dirs_are_skipped_quietly() {
        let base = std::env::temp_dir().join(format!("rupi-ctx-skip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        // 同名目录不算命中；空目录链返回空
        std::fs::create_dir_all(base.join("AGENTS.md")).unwrap();
        assert!(load_from_dir(&base).is_none());
        assert!(load_context_files(&base, &base.join("no-such-home")).is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }
}
