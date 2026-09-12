//! 多节点执行池：按各 `rupi-execd` 的容量挑最闲节点。
//! 池满返回 [`PoolExhausted`]，由控制面决定抢占闲置卷或 `429`。

use crate::{
    BootstrapKind, ExecRequest, ExecResult, Executor, ExecutorStats, FsEditRequest, FsReadRequest,
    FsWriteRequest, GlobRequest, GrepRequest, ToolText, WorkspaceHandle,
};
use async_trait::async_trait;
use std::sync::Arc;

/// 执行池耗尽（所有节点 `used >= capacity`）。
#[derive(Debug)]
pub struct PoolExhausted;

impl std::fmt::Display for PoolExhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("execution pool exhausted")
    }
}

impl std::error::Error for PoolExhausted {}

pub fn is_pool_exhausted(err: &anyhow::Error) -> bool {
    err.downcast_ref::<PoolExhausted>().is_some()
        || err.to_string().contains("pool_exhausted")
        || err.to_string().contains("execution pool exhausted")
}

#[derive(Clone)]
pub struct PoolNode {
    pub id: String,
    pub executor: Arc<dyn Executor>,
}

/// 可插拔节点列表。`WorkspaceHandle.backend` 记下节点 id，后续调用按 id 路由。
pub struct PoolScheduler {
    nodes: Vec<PoolNode>,
}

impl PoolScheduler {
    pub fn new(nodes: Vec<PoolNode>) -> Self {
        Self { nodes }
    }

    pub fn single(id: impl Into<String>, executor: Arc<dyn Executor>) -> Self {
        Self {
            nodes: vec![PoolNode {
                id: id.into(),
                executor,
            }],
        }
    }

    pub fn nodes(&self) -> &[PoolNode] {
        &self.nodes
    }

    fn pick(&self, handle: &WorkspaceHandle) -> anyhow::Result<&dyn Executor> {
        if let Some(n) = self.nodes.iter().find(|n| n.id == handle.backend) {
            return Ok(n.executor.as_ref());
        }
        if self.nodes.len() == 1 {
            return Ok(self.nodes[0].executor.as_ref());
        }
        anyhow::bail!("unknown executor node {}", handle.backend)
    }

    async fn ranked(&self) -> Vec<&PoolNode> {
        let mut pairs: Vec<(i64, &PoolNode)> = Vec::new();
        for n in &self.nodes {
            let free = match n.executor.stats().await {
                Ok(s) => s.capacity as i64 - s.used as i64,
                Err(_) => 0,
            };
            pairs.push((free, n));
        }
        pairs.sort_by_key(|(f, _)| std::cmp::Reverse(*f));
        pairs.into_iter().map(|(_, n)| n).collect()
    }
}

#[async_trait]
impl Executor for PoolScheduler {
    fn backend_name(&self) -> &str {
        "pool"
    }

    async fn alloc(&self, tenant: &str, session: &str) -> anyhow::Result<WorkspaceHandle> {
        if self.nodes.is_empty() {
            return Err(PoolExhausted.into());
        }
        let ranked = self.ranked().await;
        let mut last = None;
        for n in ranked {
            match n.executor.alloc(tenant, session).await {
                Ok(mut h) => {
                    h.backend = n.id.clone();
                    return Ok(h);
                }
                Err(e) => {
                    if is_pool_exhausted(&e) {
                        last = Some(e);
                        continue;
                    }
                    return Err(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| PoolExhausted.into()))
    }

    async fn release(&self, handle: &WorkspaceHandle) -> anyhow::Result<()> {
        self.pick(handle)?.release(handle).await
    }

    async fn destroy(&self, handle: &WorkspaceHandle) -> anyhow::Result<()> {
        self.pick(handle)?.destroy(handle).await
    }

    async fn exec(&self, handle: &WorkspaceHandle, req: ExecRequest) -> anyhow::Result<ExecResult> {
        self.pick(handle)?.exec(handle, req).await
    }

    async fn fs_read(
        &self,
        handle: &WorkspaceHandle,
        req: FsReadRequest,
    ) -> anyhow::Result<ToolText> {
        self.pick(handle)?.fs_read(handle, req).await
    }

    async fn fs_write(
        &self,
        handle: &WorkspaceHandle,
        req: FsWriteRequest,
    ) -> anyhow::Result<ToolText> {
        self.pick(handle)?.fs_write(handle, req).await
    }

    async fn fs_edit(
        &self,
        handle: &WorkspaceHandle,
        req: FsEditRequest,
    ) -> anyhow::Result<ToolText> {
        self.pick(handle)?.fs_edit(handle, req).await
    }

    async fn glob(&self, handle: &WorkspaceHandle, req: GlobRequest) -> anyhow::Result<ToolText> {
        self.pick(handle)?.glob(handle, req).await
    }

    async fn grep(&self, handle: &WorkspaceHandle, req: GrepRequest) -> anyhow::Result<ToolText> {
        self.pick(handle)?.grep(handle, req).await
    }

    async fn bootstrap(
        &self,
        handle: &WorkspaceHandle,
        kind: BootstrapKind,
    ) -> anyhow::Result<ToolText> {
        self.pick(handle)?.bootstrap(handle, kind).await
    }

    async fn snapshot(&self, handle: &WorkspaceHandle) -> anyhow::Result<Vec<u8>> {
        self.pick(handle)?.snapshot(handle).await
    }

    async fn restore(&self, handle: &WorkspaceHandle, blob: &[u8]) -> anyhow::Result<()> {
        self.pick(handle)?.restore(handle, blob).await
    }

    async fn stats(&self) -> anyhow::Result<ExecutorStats> {
        let mut used = 0u32;
        let mut capacity = 0u32;
        let mut warm = 0u32;
        for n in &self.nodes {
            if let Ok(s) = n.executor.stats().await {
                used += s.used;
                capacity += s.capacity;
                warm += s.warm;
            }
        }
        Ok(ExecutorStats {
            backend: "pool".into(),
            node_id: format!("{}-nodes", self.nodes.len()),
            used,
            capacity,
            warm,
        })
    }
}
