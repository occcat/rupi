use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionDecision {
    Allow,
    Deny,
    Ask,
}

#[derive(Debug, Clone)]
pub struct PathGuard {
    pub cwd: PathBuf,
    pub allow_outside: bool,
}

impl PathGuard {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            allow_outside: false,
        }
    }

    pub fn resolve(&self, path: &str) -> Result<PathBuf, String> {
        let p = Path::new(path);
        let resolved = if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.cwd.join(p)
        };
        let canonical_cwd = self.cwd.canonicalize().unwrap_or_else(|_| self.cwd.clone());
        if self.allow_outside {
            return Ok(resolved);
        }
        // Compare lexically when the path does not yet exist.
        let check = if resolved.exists() {
            resolved.canonicalize().unwrap_or(resolved.clone())
        } else if let Some(parent) = resolved.parent() {
            let parent = parent.canonicalize().unwrap_or(parent.to_path_buf());
            parent.join(resolved.file_name().unwrap_or_default())
        } else {
            resolved.clone()
        };
        if check.starts_with(&canonical_cwd) || resolved.starts_with(&self.cwd) {
            Ok(resolved)
        } else {
            Err(format!(
                "path `{}` is outside the workspace `{}`",
                resolved.display(),
                self.cwd.display()
            ))
        }
    }
}

#[derive(Debug, Clone)]
pub struct PermissionGate {
    pub allow_read: bool,
    pub allow_write: bool,
    pub allow_bash: bool,
    pub bash_ask_destructive: bool,
    pub path_guard: PathGuard,
}

impl PermissionGate {
    pub fn permissive(cwd: impl Into<PathBuf>) -> Self {
        Self {
            allow_read: true,
            allow_write: true,
            allow_bash: true,
            bash_ask_destructive: true,
            path_guard: PathGuard::new(cwd),
        }
    }

    pub fn sandboxed(cwd: impl Into<PathBuf>) -> Self {
        Self {
            allow_read: true,
            allow_write: true,
            allow_bash: true,
            bash_ask_destructive: true,
            path_guard: PathGuard::new(cwd),
        }
    }

    pub fn decide(&self, tool: &str, args_hint: &str) -> PermissionDecision {
        match tool {
            "read" | "grep" | "find" | "ls" | "skill_view" | "session_search" => {
                if self.allow_read {
                    PermissionDecision::Allow
                } else {
                    PermissionDecision::Deny
                }
            }
            "write" | "edit" => {
                if self.allow_write {
                    PermissionDecision::Allow
                } else {
                    PermissionDecision::Deny
                }
            }
            "bash" => {
                if !self.allow_bash {
                    PermissionDecision::Deny
                } else if self.bash_ask_destructive && looks_destructive(args_hint) {
                    PermissionDecision::Ask
                } else {
                    PermissionDecision::Allow
                }
            }
            _ => PermissionDecision::Allow,
        }
    }
}

fn looks_destructive(cmd: &str) -> bool {
    let lower = cmd.to_ascii_lowercase();
    ["rm -rf", "mkfs", "dd if=", ":(){", "shutdown", "reboot"]
        .iter()
        .any(|p| lower.contains(p))
}
