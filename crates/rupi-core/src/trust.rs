//! 项目信任：对标上游 Pi `project_trust`（0.79）。
//!
//! 加载项目本地资源（项目 `MEMORY.md`、`.rupi/skills`、`.rupi/commands`、
//! 以及上溯到的 `.pi/skills` / `.agents/skills`）前先确认：
//! 已记住 → 静默加载；否则问一次（总是信任并记住 / 仅本次 / 跳过）。
//! 拒绝后本轮只用全局资源；密钥是“是否加载”，不管“能否执行”——执行仍走策略门。

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

/// 项目信任默认策略（settings `defaultProjectTrust`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustPolicy {
    Ask,
    Always,
    Never,
}

impl TrustPolicy {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "ask" => Some(Self::Ask),
            "always" => Some(Self::Always),
            "never" => Some(Self::Never),
            _ => None,
        }
    }
}

/// 按策略决定是否加载项目资源。`force_trust`（`--trust-project`）压过 never。
/// `already_trusted` 只在 Ask 下跳过提问。
pub fn decide_load_project(
    policy: TrustPolicy,
    already_trusted: bool,
    force_trust: bool,
    ask: impl FnOnce() -> TrustAnswer,
) -> bool {
    if force_trust {
        return true;
    }
    match policy {
        TrustPolicy::Always => true,
        TrustPolicy::Never => false,
        TrustPolicy::Ask if already_trusted => true,
        TrustPolicy::Ask => !matches!(ask(), TrustAnswer::Skip),
    }
}

/// 用户对信任提问的回答。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustAnswer {
    /// 信任并记住（落盘，下次静默加载）。
    Always,
    /// 仅本次加载，不记住。
    Once,
    /// 本次跳过项目资源。
    Skip,
}

/// 解析一行输入：`y/yes` 总是，`o/once` 仅本次，其余（含空行/EOF）跳过。
pub fn parse_trust_answer(line: &str) -> TrustAnswer {
    match line.trim().to_lowercase().as_str() {
        "y" | "yes" | "a" | "always" => TrustAnswer::Always,
        "o" | "once" | "t" => TrustAnswer::Once,
        _ => TrustAnswer::Skip,
    }
}

/// 交互式提问（stdin/stdout）。管道/EOF 直接按跳过处理，不阻塞脚本。
pub fn ask_trust_stdin(root: &Path, resources: &[String]) -> TrustAnswer {
    println!("[trust] 检测到项目本地资源（{}）：", root.display());
    for r in resources {
        println!("  - {r}");
    }
    print!("  加载并注入模型上下文？ [y]总是记住 / [o]仅本次 / [n]跳过（默认）: ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) | Err(_) => TrustAnswer::Skip,
        Ok(_) => parse_trust_answer(&line),
    }
}

/// 受信任项目根存储（一行一个 canonical 路径，缺文件视为空）。
pub struct TrustStore {
    path: PathBuf,
    trusted: HashSet<String>,
}

impl TrustStore {
    pub fn open(path: PathBuf) -> Self {
        let trusted = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect();
        Self { path, trusted }
    }

    /// 比对键：canonicalize 优先，失败回退绝对路径，保证改名/链接前后稳定。
    pub fn key(root: &Path) -> String {
        root.canonicalize()
            .or_else(|_| std::env::current_dir().map(|c| c.join(root)))
            .unwrap_or_else(|_| root.to_path_buf())
            .to_string_lossy()
            .into_owned()
    }

    pub fn contains(&self, root: &Path) -> bool {
        self.trusted.contains(&Self::key(root))
    }

    /// 记住并落盘（幂等；父目录不存在则创建）。
    pub fn add(&mut self, root: &Path) -> std::io::Result<()> {
        let k = Self::key(root);
        if self.trusted.insert(k.clone()) {
            if let Some(parent) = self.path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut body = std::fs::read_to_string(&self.path).unwrap_or_default();
            if !body.is_empty() && !body.ends_with('\n') {
                body.push('\n');
            }
            body.push_str(&k);
            body.push('\n');
            std::fs::write(&self.path, body)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_trust_answer_matrix() {
        assert_eq!(parse_trust_answer("y"), TrustAnswer::Always);
        assert_eq!(parse_trust_answer("YES"), TrustAnswer::Always);
        assert_eq!(parse_trust_answer("o"), TrustAnswer::Once);
        assert_eq!(parse_trust_answer("once"), TrustAnswer::Once);
        assert_eq!(parse_trust_answer(""), TrustAnswer::Skip);
        assert_eq!(parse_trust_answer("n"), TrustAnswer::Skip);
        assert_eq!(parse_trust_answer("xxx"), TrustAnswer::Skip);
    }

    #[test]
    fn decide_load_project_never_skips_even_when_trusted() {
        assert!(!decide_load_project(
            TrustPolicy::Never,
            true,
            false,
            || TrustAnswer::Always
        ));
        assert!(decide_load_project(TrustPolicy::Never, false, true, || {
            TrustAnswer::Skip
        }));
        assert!(decide_load_project(
            TrustPolicy::Always,
            false,
            false,
            || TrustAnswer::Skip
        ));
        assert!(!decide_load_project(TrustPolicy::Ask, false, false, || {
            TrustAnswer::Skip
        }));
    }

    #[test]
    fn trust_store_roundtrip_and_idempotent() {
        let dir = std::env::temp_dir().join(format!("rupi-trust-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("trusted_projects");
        let root = dir.join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let mut s = TrustStore::open(path.clone());
        assert!(!s.contains(&root));
        s.add(&root).unwrap();
        assert!(s.contains(&root));
        // 幂等：重复记住不写重复行
        s.add(&root).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body.lines().count(), 1);
        // 重开仍记得
        assert!(TrustStore::open(path).contains(&root));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
