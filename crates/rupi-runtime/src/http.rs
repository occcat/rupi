//! 控制面侧：把 [`crate::Executor`] 打到远端 `rupi-execd`。本进程不 `sh -c`。

use crate::{
    BootstrapKind, ExecRequest, ExecResult, Executor, FsEditRequest, FsReadRequest, FsWriteRequest,
    GlobRequest, GrepRequest, ToolText, WorkspaceHandle,
};
use async_trait::async_trait;
use serde::Serialize;

#[derive(Clone)]
pub struct HttpExecutor {
    base: String,
    token: String,
    client: reqwest::Client,
}

impl HttpExecutor {
    pub fn new(base: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            token: token.into(),
            client: reqwest::Client::new(),
        }
    }

    async fn post<T: Serialize, R: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &T,
    ) -> anyhow::Result<R> {
        let url = format!("{}{path}", self.base);
        let mut req = self.client.post(&url).json(body);
        if !self.token.is_empty() {
            req = req.bearer_auth(&self.token);
        }
        let resp = req.send().await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if status.as_u16() == 429 || text.contains("pool_exhausted") {
            return Err(crate::PoolExhausted.into());
        }
        if !status.is_success() {
            anyhow::bail!("executor {path} {status}: {text}");
        }
        serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("{e}: {text}"))
    }
}

#[derive(Serialize)]
struct AllocBody<'a> {
    tenant_id: &'a str,
    session_id: &'a str,
}

#[derive(Serialize)]
struct HandleBody<'a> {
    handle: &'a str,
}

#[derive(Serialize)]
struct ExecBody<'a> {
    handle: &'a str,
    command: &'a str,
    timeout_secs: Option<u64>,
}

#[derive(Serialize)]
struct FsReadBody<'a> {
    handle: &'a str,
    path: &'a str,
    offset: Option<u64>,
    limit: Option<u64>,
}

#[derive(Serialize)]
struct FsWriteBody<'a> {
    handle: &'a str,
    path: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct FsEditBody<'a> {
    handle: &'a str,
    path: &'a str,
    arguments: &'a serde_json::Value,
}

#[derive(Serialize)]
struct GlobBody<'a> {
    handle: &'a str,
    pattern: &'a str,
    path: Option<&'a str>,
}

#[derive(Serialize)]
struct GrepBody<'a> {
    handle: &'a str,
    pattern: &'a str,
    path: Option<&'a str>,
    include: Option<&'a str>,
    max_results: Option<u64>,
}

#[derive(Serialize)]
struct BootstrapBody<'a> {
    handle: &'a str,
    kind: &'a BootstrapKind,
}

#[async_trait]
impl Executor for HttpExecutor {
    fn backend_name(&self) -> &str {
        "remote-http"
    }

    async fn alloc(&self, tenant: &str, session: &str) -> anyhow::Result<WorkspaceHandle> {
        self.post(
            "/v1/alloc",
            &AllocBody {
                tenant_id: tenant,
                session_id: session,
            },
        )
        .await
    }

    async fn release(&self, handle: &WorkspaceHandle) -> anyhow::Result<()> {
        let _: serde_json::Value = self
            .post("/v1/release", &HandleBody { handle: &handle.id })
            .await?;
        Ok(())
    }

    async fn destroy(&self, handle: &WorkspaceHandle) -> anyhow::Result<()> {
        let _: serde_json::Value = self
            .post("/v1/destroy", &HandleBody { handle: &handle.id })
            .await?;
        Ok(())
    }

    async fn exec(&self, handle: &WorkspaceHandle, req: ExecRequest) -> anyhow::Result<ExecResult> {
        self.post(
            "/v1/exec",
            &ExecBody {
                handle: &handle.id,
                command: &req.command,
                timeout_secs: req.timeout_secs,
            },
        )
        .await
    }

    async fn fs_read(
        &self,
        handle: &WorkspaceHandle,
        req: FsReadRequest,
    ) -> anyhow::Result<ToolText> {
        self.post(
            "/v1/fs/read",
            &FsReadBody {
                handle: &handle.id,
                path: &req.path,
                offset: req.offset,
                limit: req.limit,
            },
        )
        .await
    }

    async fn fs_write(
        &self,
        handle: &WorkspaceHandle,
        req: FsWriteRequest,
    ) -> anyhow::Result<ToolText> {
        self.post(
            "/v1/fs/write",
            &FsWriteBody {
                handle: &handle.id,
                path: &req.path,
                content: &req.content,
            },
        )
        .await
    }

    async fn fs_edit(
        &self,
        handle: &WorkspaceHandle,
        req: FsEditRequest,
    ) -> anyhow::Result<ToolText> {
        self.post(
            "/v1/fs/edit",
            &FsEditBody {
                handle: &handle.id,
                path: &req.path,
                arguments: &req.arguments,
            },
        )
        .await
    }

    async fn glob(&self, handle: &WorkspaceHandle, req: GlobRequest) -> anyhow::Result<ToolText> {
        self.post(
            "/v1/glob",
            &GlobBody {
                handle: &handle.id,
                pattern: &req.pattern,
                path: req.path.as_deref(),
            },
        )
        .await
    }

    async fn grep(&self, handle: &WorkspaceHandle, req: GrepRequest) -> anyhow::Result<ToolText> {
        self.post(
            "/v1/grep",
            &GrepBody {
                handle: &handle.id,
                pattern: &req.pattern,
                path: req.path.as_deref(),
                include: req.include.as_deref(),
                max_results: req.max_results,
            },
        )
        .await
    }

    async fn bootstrap(
        &self,
        handle: &WorkspaceHandle,
        kind: BootstrapKind,
    ) -> anyhow::Result<ToolText> {
        self.post(
            "/v1/bootstrap",
            &BootstrapBody {
                handle: &handle.id,
                kind: &kind,
            },
        )
        .await
    }

    async fn snapshot(&self, handle: &WorkspaceHandle) -> anyhow::Result<Vec<u8>> {
        #[derive(serde::Deserialize)]
        struct Out {
            archive_b64: String,
        }
        let out: Out = self
            .post("/v1/snapshot", &HandleBody { handle: &handle.id })
            .await?;
        crate::store::b64_decode(&out.archive_b64)
    }

    async fn restore(&self, handle: &WorkspaceHandle, blob: &[u8]) -> anyhow::Result<()> {
        #[derive(Serialize)]
        struct In<'a> {
            handle: &'a str,
            archive_b64: String,
        }
        let _: serde_json::Value = self
            .post(
                "/v1/restore",
                &In {
                    handle: &handle.id,
                    archive_b64: crate::store::b64_encode(blob),
                },
            )
            .await?;
        Ok(())
    }

    async fn stats(&self) -> anyhow::Result<crate::ExecutorStats> {
        let url = format!("{}/v1/stats", self.base);
        let mut req = self.client.get(&url);
        if !self.token.is_empty() {
            req = req.bearer_auth(&self.token);
        }
        let resp = req.send().await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("executor /v1/stats {status}: {text}");
        }
        serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("{e}: {text}"))
    }
}
