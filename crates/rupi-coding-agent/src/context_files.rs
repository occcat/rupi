use std::path::{Path, PathBuf};

/// Discover AGENTS.md / SYSTEM.md / APPEND_SYSTEM.md the way Pi's ResourceLoader does.
pub fn load_agents_files(cwd: &Path) -> Vec<(String, String)> {
    let mut files = Vec::new();
    let mut cur = Some(cwd.to_path_buf());
    let mut chain = Vec::new();
    while let Some(dir) = cur {
        chain.push(dir.clone());
        cur = dir.parent().map(|p| p.to_path_buf());
        if dir.parent().is_none() {
            break;
        }
    }
    chain.reverse();
    for dir in chain {
        for name in ["AGENTS.md", "CLAUDE.md", "CONTEXT.md"] {
            let p = dir.join(name);
            if let Ok(c) = std::fs::read_to_string(&p) {
                files.push((p.display().to_string(), c));
            }
        }
    }
    files
}

pub fn load_system_prompt_files(cwd: &Path, agent_dir: &Path) -> (Option<String>, Vec<String>) {
    let system = read_first(&[
        cwd.join(".rupi").join("SYSTEM.md"),
        cwd.join(".pi").join("SYSTEM.md"),
        agent_dir.join("SYSTEM.md"),
    ]);
    let mut append = Vec::new();
    for p in [
        cwd.join(".rupi").join("APPEND_SYSTEM.md"),
        cwd.join(".pi").join("APPEND_SYSTEM.md"),
        agent_dir.join("APPEND_SYSTEM.md"),
    ] {
        if let Ok(c) = std::fs::read_to_string(p) {
            append.push(c);
        }
    }
    (system, append)
}

fn read_first(paths: &[PathBuf]) -> Option<String> {
    for p in paths {
        if let Ok(c) = std::fs::read_to_string(p) {
            return Some(c);
        }
    }
    None
}
