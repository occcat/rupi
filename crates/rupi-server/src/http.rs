use crate::agui::RunAgentInput;
use crate::auth;
use crate::db::{self, Tenant};
use crate::reclaim;
use crate::run::{self, Preflight};
use crate::App;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::{Stream, StreamExt};
use rupi_core::CancelFlag;
use rupi_memory::{export_tree_html, export_tree_jsonl, import_jsonl, remap_tree};
use rupi_runtime::{AllocRequest, BackendKind, BootstrapKind, WorkspaceHandle};
use serde::Deserialize;
use serde_json::{json, Value};
use std::convert::Infallible;
use std::pin::Pin;
use std::task::{Context, Poll};
use uuid::Uuid;

pub fn router(app: App) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/ready", get(ready))
        .route("/metrics", get(metrics))
        .route("/v1/me", get(me))
        .route("/v1/settings", get(get_settings).patch(patch_settings))
        .route("/v1/models", get(models))
        .route("/v1/sessions", get(list_sessions).post(create_session))
        .route("/v1/sessions/{id}", get(get_session).delete(delete_session))
        .route("/v1/sessions/{id}/fork", post(fork_session))
        .route("/v1/sessions/{id}/clone", post(clone_session))
        .route("/v1/sessions/{id}/export", get(export_session))
        .route("/v1/sessions/{id}/import", post(import_session))
        .route("/v1/agent", post(agent_run))
        .merge(crate::admin::router())
        .layer(rupi_runtime::access_trace_layer())
        .with_state(app)
}

async fn ready(State(app): State<App>) -> impl IntoResponse {
    let pg = app.pool.get().await.is_ok();
    let nodes = app.executor.node_stats().await;
    // 配置了节点但 stats 失败时 pool 仍回 capacity=0 占位；空列表 / 全 0 都算执行面未就绪。
    let exec_ok = nodes.iter().any(|n| n.capacity > 0);
    let ok = pg && exec_ok;
    let body = json!({
        "instanceId": app.instance_id,
        "region": app.region,
        "postgres": pg,
        "redis": app.cache.available().await,
        "executors": app.executor.topology(),
        "nodes": nodes.iter().map(|n| json!({
            "id": n.node_id,
            "backend": n.backend,
            "used": n.used,
            "capacity": n.capacity,
            "warm": n.warm,
            "region": n.region,
            "kind": n.kind
        })).collect::<Vec<_>>(),
        "ok": ok
    });
    let status = if ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(body))
}

async fn metrics(State(app): State<App>) -> impl IntoResponse {
    let nodes = app.executor.node_stats().await;
    let runs = app.metrics.runs.load(std::sync::atomic::Ordering::Relaxed);
    let r429 = app
        .metrics
        .reject_429
        .load(std::sync::atomic::Ordering::Relaxed);
    let used: u64 = nodes.iter().map(|n| n.used as u64).sum();
    let cap: u64 = nodes.iter().map(|n| n.capacity as u64).sum();
    let body = format!(
        "# HELP rupi_runs_total admitted AG-UI runs\n# TYPE rupi_runs_total counter\nrupi_runs_total {runs}\n# HELP rupi_rejects_429_total quota/pool 429s\n# TYPE rupi_rejects_429_total counter\nrupi_rejects_429_total {r429}\n# HELP rupi_executor_used leased workspaces\n# TYPE rupi_executor_used gauge\nrupi_executor_used {used}\n# HELP rupi_executor_capacity pool capacity\n# TYPE rupi_executor_capacity gauge\nrupi_executor_capacity {cap}\n"
    );
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], body)
}

pub(crate) struct CancelOnDrop<S> {
    pub(crate) inner: S,
    pub(crate) cancel: CancelFlag,
}

