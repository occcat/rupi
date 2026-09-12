//! 执行节点上的工作区引擎。`sh -c` / 文件工具只出现在这里，不进控制面。

use crate::{store, BootstrapKind, ExecResult, ToolText};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tokio::sync::Mutex;
use uuid::Uuid;

#[derive(Debug)]
pub enum EngineError {
    Exhausted,
    NotFound,
    Other(String),
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exhausted => f.write_str("pool_exhausted"),
            Self::NotFound => f.write_str("unknown handle"),
            Self::Other(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for EngineError {}

pub struct EngineConfig {
    pub root: PathBuf,
    pub max_workspaces: u32,
    pub warm_pool: u32,
}

struct Slot {
    dir: PathBuf,
    last_used: Instant,
}

pub struct Engine {
    cfg: EngineConfig,
    leased: Mutex<HashMap<String, Slot>>,
    warm: Mutex<Vec<PathBuf>>,
}

pub struct Lease {
    pub id: String,
    pub dir: PathBuf,
}

impl Engine {
    pub fn new(cfg: EngineConfig) -> Self {
        Self {
            cfg,
            leased: Mutex::new(HashMap::new()),
            warm: Mutex::new(Vec::new()),
        }
    }

    pub fn root(&self) -> &Path {
        &self.cfg.root
    }

    pub async fn refill_warm(&self) {
        let leased = self.leased.lock().await.len() as u32;
        let mut warm = self.warm.lock().await;
        let cap = self.cfg.max_workspaces.max(1);
        let want = self.cfg.warm_pool.min(cap.saturating_sub(leased));
        while (warm.len() as u32) < want {
            let dir = self.cfg.root.join("_warm").join(Uuid::new_v4().to_string());
            if tokio::fs::create_dir_all(&dir).await.is_ok() {
                warm.push(dir);
            } else {
                break;
            }
        }
    }

    pub async fn stats(&self) -> (u32, u32, u32) {
        let used = self.leased.lock().await.len() as u32;
        let warm = self.warm.lock().await.len() as u32;
        (used, self.cfg.max_workspaces.max(1), warm)
    }

    pub async fn alloc(&self, tenant: &str, session: &str) -> Result<Lease, EngineError> {
        let leased_n = self.leased.lock().await.len() as u32;
        if leased_n >= self.cfg.max_workspaces.max(1) {
            return Err(EngineError::Exhausted);
        }
        let id = Uuid::new_v4().to_string();
        let dest = self.cfg.root.join(tenant).join(session).join(&id);
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| EngineError::Other(e.to_string()))?;
        }
        let warmed = self.warm.lock().await.pop();
        if let Some(src) = warmed {
            if tokio::fs::rename(&src, &dest).await.is_err() {
                tokio::fs::create_dir_all(&dest)
                    .await
                    .map_err(|e| EngineError::Other(e.to_string()))?;
                let _ = tokio::fs::remove_dir_all(src).await;
            }
        } else {
            tokio::fs::create_dir_all(&dest)
                .await
                .map_err(|e| EngineError::Other(e.to_string()))?;
        }
        self.leased.lock().await.insert(
            id.clone(),
            Slot {
                dir: dest.clone(),
                last_used: Instant::now(),
            },
        );
        Ok(Lease { id, dir: dest })
    }

    pub async fn dir(&self, handle: &str) -> Result<PathBuf, EngineError> {
        let mut g = self.leased.lock().await;
        match g.get_mut(handle) {
            Some(s) => {
                s.last_used = Instant::now();
                Ok(s.dir.clone())
            }
            None => Err(EngineError::NotFound),
        }
    }

    pub async fn release(&self, handle: &str) -> Result<(), EngineError> {
        let slot = self.leased.lock().await.remove(handle);
        if let Some(slot) = slot {
            wipe_dir(&slot.dir).await;
            let mut warm = self.warm.lock().await;
            if (warm.len() as u32) < self.cfg.warm_pool {
                warm.push(slot.dir);
            } else {
                let _ = tokio::fs::remove_dir_all(slot.dir).await;
            }
        }
        Ok(())
    }

    pub async fn destroy(&self, handle: &str) -> Result<(), EngineError> {
        if let Some(slot) = self.leased.lock().await.remove(handle) {
            let _ = tokio::fs::remove_dir_all(slot.dir).await;
        }
        Ok(())
    }

