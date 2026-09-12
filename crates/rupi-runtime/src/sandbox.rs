//! `rupi-sandboxd`：外部 sandbox 集群 API（与 `rupi-execd` 协议并列、可插拔）。
//!
//! 控制面只认 HTTP；本进程才碰工作区与命令。不是本机 Docker 默认执行面。

use crate::engine::{Engine, EngineConfig, EngineError};
use crate::{store, BootstrapKind, ExecResult, ExecutorStats, ToolText};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;

#[derive(Clone)]
pub struct SandboxdConfig {
    pub bind: String,
    pub root: PathBuf,
    pub token: String,
    pub max_sandboxes: u32,
    pub warm_pool: u32,
    pub region: String,
}

impl Default for SandboxdConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8190".into(),
            root: PathBuf::from("/tmp/rupi-sandboxd"),
            token: String::new(),
            max_sandboxes: 64,
            warm_pool: 2,
            region: "local".into(),
        }
    }
}

struct Inner {
    cfg: SandboxdConfig,
    engine: Engine,
}

#[derive(Serialize)]
struct SandboxCreated {
    id: String,
    backend: String,
    kind: String,
    region: String,
}

pub async fn serve(cfg: SandboxdConfig) -> anyhow::Result<()> {
    let (_addr, handle) = spawn(cfg).await?;
    handle.await??;
    Ok(())
}

pub async fn spawn(
    cfg: SandboxdConfig,
) -> anyhow::Result<(std::net::SocketAddr, tokio::task::JoinHandle<anyhow::Result<()>>)> {
    let listener = TcpListener::bind(&cfg.bind).await?;
    let addr = listener.local_addr()?;
    tracing::info!("rupi-sandboxd listen {addr} region={}", cfg.region);
    let app = router(cfg);
    let handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .map_err(|e| anyhow::anyhow!(e))
    });
    Ok((addr, handle))
}

pub fn router(cfg: SandboxdConfig) -> Router {
    let engine = Engine::new(EngineConfig {
        root: cfg.root.clone(),
        max_workspaces: cfg.max_sandboxes,
        warm_pool: cfg.warm_pool,
    });
    let state = Arc::new(Inner { cfg, engine });
    let warm_state = state.clone();
    tokio::spawn(async move {
        warm_state.engine.refill_warm().await;
    });
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/v1/cluster", get(cluster))
        .route("/v1/sandboxes", post(create))
        .route("/v1/sandboxes/{id}", delete(destroy))
        .route("/v1/sandboxes/{id}/release", post(release))
        .route("/v1/sandboxes/{id}/exec", post(exec))
        .route("/v1/sandboxes/{id}/fs/read", post(fs_read))
        .route("/v1/sandboxes/{id}/fs/write", post(fs_write))
        .route("/v1/sandboxes/{id}/fs/edit", post(fs_edit))
        .route("/v1/sandboxes/{id}/glob", post(glob))
        .route("/v1/sandboxes/{id}/grep", post(grep))
        .route("/v1/sandboxes/{id}/bootstrap", post(bootstrap))
        .route("/v1/sandboxes/{id}/snapshot", post(snapshot))
        .route("/v1/sandboxes/{id}/restore", post(restore))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

fn auth_ok(state: &Inner, headers: &HeaderMap) -> bool {
    if state.cfg.token.is_empty() {
        return true;
    }
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| t == state.cfg.token)
}

fn deny() -> (StatusCode, String) {
    (StatusCode::UNAUTHORIZED, "unauthorized".into())
}

fn map_err(e: EngineError) -> (StatusCode, String) {
    match e {
        EngineError::Exhausted => (StatusCode::TOO_MANY_REQUESTS, "pool_exhausted".into()),
        EngineError::NotFound => (StatusCode::NOT_FOUND, "unknown sandbox".into()),
        EngineError::Other(s) => (StatusCode::INTERNAL_SERVER_ERROR, s),
    }
}

async fn cluster(
    State(state): State<Arc<Inner>>,
    headers: HeaderMap,
) -> Result<Json<ExecutorStats>, (StatusCode, String)> {
    if !auth_ok(&state, &headers) {
        return Err(deny());
    }
    let (used, capacity, warm) = state.engine.stats().await;
    Ok(Json(ExecutorStats {
        backend: "sandbox".into(),
        node_id: state.cfg.bind.clone(),
        used,
        capacity,
        warm,
        region: Some(state.cfg.region.clone()),
        kind: Some("sandbox".into()),
    }))
}