impl<S: Stream + Unpin> Stream for CancelOnDrop<S> {
    type Item = S::Item;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

impl<S> Drop for CancelOnDrop<S> {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

async fn tenant_of(app: &App, headers: &HeaderMap) -> Result<Tenant, Response> {
    let raw = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let Some(key) = auth::extract_bearer(raw) else {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"unauthorized"})),
        )
            .into_response());
    };
    let hash = auth::hash_key(key);
    match db::tenant_by_key_hash(&app.pool, &hash).await {
        Ok(Some(t)) => Ok(t),
        Ok(None) => Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"unauthorized"})),
        )
            .into_response()),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response()),
    }
}

async fn session_guard(app: &App, tenant: &Tenant, id: &str) -> Result<db::SessionRow, Response> {
    match db::get_session(&app.pool, &tenant.id, id).await {
        Ok(Some(s)) => Ok(s),
        Ok(None) => match db::session_owner(&app.pool, id).await {
            Ok(Some(other)) if other != tenant.id => {
                Err((StatusCode::FORBIDDEN, Json(json!({"error":"forbidden"}))).into_response())
            }
            _ => Err((StatusCode::NOT_FOUND, Json(json!({"error":"not found"}))).into_response()),
        },
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response()),
    }
}

async fn me(State(app): State<App>, headers: HeaderMap) -> Result<Json<Value>, Response> {
    let t = tenant_of(&app, &headers).await?;
    let (tokens, runs) = db::today_quota(&app.pool, &t.id).await.unwrap_or((0, 0));
    Ok(Json(json!({
        "tenantId": t.id,
        "name": t.name,
        "defaultModel": t.default_model,
        "defaultRegion": t.default_region,
        "region": app.region,
        "quota": {
            "maxConcurrentRuns": t.max_concurrent_runs,
            "maxHandles": t.max_handles,
            "runsToday": runs,
            "tokensToday": tokens
        }
    })))
}

async fn get_settings(State(app): State<App>, headers: HeaderMap) -> Result<Json<Value>, Response> {
    let t = tenant_of(&app, &headers).await?;
    let mut s = t.settings;
    if let Some(obj) = s.as_object_mut() {
        for k in [
            "openai_api_key",
            "api_key",
            "anthropic_api_key",
            "gemini_api_key",
        ] {
            if obj.contains_key(k) {
                obj.insert(k.into(), json!("***"));
            }
        }
    }
    Ok(Json(s))
}

async fn patch_settings(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, Response> {
    let t = tenant_of(&app, &headers).await?;
    let allow_mock = std::env::var("RUPI_CLOUD_ALLOW_MOCK").ok().as_deref() == Some("1");
    if !allow_mock {
        if body.get("mock_script").is_some() {
            return Err((
                StatusCode::FORBIDDEN,
                Json(json!({"error":"mock_script is admin/test only"})),
            )
                .into_response());
        }
    }
    let merged = crate::admin::merge_allowed_settings(t.settings, &body)
        .map_err(|e| (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response())?;
    db::update_settings(&app.pool, &t.id, &merged)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response()
        })?;
    Ok(Json(json!({"ok": true})))
}

async fn models(State(app): State<App>, headers: HeaderMap) -> Result<Json<Value>, Response> {
    let _ = tenant_of(&app, &headers).await?;
    let list: Vec<Value> = rupi_llm::load_models()
        .into_iter()
        .map(|m| {
            json!({
                "id": m.id,
                "provider": m.provider,
                "name": m.name,
                "contextWindow": m.context
            })
        })
        .collect();
    Ok(Json(json!({"models": list})))
}

#[derive(Deserialize)]
struct CreateSession {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    bootstrap: Option<String>,
    #[serde(default)]
    git_url: Option<String>,
    #[serde(default)]
    git_token: Option<String>,
    #[serde(default)]
    git_hosts: Option<Vec<String>>,
    #[serde(default)]
    region: Option<String>,
    #[serde(default)]
    backend: Option<String>,
}