    pub async fn exec(
        &self,
        handle: &str,
        command: &str,
        timeout_secs: Option<u64>,
    ) -> Result<ExecResult, EngineError> {
        let dir = self.dir(handle).await?;
        let timeout = timeout_secs.unwrap_or(30).clamp(1, 300);
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg(command)
            .current_dir(&dir)
            .kill_on_drop(true)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            cmd.as_std_mut().process_group(0);
        }
        let child = cmd
            .spawn()
            .map_err(|e| EngineError::Other(e.to_string()))?;
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(timeout),
            child.wait_with_output(),
        )
        .await;
        Ok(match out {
            Ok(Ok(o)) => ExecResult {
                stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
                exit_code: o.status.code().unwrap_or(1),
            },
            Ok(Err(e)) => ExecResult {
                stdout: String::new(),
                stderr: e.to_string(),
                exit_code: 1,
            },
            Err(_) => ExecResult {
                stdout: String::new(),
                stderr: "timed out".into(),
                exit_code: 124,
            },
        })
    }

    pub async fn fs_tool(
        &self,
        handle: &str,
        name: &str,
        mut args: serde_json::Value,
    ) -> Result<ToolText, EngineError> {
        let dir = self.dir(handle).await?;
        let tools = rupi_tools::ToolRegistry::with_sandboxed_builtins(&dir);
        if let Some(obj) = args.as_object_mut() {
            if name != "bash" {
                if let Some(p) = obj.get("path").and_then(|v| v.as_str()) {
                    if !p.is_empty() && !std::path::Path::new(p).is_absolute() {
                        obj.insert(
                            "path".into(),
                            serde_json::Value::String(dir.join(p).display().to_string()),
                        );
                    }
                }
            }
        }
        let out = tools
            .execute(name, args)
            .await
            .map_err(|e| EngineError::Other(e.to_string()))?;
        Ok(ToolText {
            content: out.content,
            is_error: out.is_error,
        })
    }

    pub async fn bootstrap(
        &self,
        handle: &str,
        kind: &BootstrapKind,
    ) -> Result<ToolText, EngineError> {
        let dir = self.dir(handle).await?;
        match kind {
            BootstrapKind::Empty => Ok(ToolText::ok("empty workspace")),
            BootstrapKind::Git { url } => {
                let st = tokio::process::Command::new("git")
                    .args(["clone", "--depth", "1", url, "."])
                    .current_dir(&dir)
                    .output()
                    .await
                    .map_err(|e| EngineError::Other(e.to_string()))?;
                if st.status.success() {
                    Ok(ToolText::ok(format!("cloned {url}")))
                } else {
                    Ok(ToolText::err(String::from_utf8_lossy(&st.stderr)))
                }
            }
        }
    }

    pub async fn snapshot(&self, handle: &str) -> Result<Vec<u8>, EngineError> {
        let dir = self.dir(handle).await?;
        let tmp = self
            .cfg
            .root
            .join("_snap")
            .join(format!("{handle}.tgz"));
        if let Some(p) = tmp.parent() {
            tokio::fs::create_dir_all(p)
                .await
                .map_err(|e| EngineError::Other(e.to_string()))?;
        }
        let st = tokio::process::Command::new("tar")
            .args([
                "-C",
                &dir.display().to_string(),
                "-czf",
                &tmp.display().to_string(),
                ".",
            ])
            .status()
            .await
            .map_err(|e| EngineError::Other(e.to_string()))?;
        if !st.success() {
            return Err(EngineError::Other("tar snapshot failed".into()));
        }
        let bytes = tokio::fs::read(&tmp)
            .await
            .map_err(|e| EngineError::Other(e.to_string()))?;
        let _ = tokio::fs::remove_file(&tmp).await;
        Ok(bytes)
    }

    pub async fn restore(&self, handle: &str, blob: &[u8]) -> Result<(), EngineError> {
        let dir = self.dir(handle).await?;
        wipe_dir(&dir).await;
        let tmp = self
            .cfg
            .root
            .join("_snap")
            .join(format!("restore-{handle}.tgz"));
        if let Some(p) = tmp.parent() {
            tokio::fs::create_dir_all(p)
                .await
                .map_err(|e| EngineError::Other(e.to_string()))?;
        }
        tokio::fs::write(&tmp, blob)
            .await
            .map_err(|e| EngineError::Other(e.to_string()))?;
        let st = tokio::process::Command::new("tar")
            .args([
                "-C",
                &dir.display().to_string(),
                "-xzf",
                &tmp.display().to_string(),
            ])
            .status()
            .await
            .map_err(|e| EngineError::Other(e.to_string()))?;
        let _ = tokio::fs::remove_file(&tmp).await;
        if !st.success() {
            return Err(EngineError::Other("tar restore failed".into()));
        }
        Ok(())
    }
}

async fn wipe_dir(dir: &Path) {
    if let Ok(mut rd) = tokio::fs::read_dir(dir).await {
        while let Ok(Some(ent)) = rd.next_entry().await {
            let p = ent.path();
            let _ = if p.is_dir() {
                tokio::fs::remove_dir_all(&p).await
            } else {
                tokio::fs::remove_file(&p).await
            };
        }
    }
}

pub fn b64_archive(bytes: &[u8]) -> String {
    store::b64_encode(bytes)
}

pub fn b64_decode(s: &str) -> Result<Vec<u8>, EngineError> {
    store::b64_decode(s).map_err(|e| EngineError::Other(e.to_string()))
}
