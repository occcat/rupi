//! 控制面侧：sandbox 集群客户端。走 `/v1/sandboxes`，不是 execd `/v1/alloc`。
//! 本进程不 `sh -c`。

use crate::{
    BootstrapKind, ExecRequest, ExecResult, Executor, FsEditRequest, FsReadRequest, FsWriteRequest,
    GlobRequest, GrepRequest, ToolText, WorkspaceHandle,
};
use async_trait::async_trait;
use serde::Serialize;

#[derive(Clone)]
pub struct SandboxExecutor {
    base: String,
    token: String,
    client: reqwest::Client,
}

impl SandboxExecutor {
    pub fn new(base: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            token: token.into(),
            client: reqwest::Client::new(),
        }
    }

    async fn send<T: Serialize, R: serde::de::DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&T>,
    ) -> anyhow::Result<R> {
        let url = format!("{}{path}", self.base);
        let mut req = self.client.request(method, &url);
        if !self.token.is_empty() {
            req = req.bearer_auth(&self.token);
        }
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req.send().await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if status.as_u16() == 429 || text.contains("pool_exhausted") {
            return Err(crate::PoolExhausted.into());
        }
        if !status.is_success() {
            anyhow::bail!("sandbox {path} {status}: {text}");
        }
        serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("{e}: {text}"))
    }

    async fn post<T: Serialize, R: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &T,
    ) -> anyhow::Result<R> {
        self.send(reqwest::Method::POST, path, Some(body)).await
    }
}

#[async_trait]
impl Executor for SandboxExecutor {
    fn backend_name(&self) -> &str {
        "sandbox"
    }

    async fn alloc(&self, tenant: &str, session: &str) -> anyhow::Result<WorkspaceHandle> {
        #[derive(Serialize)]
        struct In<'a> {
            tenant_id: &'a str,
            session_id: &'a str,
            image: &'static str,
        }
        let mut h: WorkspaceHandle = self
            .post(
                "/v1/sandboxes",
                &In {
                    tenant_id: tenant,
                    session_id: session,
                    image: "default",
                },
            )
            .await?;
        if h.kind.is_none() {
            h.kind = Some("sandbox".into());
        }
        if h.backend.is_empty() {
            h.backend = "sandbox".into();
        }
        Ok(h)
    }

    async fn release(&self, handle: &WorkspaceHandle) -> anyhow::Result<()> {
        let _: serde_json::Value = self
            .post(
                &format!("/v1/sandboxes/{}/release", handle.id),
                &serde_json::json!({}),
            )
            .await?;
        Ok(())
    }

    async fn destroy(&self, handle: &WorkspaceHandle) -> anyhow::Result<()> {
        let _: serde_json::Value = self
            .send::<serde_json::Value, _>(
                reqwest::Method::DELETE,
                &format!("/v1/sandboxes/{}", handle.id),
                None,
            )
            .await?;
        Ok(())
    }

    async fn exec(&self, handle: &WorkspaceHandle, req: ExecRequest) -> anyhow::Result<ExecResult> {
        #[derive(Serialize)]
        struct In<'a> {
            command: &'a str,
            timeout_secs: Option<u64>,
        }
        self.post(
            &format!("/v1/sandboxes/{}/exec", handle.id),
            &In {
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
            &format!("/v1/sandboxes/{}/fs/read", handle.id),
            &serde_json::json!({
                "path": req.path,
                "offset": req.offset,
                "limit": req.limit
            }),
        )
        .await
    }

    async fn fs_write(
        &self,
        handle: &WorkspaceHandle,
        req: FsWriteRequest,
    ) -> anyhow::Result<ToolText> {
        self.post(
            &format!("/v1/sandboxes/{}/fs/write", handle.id),
            &serde_json::json!({"path": req.path, "content": req.content}),
        )
        .await
    }

    async fn fs_edit(
        &self,
        handle: &WorkspaceHandle,
        req: FsEditRequest,
    ) -> anyhow::Result<ToolText> {
        self.post(
            &format!("/v1/sandboxes/{}/fs/edit", handle.id),
            &serde_json::json!({"path": req.path, "arguments": req.arguments}),
        )
        .await
    }

    async fn glob(&self, handle: &WorkspaceHandle, req: GlobRequest) -> anyhow::Result<ToolText> {
        self.post(
            &format!("/v1/sandboxes/{}/glob", handle.id),
            &serde_json::json!({"pattern": req.pattern, "path": req.path}),
        )
        .await
    }

    async fn grep(&self, handle: &WorkspaceHandle, req: GrepRequest) -> anyhow::Result<ToolText> {
        self.post(
            &format!("/v1/sandboxes/{}/grep", handle.id),
            &serde_json::json!({
                "pattern": req.pattern,
                "path": req.path,
                "include": req.include,
                "max_results": req.max_results
            }),
        )
        .await
    }

    async fn bootstrap(
        &self,
        handle: &WorkspaceHandle,
        kind: BootstrapKind,
    ) -> anyhow::Result<ToolText> {
        self.post(
            &format!("/v1/sandboxes/{}/bootstrap", handle.id),
            &serde_json::json!({"kind": kind}),
        )
        .await
    }

    async fn snapshot(&self, handle: &WorkspaceHandle) -> anyhow::Result<Vec<u8>> {
        #[derive(serde::Deserialize)]
        struct Out {
            archive_b64: String,
        }
        let out: Out = self
            .post(
                &format!("/v1/sandboxes/{}/snapshot", handle.id),
                &serde_json::json!({}),
            )
            .await?;
        crate::store::b64_decode(&out.archive_b64)
    }

    async fn restore(&self, handle: &WorkspaceHandle, blob: &[u8]) -> anyhow::Result<()> {
        let _: serde_json::Value = self
            .post(
                &format!("/v1/sandboxes/{}/restore", handle.id),
                &serde_json::json!({"archive_b64": crate::store::b64_encode(blob)}),
            )
            .await?;
        Ok(())
    }

    async fn stats(&self) -> anyhow::Result<crate::ExecutorStats> {
        let url = format!("{}/v1/cluster", self.base);
        let mut req = self.client.get(&url);
        if !self.token.is_empty() {
            req = req.bearer_auth(&self.token);
        }
        let resp = req.send().await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("sandbox /v1/cluster {status}: {text}");
        }
        serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("{e}: {text}"))
    }
}