async fn create_session(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<CreateSession>,
) -> Result<(StatusCode, Json<Value>), Response> {
    let t = tenant_of(&app, &headers).await?;
    let id = Uuid::new_v4().to_string();
    let region = resolve_region(&app, &t, body.region.as_deref());
    let backend_kind = body.backend.as_deref().and_then(BackendKind::parse);
    let reserved = db::insert_session_if_under_cap(
        &app.pool,
        &t.id,
        &id,
        body.name.as_deref(),
        body.model.as_deref(),
        None,
        None,
        None,
        Some(&region),
        backend_kind.map(|k| k.as_str()),
    )
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response()
    })?;
    if reserved.is_none() {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error":"handle quota"})),
        )
            .into_response());
    }
    let wh = match alloc_workspace(&app, &t.id, &id, Some(&region), backend_kind).await {
        Ok(h) => h,
        Err(e) => {
            let _ = db::delete_session(&app.pool, &t.id, &id).await;
            if rupi_runtime::is_pool_exhausted(&e) {
                return Err((
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(json!({"error":"execution pool exhausted"})),
                )
                    .into_response());
            }
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": e.to_string()})),
            )
                .into_response());
        }
    };
    let _ = db::set_runtime(&app.pool, &t.id, &id, &wh.backend, &wh.id).await;
    let _ = db::mark_hot(
        &app.pool,
        &t.id,
        &id,
        &wh.backend,
        &wh.id,
        None,
        wh.kind.as_deref(),
        Some(&region),
    )
    .await;
    let boot = match body.bootstrap.as_deref() {
        Some("git") => BootstrapKind::Git {
            url: body.git_url.unwrap_or_default(),
            token: body.git_token.filter(|s| !s.is_empty()).or_else(|| {
                t.settings
                    .get("git_token")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
            }),
            hosts: body.git_hosts.unwrap_or_else(|| {
                t.settings
                    .get("git_hosts")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|x| x.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default()
            }),
        },
        _ => BootstrapKind::Empty,
    };
    let _ = app.executor.bootstrap(&wh, boot).await;
    let row = db::get_session(&app.pool, &t.id, &id)
        .await
        .ok()
        .flatten()
        .ok_or_else(|| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error":"session vanished"})),
            )
                .into_response()
        })?;
    cache_session_meta(&app, &row).await;
    Ok((StatusCode::CREATED, Json(session_json(&row, None))))
}

async fn list_sessions(
    State(app): State<App>,
    headers: HeaderMap,
) -> Result<Json<Value>, Response> {
    let t = tenant_of(&app, &headers).await?;
    let rows = db::list_sessions(app.reader(), &t.id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response()
    })?;
    Ok(Json(json!({
        "sessions": rows.iter().map(|r| session_json(r, None)).collect::<Vec<_>>()
    })))
}