#[derive(Deserialize)]
struct CreateIn {
    tenant_id: String,
    session_id: String,
    #[serde(default)]
    image: Option<String>,
}

async fn create(
    State(state): State<Arc<Inner>>,
    headers: HeaderMap,
    Json(body): Json<CreateIn>,
) -> Result<Json<SandboxCreated>, (StatusCode, String)> {
    if !auth_ok(&state, &headers) {
        return Err(deny());
    }
    let _ = body.image;
    let lease = state
        .engine
        .alloc(&body.tenant_id, &body.session_id)
        .await
        .map_err(map_err)?;
    let refill = state.clone();
    tokio::spawn(async move {
        refill.engine.refill_warm().await;
    });
    Ok(Json(SandboxCreated {
        id: lease.id,
        backend: "sandbox".into(),
        kind: "sandbox".into(),
        region: state.cfg.region.clone(),
    }))
}

async fn destroy(
    State(state): State<Arc<Inner>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !auth_ok(&state, &headers) {
        return Err(deny());
    }
    state.engine.destroy(&id).await.map_err(map_err)?;
    Ok(Json(serde_json::json!({"ok": true})))
}

async fn release(
    State(state): State<Arc<Inner>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !auth_ok(&state, &headers) {
        return Err(deny());
    }
    state.engine.release(&id).await.map_err(map_err)?;
    Ok(Json(serde_json::json!({"ok": true})))
}

#[derive(Deserialize)]
struct ExecIn {
    command: String,
    timeout_secs: Option<u64>,
}

async fn exec(
    State(state): State<Arc<Inner>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<ExecIn>,
) -> Result<Json<ExecResult>, (StatusCode, String)> {
    if !auth_ok(&state, &headers) {
        return Err(deny());
    }
    Ok(Json(
        state
            .engine
            .exec(&id, &body.command, body.timeout_secs)
            .await
            .map_err(map_err)?,
    ))
}

#[derive(Deserialize)]
struct FsReadIn {
    path: String,
    offset: Option<u64>,
    limit: Option<u64>,
}

async fn fs_read(
    State(state): State<Arc<Inner>>,
    headers: HeaderMap,
    Path(id): Path<String>,
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
    Ok(Json(
        state
            .engine
            .fs_tool(&id, "read", args)
            .await
            .map_err(map_err)?,
    ))
}

#[derive(Deserialize)]
struct FsWriteIn {
    path: String,
    content: String,
}

async fn fs_write(
    State(state): State<Arc<Inner>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<FsWriteIn>,
) -> Result<Json<ToolText>, (StatusCode, String)> {
    if !auth_ok(&state, &headers) {
        return Err(deny());
    }
    Ok(Json(
        state
            .engine
            .fs_tool(
                &id,
                "write",
                serde_json::json!({"path": body.path, "content": body.content}),
            )
            .await
            .map_err(map_err)?,
    ))
}

#[derive(Deserialize)]
struct FsEditIn {
    path: String,
    arguments: serde_json::Value,
}

async fn fs_edit(
    State(state): State<Arc<Inner>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<FsEditIn>,
) -> Result<Json<ToolText>, (StatusCode, String)> {
    if !auth_ok(&state, &headers) {
        return Err(deny());
    }
    let mut args = body.arguments;
    if args.get("path").is_none() {
        args["path"] = serde_json::Value::String(body.path);
    }
    Ok(Json(
        state
            .engine
            .fs_tool(&id, "edit", args)
            .await
            .map_err(map_err)?,
    ))
}

#[derive(Deserialize)]
struct GlobIn {
    pattern: String,
    path: Option<String>,
}

async fn glob(
    State(state): State<Arc<Inner>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<GlobIn>,
) -> Result<Json<ToolText>, (StatusCode, String)> {
    if !auth_ok(&state, &headers) {
        return Err(deny());
    }
    let dir = state.engine.dir(&id).await.map_err(map_err)?;
    let mut args = serde_json::json!({"pattern": body.pattern});
    args["path"] = serde_json::Value::String(body.path.unwrap_or_else(|| dir.display().to_string()));
    Ok(Json(
        state
            .engine
            .fs_tool(&id, "glob", args)
            .await
            .map_err(map_err)?,
    ))
}

