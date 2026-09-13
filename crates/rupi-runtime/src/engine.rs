//! 执行节点上的工作区引擎。`sh -c` / 文件工具只出现在这里，不进控制面。
//! bash 箍在句柄根；句柄绑 `tenant_id`，对不上 403。

use crate::jail::{self, Isolation};
use crate::{store, tar, BootstrapKind, ExecResult, ToolText};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;
use tokio::sync::Mutex;
use uuid::Uuid;

#[derive(Debug)]
pub enum EngineError {
    Exhausted,
    NotFound,
    Forbidden,
    Other(String),
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exhausted => f.write_str("pool_exhausted"),
            Self::NotFound => f.write_str("unknown handle"),
            Self::Forbidden => f.write_str("tenant mismatch"),
            Self::Other(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for EngineError {}

pub struct EngineConfig {
    pub root: PathBuf,
    pub max_workspaces: u32,
    pub warm_pool: u32,
    pub isolation: Isolation,
}

struct Slot {
    dir: PathBuf,
    tenant_id: String,
    last_used: Instant,
    children: Vec<u32>,
}

pub struct Engine {
    cfg: EngineConfig,
    leased: Mutex<HashMap<String, Slot>>,
    warm: Mutex<Vec<PathBuf>>,
    cancel_seq: AtomicU32,
}

pub struct Lease {
    pub id: String,
    pub dir: PathBuf,
    pub tenant_id: String,
}

impl Engine {
    pub fn new(cfg: EngineConfig) -> Self {
        Self {
            cfg,
            leased: Mutex::new(HashMap::new()),
            warm: Mutex::new(Vec::new()),
            cancel_seq: AtomicU32::new(1),
        }
    }

    pub fn root(&self) -> &Path {
        &self.cfg.root
    }

    pub fn isolation(&self) -> Isolation {
        self.cfg.isolation
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
        if tenant.is_empty() || tenant.contains("..") || tenant.contains('/') {
            return Err(EngineError::Other("invalid tenant".into()));
        }
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
                tenant_id: tenant.to_string(),
                last_used: Instant::now(),
                children: Vec::new(),
            },
        );
        Ok(Lease {
            id,
            dir: dest,
            tenant_id: tenant.to_string(),
        })
    }

    /// 旧接口：不带 tenant。生产路径请用 [`dir_for`]。
    pub async fn dir(&self, handle: &str) -> Result<PathBuf, EngineError> {
        self.dir_for(handle, None).await
    }

    pub async fn dir_for(
        &self,
        handle: &str,
        tenant: Option<&str>,
    ) -> Result<PathBuf, EngineError> {
        let mut g = self.leased.lock().await;
        match g.get_mut(handle) {
            Some(s) => {
                if let Some(t) = tenant {
                    if s.tenant_id != t {
                        return Err(EngineError::Forbidden);
                    }
                }
                s.last_used = Instant::now();
                Ok(s.dir.clone())
            }
            None => Err(EngineError::NotFound),
        }
    }

    pub async fn require_tenant(&self, handle: &str, tenant: &str) -> Result<PathBuf, EngineError> {
        if tenant.is_empty() {
            return Err(EngineError::Forbidden);
        }
        self.dir_for(handle, Some(tenant)).await
    }

    pub async fn release(&self, handle: &str) -> Result<(), EngineError> {
        self.abort(handle).await;
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
        self.abort(handle).await;
        if let Some(slot) = self.leased.lock().await.remove(handle) {
            let _ = tokio::fs::remove_dir_all(slot.dir).await;
        }
        Ok(())
    }

    pub async fn abort(&self, handle: &str) {
        let pids = {
            let mut g = self.leased.lock().await;
            g.get_mut(handle)
                .map(|s| std::mem::take(&mut s.children))
                .unwrap_or_default()
        };
        for pid in pids {
            #[cfg(unix)]
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
            let _ = pid;
        }
    }

    pub async fn exec(
        &self,
        handle: &str,
        command: &str,
        timeout_secs: Option<u64>,
    ) -> Result<ExecResult, EngineError> {
        self.exec_for(handle, None, command, timeout_secs).await
    }

