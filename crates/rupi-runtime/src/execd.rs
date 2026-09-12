//! `rupi-execd`：平台登记的远程执行进程。工作区与 `sh -c` 只出现在这里。

use crate::{
    BootstrapKind, ExecResult, ToolText, WorkspaceHandle,
};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use uuid::Uuid;

#[derive(Clone)]
pub struct ExecdConfig {
    pub bind: String,
    pub root: PathBuf,
    pub token: String,
}

struct Inner {
    cfg: ExecdConfig,
    dirs: Mutex<HashMap<String, PathBuf>>,
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
        dirs: Mutex::new(HashMap::new()),
    });
    Router::new()
        .route("/health", get(|| async { "ok" }))
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

async fn workspace(state: &Inner, handle: &str) -> Result<PathBuf, (StatusCode, String)> {
    let g = state.dirs.lock().await;
    g.get(handle)
        .cloned()
        .ok_or((StatusCode::NOT_FOUND, "unknown handle".into()))
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
    let id = Uuid::new_v4().to_string();
    let dir = state
        .cfg
        .root
        .join(&body.tenant_id)
        .join(&body.session_id)
        .join(&id);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    state.dirs.lock().await.insert(id.clone(), dir);
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
    let _ = body;
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
    let dir = {
        let mut g = state.dirs.lock().await;
        g.remove(&body.handle)
    };
    if let Some(dir) = dir {
        let _ = tokio::fs::remove_dir_all(dir).await;
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
