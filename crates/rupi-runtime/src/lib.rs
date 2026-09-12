//! 云执行面：控制面只认 [`Executor`]，工具/bash 在外部进程（`rupi-execd`）落地。
//!
//! 本 crate **不含**「在 API 机起 Docker / `sh -c` 当默认执行面」。
//! 默认后端是 HTTP 远程机（一台登记的 `rupi-execd` 即算）。

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub mod execd;
pub mod http;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceHandle {
    pub id: String,
    pub backend: String,
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

/// 可插拔执行后端。控制面不得在默认路径上对本机 `sh -c`。
#[async_trait]
pub trait Executor: Send + Sync {
    fn backend_name(&self) -> &str;

    async fn alloc(&self, tenant: &str, session: &str) -> anyhow::Result<WorkspaceHandle>;
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_roundtrip() {
        let h = WorkspaceHandle {
            id: "abc".into(),
            backend: "remote-http".into(),
        };
        let s = serde_json::to_string(&h).unwrap();
        let back: WorkspaceHandle = serde_json::from_str(&s).unwrap();
        assert_eq!(h, back);
    }
}