    pub async fn exec_for(
        &self,
        handle: &str,
        tenant: Option<&str>,
        command: &str,
        timeout_secs: Option<u64>,
    ) -> Result<ExecResult, EngineError> {
        let dir = match tenant {
            Some(t) => self.require_tenant(handle, t).await?,
            None => self.dir(handle).await?,
        };
        let timeout = timeout_secs.unwrap_or(30).clamp(1, 300);
        let mut cmd = jail::command(&dir, command, self.cfg.isolation)
            .map_err(|e| EngineError::Other(e.to_string()))?;
        let child = cmd
            .spawn()
            .map_err(|e| EngineError::Other(e.to_string()))?;
        if let Some(pid) = child.id() {
            if let Some(s) = self.leased.lock().await.get_mut(handle) {
                s.children.push(pid);
            }
        }
        let handle_id = handle.to_string();
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(timeout),
            child.wait_with_output(),
        )
        .await;
        if let Some(s) = self.leased.lock().await.get_mut(&handle_id) {
            s.children.clear();
        }
        let _ = self.cancel_seq.fetch_add(1, Ordering::Relaxed);
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
            Err(_) => {
                self.abort(handle).await;
                ExecResult {
                    stdout: String::new(),
                    stderr: "timed out".into(),
                    exit_code: 124,
                }
            }
        })
    }

    pub async fn fs_tool(
        &self,
        handle: &str,
        name: &str,
        args: serde_json::Value,
    ) -> Result<ToolText, EngineError> {
        self.fs_tool_for(handle, None, name, args).await
    }

    pub async fn fs_tool_for(
        &self,
        handle: &str,
        tenant: Option<&str>,
        name: &str,
        mut args: serde_json::Value,
    ) -> Result<ToolText, EngineError> {
        let dir = match tenant {
            Some(t) => self.require_tenant(handle, t).await?,
            None => self.dir(handle).await?,
        };
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
        self.bootstrap_for(handle, None, kind).await
    }

    pub async fn bootstrap_for(
        &self,
        handle: &str,
        tenant: Option<&str>,
        kind: &BootstrapKind,
    ) -> Result<ToolText, EngineError> {
        let dir = match tenant {
            Some(t) => self.require_tenant(handle, t).await?,
            None => self.dir(handle).await?,
        };
        match kind {
            BootstrapKind::Empty => Ok(ToolText::ok("empty workspace")),
            BootstrapKind::Git { url, token, hosts } => {
                let spec = crate::git::parse_git_url(url).map_err(EngineError::Other)?;
                crate::git::clone_into(&dir, &spec, token.as_deref(), hosts)
                    .await
                    .map_err(EngineError::Other)
            }
        }
    }

    pub async fn snapshot(&self, handle: &str) -> Result<Vec<u8>, EngineError> {
        self.snapshot_for(handle, None).await
    }

    pub async fn snapshot_for(
        &self,
        handle: &str,
        tenant: Option<&str>,
    ) -> Result<Vec<u8>, EngineError> {
        let dir = match tenant {
            Some(t) => self.require_tenant(handle, t).await?,
            None => self.dir(handle).await?,
        };
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
        self.restore_for(handle, None, blob).await
    }

    pub async fn restore_for(
        &self,
        handle: &str,
        tenant: Option<&str>,
        blob: &[u8],
    ) -> Result<(), EngineError> {
        let dir = match tenant {
            Some(t) => self.require_tenant(handle, t).await?,
            None => self.dir(handle).await?,
        };
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
        let extracted = tar::extract_checked(&tmp, &dir).await;
        let _ = tokio::fs::remove_file(&tmp).await;
        if let Err(e) = extracted {
            wipe_dir(&dir).await;
            return Err(EngineError::Other(e));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(root: PathBuf) -> EngineConfig {
        EngineConfig {
            root,
            max_workspaces: 8,
            warm_pool: 0,
            isolation: Isolation::Jail,
        }
    }

    #[tokio::test]
    async fn tenant_mismatch_is_forbidden() {
        let root = std::env::temp_dir().join(format!("rupi-eng-{}", Uuid::new_v4()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let eng = Engine::new(cfg(root.clone()));
        let a = eng.alloc("ten-a", "s1").await.unwrap();
        let err = eng.require_tenant(&a.id, "ten-b").await.unwrap_err();
        assert!(matches!(err, EngineError::Forbidden), "{err}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn bash_cannot_read_neighbor_volume() {
        let root = std::env::temp_dir().join(format!("rupi-jail-{}", Uuid::new_v4()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let eng = Engine::new(cfg(root.clone()));
        let a = eng.alloc("ten-a", "s1").await.unwrap();
        let b = eng.alloc("ten-b", "s1").await.unwrap();
        std::fs::write(a.dir.join("secret.txt"), "NEIGHBOR-SECRET").unwrap();
        let inside = eng
            .exec_for(&b.id, Some("ten-b"), "echo IN-VOLUME", Some(10))
            .await
            .expect("in-volume bash must start");
        assert!(
            inside.stdout.contains("IN-VOLUME"),
            "in-volume bash failed: stdout={} stderr={}",
            inside.stdout,
            inside.stderr
        );
        let escaped = format!(
            "cat {} 2>/dev/null || cat ../{}/s1/{}/secret.txt 2>/dev/null || cat ../../ten-a/s1/{}/secret.txt 2>/dev/null; echo DONE",
            a.dir.join("secret.txt").display(),
            "ten-a",
            a.id,
            a.id
        );
        let out = eng
            .exec_for(&b.id, Some("ten-b"), &escaped, Some(10))
            .await
            .expect("jailed bash must start");
        assert!(
            out.stdout.contains("DONE"),
            "jailed bash did not finish: stdout={} stderr={}",
            out.stdout,
            out.stderr
        );
        assert!(
            !out.stdout.contains("NEIGHBOR-SECRET"),
            "jail leaked neighbor: {}",
            out.stdout
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn poison_tar_restore_fails_and_wipes() {
        let root = std::env::temp_dir().join(format!("rupi-restore-{}", Uuid::new_v4()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let eng = Engine::new(cfg(root.clone()));
        let h = eng.alloc("ten", "s").await.unwrap();
        let Ok(blob) = tar::poison_tarball() else {
            let _ = std::fs::remove_dir_all(root);
            return;
        };
        let err = eng.restore(&h.id, &blob).await.unwrap_err();
        assert!(format!("{err}").contains("unsafe") || format!("{err}").contains("tar"), "{err}");
        let _ = std::fs::remove_dir_all(root);
    }
}
