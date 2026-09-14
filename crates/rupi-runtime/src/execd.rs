//! `rupi-execd`：平台登记的远程执行进程。工作区与 `sh -c` 只出现在这里。

use crate::engine::{Engine, EngineConfig, EngineError};
use crate::jail::Isolation;
use crate::token::{tokens_eq, validate_listen_token};
use crate::{store, BootstrapKind, ExecResult, ExecutorStats, ToolText, WorkspaceHandle};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;

#[derive(Clone)]
pub struct ExecdConfig {
    pub bind: String,
    pub root: PathBuf,
    pub token: String,
    /// 同时租出的工作区上限。满了 `alloc` 返回 429。
    pub max_workspaces: u32,
    /// 预热空仓数量（不计入已租出，但 `used + warm <= capacity`）。
    pub warm_pool: u32,
    /// 空 token 仅回环 + 本开关。
    pub insecure: bool,
}

impl Default for ExecdConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8090".into(),
            root: PathBuf::from("/tmp/rupi-execd"),
            token: String::new(),
            max_workspaces: 64,
            warm_pool: 2,
            insecure: false,
        }
    }
}

struct Inner {
    cfg: ExecdConfig,
    engine: Engine,
}

pub async fn serve(cfg: ExecdConfig) -> anyhow::Result<()> {
    let (_addr, handle) = spawn(cfg).await?;
    handle.await??;
    Ok(())
}

/// 绑定并在后台跑；测试与控制面拉起时用。
pub async fn spawn(
    cfg: ExecdConfig,
) -> anyhow::Result<(
    std::net::SocketAddr,
    tokio::task::JoinHandle<anyhow::Result<()>>,
)> {
    validate_listen_token(&cfg.bind, &cfg.token, cfg.insecure).map_err(|e| anyhow::anyhow!(e))?;
    std::fs::create_dir_all(&cfg.root).map_err(|e| {
        anyhow::anyhow!(
            "cannot create execd --root {}: {e} (loopback: use a writable path such as ./rupi-data/execd)",
            cfg.root.display()
        )
    })?;
    let listener = TcpListener::bind(&cfg.bind).await?;
    let addr = listener.local_addr()?;
    tracing::info!("rupi-execd listen {addr}");
    let app = router(cfg);
    let handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .map_err(|e| anyhow::anyhow!(e))
    });
    Ok((addr, handle))
}

pub fn router(cfg: ExecdConfig) -> Router {
    let engine = Engine::new(EngineConfig {
        root: cfg.root.clone(),
        max_workspaces: cfg.max_workspaces,
        warm_pool: cfg.warm_pool,
        isolation: Isolation::Jail,
    });
    let state = Arc::new(Inner { cfg, engine });
    let warm_state = state.clone();
    tokio::spawn(async move {
        warm_state.engine.refill_warm().await;
    });
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/v1/stats", get(stats))
        .route("/v1/alloc", post(alloc))
        .route("/v1/release", post(release))
        .route("/v1/destroy", post(destroy))
        .route("/v1/exec", post(exec))
        .route("/v1/abort", post(abort))
        .route("/v1/fs/read", post(fs_read))
        .route("/v1/fs/write", post(fs_write))
        .route("/v1/fs/edit", post(fs_edit))
        .route("/v1/glob", post(glob))
        .route("/v1/grep", post(grep))
        .route("/v1/bootstrap", post(bootstrap))
        .route("/v1/snapshot", post(snapshot))
        .route("/v1/restore", post(restore))
        .layer(crate::access_trace_layer())
        .with_state(state)
}

fn auth_ok(state: &Inner, headers: &HeaderMap) -> bool {
    if state.cfg.token.is_empty() {
        return state.cfg.insecure && crate::token::is_loopback_bind(&state.cfg.bind);
    }
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| tokens_eq(t, &state.cfg.token))
}

fn deny() -> (StatusCode, String) {
    (StatusCode::UNAUTHORIZED, "unauthorized".into())
}

fn map_err(e: EngineError) -> (StatusCode, String) {
    match e {
        EngineError::Exhausted => (StatusCode::TOO_MANY_REQUESTS, "pool_exhausted".into()),
        EngineError::NotFound => (StatusCode::NOT_FOUND, "unknown handle".into()),
        EngineError::Forbidden => (StatusCode::FORBIDDEN, "tenant mismatch".into()),
        EngineError::Other(s) => (StatusCode::INTERNAL_SERVER_ERROR, s),
    }
}

fn tenant_of(body_tenant: Option<&str>) -> Result<&str, (StatusCode, String)> {
    match body_tenant.map(str::trim).filter(|s| !s.is_empty()) {
        Some(t) => Ok(t),
        None => Err((StatusCode::FORBIDDEN, "tenant_id required".into())),
    }
}

