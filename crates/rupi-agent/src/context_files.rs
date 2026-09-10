use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct ContextFile {
    pub path: String,
    pub content: String,
}

const CANDIDATES: &[&str] = &[
    "AGENTS.override.md",
    "AGENTS.md",
    "AGENTS.MD",
    "CLAUDE.md",
    "CLAUDE.MD",
];

/// Load Pi-style project context files: global `~/.rupi/AGENTS.md` plus
/// AGENTS.md / CLAUDE.md walking from cwd up to the git root (or filesystem root).
pub fn load_context_files(cwd: &Path, home_agent_dir: Option<&Path>) -> Vec<ContextFile> {
    let mut files = Vec::new();
    if let Some(home) = home_agent_dir {
        if let Some(f) = load_from_dir(home) {
            files.push(f);
        }
    }
    let mut dir = cwd.to_path_buf();
    let stop = git_root(cwd).unwrap_or_else(|| PathBuf::from("/"));
    loop {
        if let Some(f) = load_from_dir(&dir) {
            if !files.iter().any(|e| e.path == f.path) {
                files.push(f);
            }
        }
        if dir == stop {
            break;
        }
        match dir.parent() {
            Some(p) => dir = p.to_path_buf(),
            None => break,
        }
    }
    files.reverse();
    files
}

fn load_from_dir(dir: &Path) -> Option<ContextFile> {
    for name in CANDIDATES {
        let path = dir.join(name);
        if path.is_file() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                return Some(ContextFile {
                    path: path.display().to_string(),
                    content,
                });
            }
        }
    }
    None
}

fn git_root(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}
