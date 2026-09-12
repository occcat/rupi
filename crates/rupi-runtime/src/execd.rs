//! `rupi-execd`：平台登记的远程执行进程。工作区与 `sh -c` 只出现在这里。

use crate::{store, BootstrapKind, ExecResult, ExecutorStats, ToolText, WorkspaceHandle};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use uuid::Uuid;

#[derive(Clone)]
pub struct ExecdConfig {
    pub bind: String,
    pub root: PathBuf,
    pub token: String,
    /// 同时租出的工作区上限。满了 `alloc` 返回 429。
    pub max_workspaces: u32,
    /// 预热空仓数量（不计入已租出，但 `used + warm <= capacity`）。
    pub warm_pool: u32,
}

impl Default for ExecdConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8090".into(),
            root: PathBuf::from("/tmp/rupi-execd"),
            token: String::new(),
            max_workspaces: 64,
            warm_pool: 2,
        }
    }
}

struct Slot {
    dir: PathBuf,
    #[allow(dead_code)]
    tenant_id: String,
    #[allow(dead_code)]
    session_id: String,
    last_used: Instant,
}

struct Inner {
    cfg: ExecdConfig,
    leased: Mutex<HashMap<String, Slot>>,
    warm: Mutex<Vec<PathBuf>>,
}

pub async fn serve(cfg: ExecdConfig) -> anyhow::Result<()> {
    let (_addr, handle) = spawn(cfg).await?;
    handle.await??;
    Ok(())
}