async fn stats(
    State(state): State<Arc<Inner>>,
    headers: HeaderMap,
) -> Result<Json<ExecutorStats>, (StatusCode, String)> {
    if !auth_ok(&state, &headers) {
        return Err(deny());
    }
    let (used, capacity, warm) = state.engine.stats().await;
    Ok(Json(ExecutorStats {
        backend: "remote-http".into(),
        node_id: state.cfg.bind.clone(),
        used,
        capacity,
        warm,
        region: None,
        kind: Some("remote-http".into()),
    }))
}

#[derive(Deserialize)]
struct AllocIn {
    tenant_id: String,
    session_id: String,
}

async fn alloc(
    State(state): State<Arc<Inner>>,
    headers: HeaderMap,
    Json(body): Json<AllocIn>,
) -> Result<Json<WorkspaceHandle>, (StatusCode, String)> {
    if !auth_ok(&state, &headers) {
        return Err(deny());
    }
    let lease = state
        .engine
        .alloc(&body.tenant_id, &body.session_id)
        .await
        .map_err(map_err)?;
    let refill = state.clone();
    tokio::spawn(async move {
        refill.engine.refill_warm().await;
    });
    Ok(Json(WorkspaceHandle {
        id: lease.id,
        backend: "remote-http".into(),
        tenant_id: Some(body.tenant_id),
        region: None,
        kind: Some("remote-http".into()),
    }))
}

#[derive(Deserialize)]
struct HandleIn {
    handle: String,
    #[serde(default)]
    tenant_id: Option<String>,
}

async fn release(
    State(state): State<Arc<Inner>>,
    headers: HeaderMap,
    Json(body): Json<HandleIn>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !auth_ok(&state, &headers) {
        return Err(deny());
    }
    let t = tenant_of(body.tenant_id.as_deref())?;
    state
        .engine
        .require_tenant(&body.handle, t)
        .await
        .map_err(map_err)?;
    state.engine.release(&body.handle).await.map_err(map_err)?;
    Ok(Json(serde_json::json!({"ok": true})))
}

async fn destroy(
    State(state): State<Arc<Inner>>,
    headers: HeaderMap,
    Json(body): Json<HandleIn>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !auth_ok(&state, &headers) {
        return Err(deny());
    }
    let t = tenant_of(body.tenant_id.as_deref())?;
    state
        .engine
        .require_tenant(&body.handle, t)
        .await
        .map_err(map_err)?;
    state.engine.destroy(&body.handle).await.map_err(map_err)?;
    Ok(Json(serde_json::json!({"ok": true})))
}

#[derive(Deserialize)]
struct ExecIn {
    handle: String,
    #[serde(default)]
    tenant_id: Option<String>,
    command: String,
    timeout_secs: Option<u64>,
}

async fn exec(
    State(state): State<Arc<Inner>>,
    headers: HeaderMap,
    Json(body): Json<ExecIn>,
) -> Result<Json<ExecResult>, (StatusCode, String)> {
    if !auth_ok(&state, &headers) {
        return Err(deny());
    }
    let t = tenant_of(body.tenant_id.as_deref())?;
    Ok(Json(
        state
            .engine
            .exec_for(&body.handle, Some(t), &body.command, body.timeout_secs)
            .await
            .map_err(map_err)?,
    ))
}

async fn abort(
    State(state): State<Arc<Inner>>,
    headers: HeaderMap,
    Json(body): Json<HandleIn>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !auth_ok(&state, &headers) {
        return Err(deny());
    }
    let t = tenant_of(body.tenant_id.as_deref())?;
    state
        .engine
        .require_tenant(&body.handle, t)
        .await
        .map_err(map_err)?;
    state.engine.abort(&body.handle).await;
    Ok(Json(serde_json::json!({"ok": true})))
}

#[derive(Deserialize)]
struct FsReadIn {
    handle: String,
    #[serde(default)]
    tenant_id: Option<String>,
    path: String,
    offset: Option<u64>,
    limit: Option<u64>,
}

async fn fs_read(
    State(state): State<Arc<Inner>>,
    headers: HeaderMap,
    Json(body): Json<FsReadIn>,
) -> Result<Json<ToolText>, (StatusCode, String)> {
    if !auth_ok(&state, &headers) {
        return Err(deny());
    }
    let mut args = serde_json::json!({"path": body.path});
    if let Some(o) = body.offset {
        args["offset"] = o.into();
    }
    if let Some(l) = body.limit {
        args["limit"] = l.into();
    }
    let t = tenant_of(body.tenant_id.as_deref())?;
    Ok(Json(
        state
            .engine
            .fs_tool_for(&body.handle, Some(t), "read", args)
            .await
            .map_err(map_err)?,
    ))
}

