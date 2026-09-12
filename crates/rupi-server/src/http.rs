use crate::agui::RunAgentInput;
use crate::auth;
use crate::db::{self, Tenant};
use crate::run::{self, Preflight};
use crate::App;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use rupi_memory::{export_tree_html, export_tree_jsonl, import_jsonl, remap_tree};
use rupi_runtime::{BootstrapKind, WorkspaceHandle};
use serde::Deserialize;
use serde_json::{json, Value};
use std::convert::Infallible;
use uuid::Uuid;

pub fn router(app: App) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
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
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(app)
}

async fn tenant_of(app: &App, headers: &HeaderMap) -> Result<Tenant, Response> {
    let raw = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let Some(key) = auth::extract_bearer(raw) else {
        return Err((StatusCode::UNAUTHORIZED, Json(json!({"error":"unauthorized"}))).into_response());
    };
    let hash = auth::hash_key(key);
    match db::tenant_by_key_hash(&app.pool, &hash).await {
        Ok(Some(t)) => Ok(t),
        Ok(None) => {
            Err((StatusCode::UNAUTHORIZED, Json(json!({"error":"unauthorized"}))).into_response())
        }
        Err(e) => {
            Err((StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response())
        }
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
        Err(e) => {
            Err((StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response())
        }
    }
}

async fn me(State(app): State<App>, headers: HeaderMap) -> Result<Json<Value>, Response> {
    let t = tenant_of(&app, &headers).await?;
    let (tokens, runs) = db::today_quota(&app.pool, &t.id).await.unwrap_or((0, 0));
    Ok(Json(json!({
        "tenantId": t.id,
        "name": t.name,
        "defaultModel": t.default_model,
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
        for k in ["openai_api_key", "api_key", "anthropic_api_key", "gemini_api_key"] {
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
    let mut merged = t.settings;
    if let (Some(dst), Some(src)) = (merged.as_object_mut(), body.as_object()) {
        for (k, v) in src {
            dst.insert(k.clone(), v.clone());
        }
    } else {
        merged = body;
    }
    db::update_settings(&app.pool, &t.id, &merged)
        .await
        .map_err(|e| {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response()
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
}

async fn create_session(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<CreateSession>,
) -> Result<(StatusCode, Json<Value>), Response> {
    let t = tenant_of(&app, &headers).await?;
    let n = db::count_sessions(&app.pool, &t.id).await.unwrap_or(0);
    if n >= t.max_handles as i64 {
        return Err((StatusCode::TOO_MANY_REQUESTS, Json(json!({"error":"handle quota"}))).into_response());
    }
    let id = Uuid::new_v4().to_string();
    let wh = app
        .executor
        .alloc(&t.id, &id)
        .await
        .map_err(|e| {
            (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": e.to_string()}))).into_response()
        })?;
    let kind = match body.bootstrap.as_deref() {
        Some("git") => BootstrapKind::Git {
            url: body.git_url.unwrap_or_default(),
        },
        _ => BootstrapKind::Empty,
    };
    let _ = app.executor.bootstrap(&wh, kind).await;
    let row = db::insert_session(
        &app.pool,
        &t.id,
        &id,
        body.name.as_deref(),
        body.model.as_deref(),
        Some(&wh.backend),
        Some(&wh.id),
        None,
    )
    .await
    .map_err(|e| {
        (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response()
    })?;
    Ok((StatusCode::CREATED, Json(session_json(&row, None))))
}

async fn list_sessions(State(app): State<App>, headers: HeaderMap) -> Result<Json<Value>, Response> {
    let t = tenant_of(&app, &headers).await?;
    let rows = db::list_sessions(&app.pool, &t.id).await.map_err(|e| {
        (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response()
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
    let tree = db::load_tree(&app.pool, &t.id, &id).await.ok();
    Ok(Json(session_json(
        &row,
        tree.as_ref().map(|tr| json!({"nodes": tr.nodes.len(), "tokens": tr.history_tokens()})),
    )))
}

async fn delete_session(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, Response> {
    let t = tenant_of(&app, &headers).await?;
    let row = session_guard(&app, &t, &id).await?;
    if let (Some(b), Some(h)) = (row.runtime_backend, row.runtime_handle) {
        let _ = app
            .executor
            .destroy(&WorkspaceHandle {
                id: h,
                backend: b,
            })
            .await;
    }
    let _ = db::delete_session(&app.pool, &t.id, &id).await;
    app.cache.invalidate_session(&t.id, &id).await;
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
        (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response()
    })?;
    if let Some(eid) = entry_id {
        if !tree.rewind_to(eid) && !tree.goto_node(eid) {
            return Err((StatusCode::BAD_REQUEST, Json(json!({"error":"unknown entryId"}))).into_response());
        }
    }
    let new_tree = remap_tree(&tree, path_only);
    let new_id = Uuid::new_v4().to_string();
    let wh = app.executor.alloc(&t.id, &new_id).await.map_err(|e| {
        (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": e.to_string()}))).into_response()
    })?;
    let _ = app.executor.bootstrap(&wh, BootstrapKind::Empty).await;
    let row = db::insert_session(
        &app.pool,
        &t.id,
        &new_id,
        src.name.as_deref(),
        src.model.as_deref(),
        Some(&wh.backend),
        Some(&wh.id),
        Some(id),
    )
    .await
    .map_err(|e| {
        (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response()
    })?;
    let mut new_tree = new_tree;
    new_tree.id = new_id.clone();
    db::persist_tree(&app.pool, &t.id, &new_id, &new_tree)
        .await
        .map_err(|e| {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response()
        })?;
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
        (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response()
    })?;
    if q.format.as_deref() == Some("html") {
        let html = export_tree_html(&tree, row.name.as_deref().unwrap_or("session"));
        return Ok(([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response());
    }
    let cwd = row
        .runtime_handle
        .as_deref()
        .unwrap_or("executor");
    let jsonl = export_tree_jsonl(&tree, cwd, row.name.as_deref(), row.parent_session.as_deref());
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
        (StatusCode::BAD_REQUEST, Json(json!({"error": e.to_string()}))).into_response()
    })?;
    let mut tree = remap_tree(&imported.tree, false);
    tree.id = id.clone();
    db::persist_tree(&app.pool, &t.id, &id, &tree)
        .await
        .map_err(|e| {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response()
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
        Preflight::Stream(rx, _join) => {
            let stream = tokio_stream::wrappers::UnboundedReceiverStream::new(rx).map(|ev| {
                Ok::<_, Infallible>(Event::default().data(ev.to_sse_data()))
            });
            Ok(Sse::new(stream)
                .keep_alive(axum::response::sse::KeepAlive::default())
                .into_response())
        }
    }
}

fn session_json(row: &db::SessionRow, extra: Option<Value>) -> Value {
    let mut v = json!({
        "id": row.id,
        "name": row.name,
        "model": row.model,
        "thinkingLevel": row.thinking_level,
        "autoCompaction": row.auto_compaction,
        "runtime": {
            "backend": row.runtime_backend,
            "handle": row.runtime_handle,
            "status": if row.runtime_handle.is_some() { "ready" } else { "none" }
        },
        "parentSession": row.parent_session,
        "createdAt": row.created_at,
        "updatedAt": row.updated_at
    });
    if let Some(e) = extra {
        v["tree"] = e;
    }
    v
}