/// 绑定并在后台跑；测试与控制面拉起时用。
pub async fn spawn(
    cfg: ExecdConfig,
) -> anyhow::Result<(std::net::SocketAddr, tokio::task::JoinHandle<anyhow::Result<()>>)> {
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
    let state = Arc::new(Inner {
        cfg,
        leased: Mutex::new(HashMap::new()),
        warm: Mutex::new(Vec::new()),
    });
    let warm_state = state.clone();
    tokio::spawn(async move {
        refill_warm(&warm_state).await;
    });
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/v1/stats", get(stats))
        .route("/v1/alloc", post(alloc))
        .route("/v1/release", post(release))
        .route("/v1/destroy", post(destroy))
        .route("/v1/exec", post(exec))
        .route("/v1/fs/read", post(fs_read))
        .route("/v1/fs/write", post(fs_write))
        .route("/v1/fs/edit", post(fs_edit))
        .route("/v1/glob", post(glob))
        .route("/v1/grep", post(grep))
        .route("/v1/bootstrap", post(bootstrap))
        .route("/v1/snapshot", post(snapshot))
        .route("/v1/restore", post(restore))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

async fn refill_warm(state: &Inner) {
    let leased = state.leased.lock().await.len() as u32;
    let mut warm = state.warm.lock().await;
    let cap = state.cfg.max_workspaces.max(1);
    let want = state.cfg.warm_pool.min(cap.saturating_sub(leased));
    while (warm.len() as u32) < want {
        let dir = state.cfg.root.join("_warm").join(Uuid::new_v4().to_string());
        if tokio::fs::create_dir_all(&dir).await.is_ok() {
            warm.push(dir);
        } else {
            break;
        }
    }
}

async fn wipe_dir(dir: &std::path::Path) {
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

async fn workspace(state: &Inner, handle: &str) -> Result<PathBuf, (StatusCode, String)> {
    let mut g = state.leased.lock().await;
    match g.get_mut(handle) {
        Some(s) => {
            s.last_used = Instant::now();
            Ok(s.dir.clone())
        }
        None => Err((StatusCode::NOT_FOUND, "unknown handle".into())),
    }
}

async fn stats(
    State(state): State<Arc<Inner>>,
    headers: HeaderMap,
) -> Result<Json<ExecutorStats>, (StatusCode, String)> {
    if !auth_ok(&state, &headers) {
        return Err(deny());
    }
    let used = state.leased.lock().await.len() as u32;
    let warm = state.warm.lock().await.len() as u32;
    Ok(Json(ExecutorStats {
        backend: "remote-http".into(),
        node_id: state.cfg.bind.clone(),
        used,
        capacity: state.cfg.max_workspaces.max(1),
        warm,
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
    let leased_n = state.leased.lock().await.len() as u32;
    if leased_n >= state.cfg.max_workspaces.max(1) {
        return Err((StatusCode::TOO_MANY_REQUESTS, "pool_exhausted".into()));
    }
    let id = Uuid::new_v4().to_string();
    let dest = state
        .cfg
        .root
        .join(&body.tenant_id)
        .join(&body.session_id)
        .join(&id);
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    let warmed = state.warm.lock().await.pop();
    if let Some(src) = warmed {
        if tokio::fs::rename(&src, &dest).await.is_err() {
            tokio::fs::create_dir_all(&dest)
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            let _ = tokio::fs::remove_dir_all(src).await;
        }
    } else {
        tokio::fs::create_dir_all(&dest)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    state.leased.lock().await.insert(
        id.clone(),
        Slot {
            dir: dest,
            tenant_id: body.tenant_id,
            session_id: body.session_id,
            last_used: Instant::now(),
        },
    );
    let refill = state.clone();
    tokio::spawn(async move {
        refill_warm(&refill).await;
    });
    Ok(Json(WorkspaceHandle {
        id,
        backend: "remote-http".into(),
    }))
}

#[derive(Deserialize)]
struct HandleIn {
    handle: String,
}

async fn release(
    State(state): State<Arc<Inner>>,
    headers: HeaderMap,
    Json(body): Json<HandleIn>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !auth_ok(&state, &headers) {
        return Err(deny());
    }
    let slot = state.leased.lock().await.remove(&body.handle);
    if let Some(slot) = slot {
        wipe_dir(&slot.dir).await;
        let mut warm = state.warm.lock().await;
        if (warm.len() as u32) < state.cfg.warm_pool {
            warm.push(slot.dir);
        } else {
            let _ = tokio::fs::remove_dir_all(slot.dir).await;
        }
    }
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
    let slot = state.leased.lock().await.remove(&body.handle);
    if let Some(slot) = slot {
        let _ = tokio::fs::remove_dir_all(slot.dir).await;
    }
    Ok(Json(serde_json::json!({"ok": true})))
}

#[derive(Deserialize)]
struct ExecIn {
    handle: String,
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
    let dir = workspace(&state, &body.handle).await?;
    let timeout = body.timeout_secs.unwrap_or(30).clamp(1, 300);
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(&body.command)
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
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(timeout),
        child.wait_with_output(),
    )
    .await;
    match out {
        Ok(Ok(o)) => Ok(Json(ExecResult {
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
            exit_code: o.status.code().unwrap_or(1),
        })),
        Ok(Err(e)) => Ok(Json(ExecResult {
            stdout: String::new(),
            stderr: e.to_string(),
            exit_code: 1,
        })),
        Err(_) => Ok(Json(ExecResult {
            stdout: String::new(),
            stderr: "timed out".into(),
            exit_code: 124,
        })),
    }
}

async fn run_sandboxed(
    dir: PathBuf,
    name: &str,
    mut args: serde_json::Value,
) -> Result<ToolText, (StatusCode, String)> {
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
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(ToolText {
        content: out.content,
        is_error: out.is_error,
    })
}

#[derive(Deserialize)]
struct FsReadIn {
    handle: String,
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
    let dir = workspace(&state, &body.handle).await?;
    let mut args = serde_json::json!({"path": body.path});
    if let Some(o) = body.offset {
        args["offset"] = o.into();
    }
    if let Some(l) = body.limit {
        args["limit"] = l.into();
    }
    Ok(Json(run_sandboxed(dir, "read", args).await?))
}

#[derive(Deserialize)]
struct FsWriteIn {
    handle: String,
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
    let dir = workspace(&state, &body.handle).await?;
    Ok(Json(
        run_sandboxed(
            dir,
            "write",
            serde_json::json!({"path": body.path, "content": body.content}),
        )
        .await?,
    ))
}

#[derive(Deserialize)]
struct FsEditIn {
    handle: String,
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
    let dir = workspace(&state, &body.handle).await?;
    let mut args = body.arguments;
    if args.get("path").is_none() {
        args["path"] = serde_json::Value::String(body.path);
    }
    Ok(Json(run_sandboxed(dir, "edit", args).await?))
}

#[derive(Deserialize)]
struct GlobIn {
    handle: String,
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
    let dir = workspace(&state, &body.handle).await?;
    let mut args = serde_json::json!({"pattern": body.pattern});
    args["path"] = serde_json::Value::String(body.path.unwrap_or_else(|| dir.display().to_string()));
    Ok(Json(run_sandboxed(dir, "glob", args).await?))
}

#[derive(Deserialize)]
struct GrepIn {
    handle: String,
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
    let dir = workspace(&state, &body.handle).await?;
    let mut args = serde_json::json!({"pattern": body.pattern});
    args["path"] = serde_json::Value::String(body.path.unwrap_or_else(|| dir.display().to_string()));
    if let Some(inc) = body.include {
        args["include"] = inc.into();
    }
    if let Some(m) = body.max_results {
        args["max_results"] = m.into();
    }
    Ok(Json(run_sandboxed(dir, "grep", args).await?))
}

#[derive(Deserialize)]
struct BootstrapIn {
    handle: String,
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
    let dir = workspace(&state, &body.handle).await?;
    match body.kind {
        BootstrapKind::Empty => Ok(Json(ToolText::ok("empty workspace"))),
        BootstrapKind::Git { url } => {
            let st = tokio::process::Command::new("git")
                .args(["clone", "--depth", "1", &url, "."])
                .current_dir(&dir)
                .output()
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            if st.status.success() {
                Ok(Json(ToolText::ok(format!("cloned {url}"))))
            } else {
                Ok(Json(ToolText::err(String::from_utf8_lossy(&st.stderr))))
            }
        }
    }
}

#[derive(Deserialize)]
struct SnapshotIn {
    handle: String,
}

async fn snapshot(
    State(state): State<Arc<Inner>>,
    headers: HeaderMap,
    Json(body): Json<SnapshotIn>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !auth_ok(&state, &headers) {
        return Err(deny());
    }
    let dir = workspace(&state, &body.handle).await?;
    let tmp = state
        .cfg
        .root
        .join("_snap")
        .join(format!("{}.tgz", body.handle));
    if let Some(p) = tmp.parent() {
        tokio::fs::create_dir_all(p)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
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
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if !st.success() {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "tar snapshot failed".into(),
        ));
    }
    let bytes = tokio::fs::read(&tmp)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let _ = tokio::fs::remove_file(&tmp).await;
    Ok(Json(serde_json::json!({
        "bytes": bytes.len(),
        "archive_b64": store::b64_encode(&bytes),
    })))
}

#[derive(Deserialize)]
struct RestoreIn {
    handle: String,
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
    let dir = workspace(&state, &body.handle).await?;
    let bytes = store::b64_decode(&body.archive_b64)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    wipe_dir(&dir).await;
    let tmp = state
        .cfg
        .root
        .join("_snap")
        .join(format!("restore-{}.tgz", body.handle));
    if let Some(p) = tmp.parent() {
        tokio::fs::create_dir_all(p)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    tokio::fs::write(&tmp, &bytes)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let st = tokio::process::Command::new("tar")
        .args([
            "-C",
            &dir.display().to_string(),
            "-xzf",
            &tmp.display().to_string(),
        ])
        .status()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let _ = tokio::fs::remove_file(&tmp).await;
    if !st.success() {
        return Err((StatusCode::INTERNAL_SERVER_ERROR, "tar restore failed".into()));
    }
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
}