#[derive(Deserialize)]
struct FsWriteIn {
    handle: String,
    #[serde(default)]
    tenant_id: Option<String>,
    path: String,
    content: String,
}

async fn fs_write(
    State(state): State<Arc<Inner>>,
    headers: HeaderMap,
    Json(body): Json<FsWriteIn>,
) -> Result<Json<ToolText>, (StatusCode, String)> {
    if !auth_ok(&state, &headers) {
        return Err(deny());
    }
    let t = tenant_of(body.tenant_id.as_deref())?;
    Ok(Json(
        state
            .engine
            .fs_tool_for(
                &body.handle,
                Some(t),
                "write",
                serde_json::json!({"path": body.path, "content": body.content}),
            )
            .await
            .map_err(map_err)?,
    ))
}

#[derive(Deserialize)]
struct FsEditIn {
    handle: String,
    #[serde(default)]
    tenant_id: Option<String>,
    path: String,
    arguments: serde_json::Value,
}

async fn fs_edit(
    State(state): State<Arc<Inner>>,
    headers: HeaderMap,
    Json(body): Json<FsEditIn>,
) -> Result<Json<ToolText>, (StatusCode, String)> {
    if !auth_ok(&state, &headers) {
        return Err(deny());
    }
    let mut args = body.arguments;
    if args.get("path").is_none() {
        args["path"] = serde_json::Value::String(body.path);
    }
    let t = tenant_of(body.tenant_id.as_deref())?;
    Ok(Json(
        state
            .engine
            .fs_tool_for(&body.handle, Some(t), "edit", args)
            .await
            .map_err(map_err)?,
    ))
}

#[derive(Deserialize)]
struct GlobIn {
    handle: String,
    #[serde(default)]
    tenant_id: Option<String>,
    pattern: String,
    path: Option<String>,
}

async fn glob(
    State(state): State<Arc<Inner>>,
    headers: HeaderMap,
    Json(body): Json<GlobIn>,
) -> Result<Json<ToolText>, (StatusCode, String)> {
    if !auth_ok(&state, &headers) {
        return Err(deny());
    }
    let t = tenant_of(body.tenant_id.as_deref())?;
    let dir = state
        .engine
        .require_tenant(&body.handle, t)
        .await
        .map_err(map_err)?;
    let mut args = serde_json::json!({"pattern": body.pattern});
    args["path"] =
        serde_json::Value::String(body.path.unwrap_or_else(|| dir.display().to_string()));
    Ok(Json(
        state
            .engine
            .fs_tool_for(&body.handle, Some(t), "glob", args)
            .await
            .map_err(map_err)?,
    ))
}

#[derive(Deserialize)]
struct GrepIn {
    handle: String,
    #[serde(default)]
    tenant_id: Option<String>,
    pattern: String,
    path: Option<String>,
    include: Option<String>,
    max_results: Option<u64>,
}

async fn grep(
    State(state): State<Arc<Inner>>,
    headers: HeaderMap,
    Json(body): Json<GrepIn>,
) -> Result<Json<ToolText>, (StatusCode, String)> {
    if !auth_ok(&state, &headers) {
        return Err(deny());
    }
    let t = tenant_of(body.tenant_id.as_deref())?;
    let dir = state
        .engine
        .require_tenant(&body.handle, t)
        .await
        .map_err(map_err)?;
    let mut args = serde_json::json!({"pattern": body.pattern});
    args["path"] =
        serde_json::Value::String(body.path.unwrap_or_else(|| dir.display().to_string()));
    if let Some(inc) = body.include {
        args["include"] = inc.into();
    }
    if let Some(m) = body.max_results {
        args["max_results"] = m.into();
    }
    Ok(Json(
        state
            .engine
            .fs_tool_for(&body.handle, Some(t), "grep", args)
            .await
            .map_err(map_err)?,
    ))
}

#[derive(Deserialize)]
struct BootstrapIn {
    handle: String,
    #[serde(default)]
    tenant_id: Option<String>,
    kind: BootstrapKind,
}

async fn bootstrap(
    State(state): State<Arc<Inner>>,
    headers: HeaderMap,
    Json(body): Json<BootstrapIn>,
) -> Result<Json<ToolText>, (StatusCode, String)> {
    if !auth_ok(&state, &headers) {
        return Err(deny());
    }
    let t = tenant_of(body.tenant_id.as_deref())?;
    Ok(Json(
        state
            .engine
            .bootstrap_for(&body.handle, Some(t), &body.kind)
            .await
            .map_err(map_err)?,
    ))
}

#[derive(Deserialize)]
struct SnapshotIn {
    handle: String,
    #[serde(default)]
    tenant_id: Option<String>,
}