async fn get_session(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, Response> {
    let t = tenant_of(&app, &headers).await?;
    let row = session_guard(&app, &t, &id).await?;
    cache_session_meta(&app, &row).await;
    let tree = db::load_tree(&app.pool, &t.id, &id).await.ok();
    Ok(Json(session_json(
        &row,
        tree.as_ref()
            .map(|tr| json!({"nodes": tr.nodes.len(), "tokens": tr.history_tokens()})),
    )))
}

pub(crate) async fn teardown_session(app: &App, row: &db::SessionRow) {
    if let (Some(b), Some(h)) = (row.runtime_backend.clone(), row.runtime_handle.clone()) {
        let _ = app
            .executor
            .destroy(&WorkspaceHandle {
                id: h,
                backend: b,
                tenant_id: Some(row.tenant_id.clone()),
                region: row.region.clone(),
                kind: row.runtime_kind.clone(),
            })
            .await;
    }
    if let Some(key) = row.snapshot_key.as_deref() {
        let _ = app.object_store.delete(key).await;
    }
    let _ = db::delete_session(&app.pool, &row.tenant_id, &row.id).await;
    app.cache.invalidate_session(&row.tenant_id, &row.id).await;
}

async fn delete_session(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, Response> {
    let t = tenant_of(&app, &headers).await?;
    let row = session_guard(&app, &t, &id).await?;
    teardown_session(&app, &row).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, Default)]
struct ForkBody {
    #[serde(default)]
    entry_id: Option<String>,
}

async fn fork_session(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<ForkBody>,
) -> Result<Json<Value>, Response> {
    duplicate(&app, &headers, &id, true, body.entry_id.as_deref()).await
}

async fn clone_session(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, Response> {
    duplicate(&app, &headers, &id, false, None).await
}

async fn duplicate(
    app: &App,
    headers: &HeaderMap,
    id: &str,
    path_only: bool,
    entry_id: Option<&str>,
) -> Result<Json<Value>, Response> {
    let t = tenant_of(app, headers).await?;
    let src = session_guard(app, &t, id).await?;
    let mut tree = db::load_tree(&app.pool, &t.id, id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response()
    })?;
    if let Some(eid) = entry_id {
        if !tree.rewind_to(eid) && !tree.goto_node(eid) {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({"error":"unknown entryId"})),
            )
                .into_response());
        }
    }
    let new_tree = remap_tree(&tree, path_only);
    let new_id = Uuid::new_v4().to_string();
    let region = src
        .region
        .clone()
        .unwrap_or_else(|| resolve_region(app, &t, None));
    let kind = src.runtime_kind.as_deref().and_then(BackendKind::parse);
    let reserved = db::insert_session_if_under_cap(
        &app.pool,
        &t.id,
        &new_id,
        src.name.as_deref(),
        src.model.as_deref(),
        None,
        None,
        Some(id),
        Some(&region),
        kind.map(|k| k.as_str()),
    )
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response()
    })?;
    if reserved.is_none() {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error":"handle quota"})),
        )
            .into_response());
    }
    let wh = match alloc_workspace(app, &t.id, &new_id, Some(&region), kind).await {
        Ok(h) => h,
        Err(e) => {
            let _ = db::delete_session(&app.pool, &t.id, &new_id).await;
            if rupi_runtime::is_pool_exhausted(&e) {
                return Err((
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(json!({"error":"execution pool exhausted"})),
                )
                    .into_response());
            }
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": e.to_string()})),
            )
                .into_response());
        }
    };
    let _ = db::mark_hot(
        &app.pool,
        &t.id,
        &new_id,
        &wh.backend,
        &wh.id,
        None,
        wh.kind.as_deref(),
        Some(&region),
    )
    .await;
    let _ = app.executor.bootstrap(&wh, BootstrapKind::Empty).await;
    let mut new_tree = new_tree;
    new_tree.id = new_id.clone();
    db::persist_tree(&app.pool, &t.id, &new_id, &new_tree)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response()
        })?;
    let row = db::get_session(&app.pool, &t.id, &new_id)
        .await
        .ok()
        .flatten()
        .ok_or_else(|| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error":"session vanished"})),
            )
                .into_response()
        })?;
    cache_session_meta(app, &row).await;
    Ok(Json(session_json(&row, None)))
}

#[derive(Deserialize)]
struct ExportQ {
    #[serde(default)]
    format: Option<String>,
}

async fn export_session(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<ExportQ>,
) -> Result<Response, Response> {
    let t = tenant_of(&app, &headers).await?;
    let row = session_guard(&app, &t, &id).await?;
    let tree = db::load_tree(&app.pool, &t.id, &id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response()
    })?;
    if q.format.as_deref() == Some("html") {
        let html = export_tree_html(&tree, row.name.as_deref().unwrap_or("session"));
        return Ok(([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response());
    }
    let cwd = row.runtime_handle.as_deref().unwrap_or("executor");
    let jsonl = export_tree_jsonl(
        &tree,
        cwd,
        row.name.as_deref(),
        row.parent_session.as_deref(),
    );
    Ok(([(header::CONTENT_TYPE, "application/x-ndjson")], jsonl).into_response())
}

async fn import_session(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: String,
) -> Result<Json<Value>, Response> {
    let t = tenant_of(&app, &headers).await?;
    let _ = session_guard(&app, &t, &id).await?;
    let imported = import_jsonl(&body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response()
    })?;
    let mut tree = remap_tree(&imported.tree, false);
    tree.id = id.clone();
    db::persist_tree(&app.pool, &t.id, &id, &tree)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response()
        })?;
    app.cache.invalidate_session(&t.id, &id).await;
    Ok(Json(json!({"ok": true, "sessionId": id})))
}

