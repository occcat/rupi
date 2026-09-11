//! Skills 发现目录：对标 Pi / Agent Skills 约定，并保留 rupi 既有路径。
//!
//! 先发现者胜（与 `SkillRegistry::refresh` 同名去重一致）。顺序：
//! 内建 → `~/.rupi/skills` → 项目 `.rupi/skills` → `~/.pi/agent/skills` →
//! `~/.agents/skills` → 从 cwd 上溯的 `.pi/skills` / `.agents/skills`。
//!
//! 项目级上溯与 Pi `collectAncestorAgentsSkillDirs` 同款：有 `.git` 停在仓库根，
//! 否则走到文件系统根；与全局 `~/.agents/skills` 重合的路径不重复列入项目层。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// 从 `start` 上溯找 `.git`（文件或目录均可，含 worktree gitfile）。
pub fn find_git_root(start: &Path) -> Option<PathBuf> {
    let mut dir = if start.is_file() {
        start.parent()?.to_path_buf()
    } else {
        start.to_path_buf()
    };
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// 用户家目录下的 Pi / Agent Skills 全局约定路径（不走 `RUPI_HOME`）。
pub fn user_shared_skill_dirs(user_home: &Path) -> Vec<PathBuf> {
    vec![
        user_home.join(".pi").join("agent").join("skills"),
        user_home.join(".agents").join("skills"),
    ]
}

fn path_key(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

fn same_path(a: &Path, b: &Path) -> bool {
    path_key(a) == path_key(b)
}

fn push_unique(out: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>, dir: PathBuf) {
    if seen.insert(path_key(&dir)) {
        out.push(dir);
    }
}

/// 从 cwd 上溯收集项目级 `.pi/skills` 与 `.agents/skills`（目录不必已存在）。
///
/// 有 git 仓库则含仓库根、不含仓外祖先；无仓库则走到文件系统根。
/// 跳过与 `~/.agents/skills` / `~/.pi/agent/skills` 重合的路径，避免把全局目录
/// 再当成项目资源（对标 Pi `resolve(dir) !== resolve(userAgentsSkillsDir)`）。
pub fn ancestor_project_skill_dirs(cwd: &Path, user_home: &Path) -> Vec<PathBuf> {
    let git_root = find_git_root(cwd);
    let skip = user_shared_skill_dirs(user_home);
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut dir = cwd.to_path_buf();
    loop {
        for rel in [".pi/skills", ".agents/skills"] {
            let cand = dir.join(rel);
            if skip.iter().any(|g| same_path(&cand, g)) {
                continue;
            }
            push_unique(&mut out, &mut seen, cand);
        }
        if git_root.as_ref().is_some_and(|root| same_path(&dir, root)) {
            break;
        }
        match dir.parent() {
            Some(parent) if parent != dir => dir = parent.to_path_buf(),
            _ => break,
        }
    }
    out
}

/// 已存在的项目级 Pi / Agent Skills 目录（供信任门列出）。
pub fn existing_project_skill_dirs(cwd: &Path, user_home: &Path) -> Vec<PathBuf> {
    ancestor_project_skill_dirs(cwd, user_home)
        .into_iter()
        .filter(|p| p.is_dir())
        .collect()
}

/// 完整发现目录列表。`builtin` 由调用方按 exe 锚定；`rupi_home` 为 `~/.rupi`
///（或 `RUPI_HOME`）；`user_home` 为 `$HOME`。
pub fn skill_search_dirs(
    builtin: PathBuf,
    rupi_home: &Path,
    user_home: &Path,
    cwd: &Path,
    load_project: bool,
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    push_unique(&mut out, &mut seen, builtin);
    push_unique(&mut out, &mut seen, rupi_home.join("skills"));
    if load_project {
        push_unique(&mut out, &mut seen, cwd.join(".rupi").join("skills"));
    }
    for d in user_shared_skill_dirs(user_home) {
        push_unique(&mut out, &mut seen, d);
    }
    if load_project {
        for d in ancestor_project_skill_dirs(cwd, user_home) {
            push_unique(&mut out, &mut seen, d);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn scratch() -> PathBuf {
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("rupi-skill-dirs-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_skill(dir: &Path, name: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {name} skill\n---\n\n# {name}\n"),
        )
        .unwrap();
    }

    #[test]
    fn load_project_false_omits_rupi_and_ancestors() {
        let root = scratch();
        let cwd = root.join("proj");
        let rupi_home = root.join("rupi-home");
        let user_home = root.join("user-home");
        std::fs::create_dir_all(&cwd).unwrap();
        let dirs = skill_search_dirs(root.join("builtin"), &rupi_home, &user_home, &cwd, false);
        assert!(dirs.contains(&root.join("builtin")));
        assert!(dirs.contains(&rupi_home.join("skills")));
        assert!(dirs.contains(&user_home.join(".pi").join("agent").join("skills")));
        assert!(dirs.contains(&user_home.join(".agents").join("skills")));
        assert!(!dirs.iter().any(|p| p.ends_with(Path::new(".rupi/skills"))));
        assert!(!dirs.iter().any(|p| p.ends_with(Path::new(".pi/skills"))));
        assert!(!dirs
            .iter()
            .any(|p| p != &user_home.join(".agents").join("skills")
                && p.ends_with(Path::new(".agents/skills"))));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn git_root_stops_ancestor_walk() {
        let root = scratch();
        let outside = root
            .join("outside")
            .join(".agents")
            .join("skills")
            .join("leak");
        let repo = root.join("repo");
        let nested = repo.join("nested");
        write_skill(&outside, "leak-skill");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(repo.join(".git"), "gitdir: fake\n").unwrap();
        write_skill(
            &repo.join(".agents").join("skills").join("repo-skill"),
            "repo-skill",
        );
        write_skill(&nested.join(".pi").join("skills").join("cwd-pi"), "cwd-pi");

        let walked = ancestor_project_skill_dirs(&nested, &root.join("user-home"));
        assert!(walked.contains(&nested.join(".pi").join("skills")));
        assert!(walked.contains(&nested.join(".agents").join("skills")));
        assert!(walked.contains(&repo.join(".agents").join("skills")));
        assert!(walked.contains(&repo.join(".pi").join("skills")));
        assert!(!walked.iter().any(|p| p.starts_with(root.join("outside"))));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn no_git_walks_parents() {
        let root = scratch();
        let parent = root.join("parent");
        let child = parent.join("child");
        std::fs::create_dir_all(&child).unwrap();
        write_skill(
            &parent.join(".agents").join("skills").join("up-skill"),
            "up-skill",
        );
        let walked = ancestor_project_skill_dirs(&child, &root.join("user-home"));
        assert!(walked.contains(&parent.join(".agents").join("skills")));
        assert!(walked.contains(&child.join(".agents").join("skills")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ancestor_walk_skips_global_agents_dir() {
        let root = scratch();
        let user_home = root.join("home");
        let cwd = user_home.join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        write_skill(
            &user_home.join(".agents").join("skills").join("global"),
            "global-skill",
        );
        let walked = ancestor_project_skill_dirs(&cwd, &user_home);
        assert!(!walked
            .iter()
            .any(|p| same_path(p, &user_home.join(".agents").join("skills"))));
        assert!(walked.contains(&cwd.join(".agents").join("skills")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn search_order_keeps_rupi_dirs_ahead_of_shared() {
        let root = scratch();
        let cwd = root.join("proj");
        let rupi_home = root.join("rupi-home");
        let user_home = root.join("user-home");
        std::fs::create_dir_all(&cwd).unwrap();
        let dirs = skill_search_dirs(root.join("builtin"), &rupi_home, &user_home, &cwd, true);
        let idx = |suffix: &str| {
            dirs.iter()
                .position(|p| p.ends_with(Path::new(suffix)))
                .unwrap_or_else(|| panic!("missing {suffix} in {dirs:?}"))
        };
        assert!(idx("builtin") < idx("rupi-home/skills"));
        assert!(idx("rupi-home/skills") < idx("proj/.rupi/skills"));
        assert!(idx("proj/.rupi/skills") < idx(".pi/agent/skills"));
        assert!(idx(".pi/agent/skills") < idx("user-home/.agents/skills"));
        assert!(idx("user-home/.agents/skills") < idx("proj/.pi/skills"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn existing_project_dirs_only_lists_directories() {
        let root = scratch();
        let cwd = root.join("proj");
        std::fs::create_dir_all(cwd.join(".agents").join("skills")).unwrap();
        // 钉在 scratch 根，避免无 git 时上溯到 /tmp 碰上环境里的 .agents/skills。
        std::fs::write(root.join(".git"), "gitdir: fake\n").unwrap();
        let found = existing_project_skill_dirs(&cwd, &root.join("home"));
        assert_eq!(found, vec![cwd.join(".agents").join("skills")]);
        let _ = std::fs::remove_dir_all(&root);
    }
}
