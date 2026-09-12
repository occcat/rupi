//! 云执行面：控制面只认 [`Executor`]，工具/bash 在外部进程落地。
//!
//! 本 crate **不含**「在 API 机起 Docker / `sh -c` 当默认执行面」。
//! 可插拔后端：
//! - `remote-http`：登记的 `rupi-execd`（远程机器池）
//! - `sandbox`：`rupi-sandboxd` 集群 API（与 execd 协议并列）

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub mod engine;
pub mod execd;
pub mod http;
pub mod pool;
pub mod sandbox;
pub mod sandbox_http;
pub mod store;

pub use pool::{is_pool_exhausted, parse_endpoint_list, PoolExhausted, PoolNode, PoolScheduler};
pub use sandbox_http::SandboxExecutor;
pub use store::{LocalObjectStore, ObjectStore};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackendKind {
    RemoteHttp,
    Sandbox,
}

impl BackendKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RemoteHttp => "remote-http",
            Self::Sandbox => "sandbox",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "remote-http" | "execd" | "remote" => Some(Self::RemoteHttp),
            "sandbox" | "sandbox-cluster" => Some(Self::Sandbox),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct AllocRequest {
    pub tenant: String,
    pub session: String,
    pub region: Option<String>,
    pub kind: Option<BackendKind>,
}

impl AllocRequest {
    pub fn new(tenant: impl Into<String>, session: impl Into<String>) -> Self {
        Self {
            tenant: tenant.into(),
            session: session.into(),
            region: None,
            kind: None,
        }
    }

    pub fn with_region(mut self, region: impl Into<String>) -> Self {
        let r = region.into();
        if !r.is_empty() {
            self.region = Some(r);
        }
        self
    }

    pub fn with_kind(mut self, kind: BackendKind) -> Self {
        self.kind = Some(kind);
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct WorkspaceHandle {
    pub id: String,
    pub backend: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

impl WorkspaceHandle {
    pub fn new(id: impl Into<String>, backend: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            backend: backend.into(),
            region: None,
            kind: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecRequest {
    pub command: String,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsReadRequest {
    pub path: String,
    #[serde(default)]
    pub offset: Option<u64>,
    #[serde(default)]
    pub limit: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsWriteRequest {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsEditRequest {
    pub path: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobRequest {
    pub pattern: String,
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepRequest {
    pub pattern: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub include: Option<String>,
    #[serde(default)]
    pub max_results: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BootstrapKind {
    Empty,
    Git { url: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolText {
    pub content: String,
    pub is_error: bool,
}

impl ToolText {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }
    pub fn err(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

/// 单节点容量。调度器用它挑最闲的执行节点。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ExecutorStats {
    pub backend: String,
    pub node_id: String,
    pub used: u32,
    pub capacity: u32,
    pub warm: u32,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NodeInfo {
    pub id: String,
    pub kind: String,
    pub region: String,
}

/// 可插拔执行后端。控制面不得在默认路径上对本机 `sh -c`。
#[async_trait]
pub trait Executor: Send + Sync {
    fn backend_name(&self) -> &str;

    fn topology(&self) -> Vec<NodeInfo> {
        vec![NodeInfo {
            id: self.backend_name().into(),
            kind: self.backend_name().into(),
            region: String::new(),
        }]
    }

    async fn alloc(&self, tenant: &str, session: &str) -> anyhow::Result<WorkspaceHandle>;

    async fn alloc_pref(&self, req: &AllocRequest) -> anyhow::Result<WorkspaceHandle> {
        self.alloc(&req.tenant, &req.session).await
    }

    async fn release(&self, handle: &WorkspaceHandle) -> anyhow::Result<()>;
    async fn destroy(&self, handle: &WorkspaceHandle) -> anyhow::Result<()>;
    async fn exec(&self, handle: &WorkspaceHandle, req: ExecRequest) -> anyhow::Result<ExecResult>;
    async fn fs_read(
        &self,
        handle: &WorkspaceHandle,
        req: FsReadRequest,
    ) -> anyhow::Result<ToolText>;
    async fn fs_write(
        &self,
        handle: &WorkspaceHandle,
        req: FsWriteRequest,
    ) -> anyhow::Result<ToolText>;
    async fn fs_edit(
        &self,
        handle: &WorkspaceHandle,
        req: FsEditRequest,
    ) -> anyhow::Result<ToolText>;
    async fn glob(&self, handle: &WorkspaceHandle, req: GlobRequest) -> anyhow::Result<ToolText>;
    async fn grep(&self, handle: &WorkspaceHandle, req: GrepRequest) -> anyhow::Result<ToolText>;
    async fn bootstrap(
        &self,
        handle: &WorkspaceHandle,
        kind: BootstrapKind,
    ) -> anyhow::Result<ToolText>;

    /// 把工作区打成 tar.gz 字节。默认后端不支持。
    async fn snapshot(&self, _handle: &WorkspaceHandle) -> anyhow::Result<Vec<u8>> {
        anyhow::bail!("snapshot not supported by this backend")
    }

    /// 把快照解到已 alloc 的句柄。默认后端不支持。
    async fn restore(&self, _handle: &WorkspaceHandle, _blob: &[u8]) -> anyhow::Result<()> {
        anyhow::bail!("restore not supported by this backend")
    }

    async fn stats(&self) -> anyhow::Result<ExecutorStats> {
        Ok(ExecutorStats {
            backend: self.backend_name().into(),
            node_id: self.backend_name().into(),
            used: 0,
            capacity: 0,
            warm: 0,
            region: None,
            kind: Some(self.backend_name().into()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_roundtrip() {
        let h = WorkspaceHandle {
            id: "abc".into(),
            backend: "remote-http".into(),
            region: Some("us-east".into()),
            kind: Some("remote-http".into()),
        };
        let s = serde_json::to_string(&h).unwrap();
        let back: WorkspaceHandle = serde_json::from_str(&s).unwrap();
        assert_eq!(h, back);
        let old: WorkspaceHandle =
            serde_json::from_str(r#"{"id":"x","backend":"remote-http"}"#).unwrap();
        assert_eq!(old.id, "x");
        assert!(old.region.is_none());
    }
}