async fn agent_run(
    State(app): State<App>,
    headers: HeaderMap,
    Json(input): Json<RunAgentInput>,
) -> Result<Response, Response> {
    let t = tenant_of(&app, &headers).await?;
    match run::start_run(app, t, input).await {
        Preflight::Status { code, body } => {
            let st = StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_REQUEST);
            Ok((st, Json(body)).into_response())
        }
        Preflight::Stream(rx, cancel) => {
            let inner = tokio_stream::wrappers::ReceiverStream::new(rx)
                .map(|ev| Ok::<_, Infallible>(Event::default().data(ev.to_sse_data())));
            let stream = CancelOnDrop { inner, cancel };
            Ok(Sse::new(stream)
                .keep_alive(axum::response::sse::KeepAlive::default())
                .into_response())
        }
    }
}

fn resolve_region(app: &App, tenant: &Tenant, requested: Option<&str>) -> String {
    requested
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| {
            tenant
                .default_region
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| app.region.clone())
}

async fn alloc_workspace(
    app: &App,
    tenant_id: &str,
    session_id: &str,
    region: Option<&str>,
    kind: Option<BackendKind>,
) -> anyhow::Result<WorkspaceHandle> {
    let mut req = AllocRequest::new(tenant_id, session_id);
    if let Some(r) = region.filter(|s| !s.is_empty()) {
        req = req.with_region(r);
    }
    if let Some(k) = kind {
        req = req.with_kind(k);
    }
    match app.executor.alloc_pref(&req).await {
        Ok(h) => {
            app.cache.clear_missing_session(tenant_id, session_id).await;
            Ok(h)
        }
        Err(e) if rupi_runtime::is_pool_exhausted(&e) => {
            if reclaim::preempt_one(app, tenant_id).await {
                let h = app.executor.alloc_pref(&req).await?;
                app.cache.clear_missing_session(tenant_id, session_id).await;
                Ok(h)
            } else {
                Err(e)
            }
        }
        Err(e) => Err(e),
    }
}

async fn cache_session_meta(app: &App, row: &db::SessionRow) {
    let _ = app
        .cache
        .put_session_meta(
            &row.tenant_id,
            &row.id,
            &session_json(row, None).to_string(),
            crate::cache::SESS_META_TTL_SECS,
        )
        .await;
}

pub(crate) fn session_json(row: &db::SessionRow, extra: Option<Value>) -> Value {
    let status = if row.workspace_state.as_deref() == Some("snapshotted") {
        "snapshotted"
    } else if row.runtime_handle.is_some() {
        "ready"
    } else {
        "none"
    };
    let mut v = json!({
        "id": row.id,
        "name": row.name,
        "model": row.model,
        "thinkingLevel": row.thinking_level,
        "autoCompaction": row.auto_compaction,
        "region": row.region.clone(),
        "runtime": {
            "backend": row.runtime_backend,
            "kind": row.runtime_kind,
            "handle": row.runtime_handle,
            "status": status,
            "region": row.region.clone()
        },
        "runId": row.run_id,
        "parentSession": row.parent_session,
        "createdAt": row.created_at,
        "updatedAt": row.updated_at
    });
    if let Some(e) = extra {
        v["tree"] = e;
    }
    v
}