async fn snapshot(
    State(state): State<Arc<Inner>>,
    headers: HeaderMap,
    Json(body): Json<SnapshotIn>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !auth_ok(&state, &headers) {
        return Err(deny());
    }
    let t = tenant_of(body.tenant_id.as_deref())?;
    let bytes = state
        .engine
        .snapshot_for(&body.handle, Some(t))
        .await
        .map_err(map_err)?;
    Ok(Json(serde_json::json!({
        "bytes": bytes.len(),
        "archive_b64": store::b64_encode(&bytes),
    })))
}

#[derive(Deserialize)]
struct RestoreIn {
    handle: String,
    #[serde(default)]
    tenant_id: Option<String>,
    archive_b64: String,
}

async fn restore(
    State(state): State<Arc<Inner>>,
    headers: HeaderMap,
    Json(body): Json<RestoreIn>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !auth_ok(&state, &headers) {
        return Err(deny());
    }
    let bytes = store::b64_decode(&body.archive_b64)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let t = tenant_of(body.tenant_id.as_deref())?;
    state
        .engine
        .restore_for(&body.handle, Some(t), &bytes)
        .await
        .map_err(map_err)?;
    Ok(Json(serde_json::json!({"ok": true, "bytes": bytes.len()})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::HttpExecutor;
    use crate::{Executor, FsWriteRequest};

    #[tokio::test]
    async fn pool_capacity_snapshot_restore() {
        let root = std::env::temp_dir().join(format!("rupi-execd-scale-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let (addr, _h) = spawn(ExecdConfig {
            bind: "127.0.0.1:0".into(),
            root: root.clone(),
            token: "t".into(),
            max_workspaces: 1,
            warm_pool: 1,
            insecure: false,
        })
        .await
        .unwrap();
        let exec = HttpExecutor::new(format!("http://{addr}"), "t");
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        let a = exec.alloc("ten", "s1").await.unwrap();
        exec.fs_write(
            &a,
            FsWriteRequest {
                path: "keep.txt".into(),
                content: "snap-me".into(),
            },
        )
        .await
        .unwrap();
        let blob = exec.snapshot(&a).await.unwrap();
        assert!(blob.len() > 20);
        assert!(crate::is_pool_exhausted(
            &exec.alloc("ten", "s2").await.unwrap_err()
        ));
        exec.release(&a).await.unwrap();
        let b = exec.alloc("ten", "s2").await.unwrap();
        exec.restore(&b, &blob).await.unwrap();
        let got = exec
            .fs_read(
                &b,
                crate::FsReadRequest {
                    path: "keep.txt".into(),
                    offset: None,
                    limit: None,
                },
            )
            .await
            .unwrap();
        assert!(got.content.contains("snap-me"), "{}", got.content);
        let st = exec.stats().await.unwrap();
        assert_eq!(st.capacity, 1);
        assert_eq!(st.used, 1);
        exec.destroy(&b).await.unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn unwritable_root_mentions_loopback_path() {
        let path = std::env::temp_dir().join(format!("rupi-execd-notdir-{}", uuid::Uuid::new_v4()));
        std::fs::write(&path, b"not a dir").unwrap();
        let err = spawn(ExecdConfig {
            bind: "127.0.0.1:0".into(),
            root: path.clone(),
            token: "t".into(),
            ..Default::default()
        })
        .await
        .unwrap_err();
        let _ = std::fs::remove_file(&path);
        let s = err.to_string();
        assert!(s.contains("./rupi-data/execd"), "{s}");
        assert!(s.contains("cannot create execd --root"), "{s}");
    }

    #[tokio::test]
    async fn empty_token_non_loopback_refused() {
        let err = spawn(ExecdConfig {
            bind: "0.0.0.0:0".into(),
            token: String::new(),
            insecure: true,
            ..Default::default()
        })
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("non-loopback") || err.to_string().contains("token"),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn tenant_mismatch_is_403() {
        let root = std::env::temp_dir().join(format!("rupi-execd-ten-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let (addr, _h) = spawn(ExecdConfig {
            bind: "127.0.0.1:0".into(),
            root: root.clone(),
            token: "t".into(),
            ..Default::default()
        })
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        let exec = HttpExecutor::new(format!("http://{addr}"), "t");
        let a = exec.alloc("ten-a", "s1").await.unwrap();
        let mut stolen = a.clone();
        stolen.tenant_id = Some("ten-b".into());
        let err = exec
            .exec(
                &stolen,
                crate::ExecRequest {
                    command: "echo hi".into(),
                    timeout_secs: Some(2),
                },
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("403") || err.to_string().contains("tenant"),
            "{err:#}"
        );
        exec.destroy(&a).await.unwrap();
        let _ = std::fs::remove_dir_all(root);
    }
}