#[derive(Deserialize)]
struct GrepIn {
    pattern: String,
    path: Option<String>,
    include: Option<String>,
    max_results: Option<u64>,
}

async fn grep(
    State(state): State<Arc<Inner>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<GrepIn>,
) -> Result<Json<ToolText>, (StatusCode, String)> {
    if !auth_ok(&state, &headers) {
        return Err(deny());
    }
    let dir = state.engine.dir(&id).await.map_err(map_err)?;
    let mut args = serde_json::json!({"pattern": body.pattern});
    args["path"] = serde_json::Value::String(body.path.unwrap_or_else(|| dir.display().to_string()));
    if let Some(inc) = body.include {
        args["include"] = inc.into();
    }
    if let Some(m) = body.max_results {
        args["max_results"] = m.into();
    }
    Ok(Json(
        state
            .engine
            .fs_tool(&id, "grep", args)
            .await
            .map_err(map_err)?,
    ))
}

#[derive(Deserialize)]
struct BootstrapIn {
    kind: BootstrapKind,
}

async fn bootstrap(
    State(state): State<Arc<Inner>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<BootstrapIn>,
) -> Result<Json<ToolText>, (StatusCode, String)> {
    if !auth_ok(&state, &headers) {
        return Err(deny());
    }
    Ok(Json(
        state
            .engine
            .bootstrap(&id, &body.kind)
            .await
            .map_err(map_err)?,
    ))
}

async fn snapshot(
    State(state): State<Arc<Inner>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !auth_ok(&state, &headers) {
        return Err(deny());
    }
    let bytes = state.engine.snapshot(&id).await.map_err(map_err)?;
    Ok(Json(serde_json::json!({
        "bytes": bytes.len(),
        "archive_b64": store::b64_encode(&bytes),
    })))
}

#[derive(Deserialize)]
struct RestoreIn {
    archive_b64: String,
}

async fn restore(
    State(state): State<Arc<Inner>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<RestoreIn>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !auth_ok(&state, &headers) {
        return Err(deny());
    }
    let bytes = store::b64_decode(&body.archive_b64)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    state.engine.restore(&id, &bytes).await.map_err(map_err)?;
    Ok(Json(serde_json::json!({"ok": true, "bytes": bytes.len()})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox_http::SandboxExecutor;
    use crate::{AllocRequest, BackendKind, Executor, FsWriteRequest};

    #[tokio::test]
    async fn sandbox_api_is_not_execd() {
        let root = std::env::temp_dir().join(format!("rupi-sb-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let (addr, _h) = spawn(SandboxdConfig {
            bind: "127.0.0.1:0".into(),
            root: root.clone(),
            token: "sb".into(),
            max_sandboxes: 2,
            warm_pool: 0,
            region: "eu-west".into(),
        })
        .await
        .unwrap();
        let exec = SandboxExecutor::new(format!("http://{addr}"), "sb");
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert_eq!(exec.backend_name(), "sandbox");
        let h = exec
            .alloc_pref(&AllocRequest::new("ten", "s1").with_kind(BackendKind::Sandbox))
            .await
            .unwrap();
        assert_eq!(h.kind.as_deref(), Some("sandbox"));
        assert_eq!(h.region.as_deref(), Some("eu-west"));
        exec.fs_write(
            &h,
            FsWriteRequest {
                path: "box.txt".into(),
                content: "sandbox-ok".into(),
            },
        )
        .await
        .unwrap();
        let got = exec
            .fs_read(
                &h,
                crate::FsReadRequest {
                    path: "box.txt".into(),
                    offset: None,
                    limit: None,
                },
            )
            .await
            .unwrap();
        assert!(got.content.contains("sandbox-ok"), "{}", got.content);
        let st = exec.stats().await.unwrap();
        assert_eq!(st.backend, "sandbox");
        assert_eq!(st.used, 1);
        // 协议面：sandbox 没有 /v1/alloc（那是 execd）。
        let miss = reqwest::Client::new()
            .post(format!("http://{addr}/v1/alloc"))
            .bearer_auth("sb")
            .json(&serde_json::json!({"tenant_id":"t","session_id":"s"}))
            .send()
            .await
            .unwrap();
        assert_eq!(miss.status().as_u16(), 404, "sandboxd must not speak execd /v1/alloc");
        exec.destroy(&h).await.unwrap();
        let _ = std::fs::remove_dir_all(root);
    }
}
