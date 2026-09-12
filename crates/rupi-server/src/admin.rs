//! 管理面：独立 admin 凭证 + 运营 REST + 同进程静态控制台。
//! 不是聊天 IDE，也不复刻 TUI。

use crate::agui::{self, RunAgentInput};
use crate::auth;
use crate::db::{self, Tenant};
use crate::http::{self, session_json, teardown_session};
use crate::run::{self, Preflight};
use crate::App;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderName, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use std::convert::Infallible;
use uuid::Uuid;

const CSP: &str = "default-src 'self'; img-src 'self' data:; style-src 'self'; script-src 'self'; connect-src 'self'; frame-ancestors 'none'";

/// 云路径已经在用、或本机 settings 已有的键。不发明新配置体系。
pub const ALLOWED_SETTING_KEYS: &[&str] = &[
    "model",
    "thinking",
    "theme",
    "tools",
    "defaultTools",
    "excludeTools",
    "exclude_tools",
    "compaction",
    "steeringMode",
    "followUpMode",
    "defaultProjectTrust",
    "externalEditor",
    "enabledModels",
    "provider",
    "openai_api_key",
    "api_key",
    "anthropic_api_key",
    "gemini_api_key",
    "base_url",
    "mock_script",
];

pub const SECRET_SETTING_KEYS: &[&str] = &[
    "openai_api_key",
    "api_key",
    "anthropic_api_key",
    "gemini_api_key",
];

pub fn router() -> Router<App> {
    Router::new()
        .route("/admin", get(ui_index))
        .route("/admin/", get(ui_index))
        .route("/admin/assets/admin.css", get(ui_css))
        .route("/admin/assets/admin.js", get(ui_js))
        .route("/admin/api/me", get(me))
        .route("/admin/api/overview", get(overview))
        .route("/admin/api/tenants", get(list_tenants).post(create_tenant))
        .route(
            "/admin/api/tenants/{id}",
            get(get_tenant).patch(patch_tenant),
        )
        .route(
            "/admin/api/tenants/{id}/keys",
            get(list_keys).post(create_key),
        )
        .route("/admin/api/keys/{id}/revoke", post(revoke_key))
        .route(
            "/admin/api/tenants/{id}/quota",
            get(get_quota).patch(patch_quota),
        )
        .route(
            "/admin/api/tenants/{id}/settings",
            get(get_settings).patch(patch_settings),
        )
        .route("/admin/api/sessions", get(list_sessions))
        .route(
            "/admin/api/sessions/{id}",
            get(get_session).delete(delete_session),
        )
        .route(
            "/admin/api/sessions/{id}/debug-run",
            post(debug_run),
        )
        .route("/admin/api/executors", get(executors))
        .route("/admin/api/regions", get(regions))
        .route(
            "/admin/api/admin-keys",
            get(list_admin_keys).post(create_admin_key),
        )
        .route("/admin/api/admin-keys/{id}/revoke", post(revoke_admin_key))
}

async fn ui_index() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
            (HeaderName::from_static("content-security-policy"), CSP),
        ],
        include_str!("../static/admin.html"),
    )
}

async fn ui_css() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=120"),
        ],
        include_str!("../static/admin.css"),
    )
}

async fn ui_js() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "application/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=120"),
        ],
        include_str!("../static/admin.js"),
    )
}

fn unauth() -> Response {
    (StatusCode::UNAUTHORIZED, Json(json!({"error": "unauthorized"}))).into_response()
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response()
}

fn bad(msg: impl Into<String>) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({"error": msg.into()}))).into_response()
}

fn internal(e: impl ToString) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": e.to_string()})),
    )
        .into_response()
}

async fn require_admin(app: &App, headers: &HeaderMap) -> Result<(), Response> {
    let raw = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let Some(key) = auth::extract_bearer(raw).filter(|s| !s.is_empty()) else {
        return Err(unauth());
    };
    if let Some(expected) = app.admin_token.as_deref() {
        if auth::tokens_eq(key, expected) {
            return Ok(());
        }
    }
    match db::admin_by_key_hash(&app.pool, &auth::hash_key(key)).await {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err(unauth()),
        Err(e) => Err(internal(e)),
    }
}

fn tenant_json(t: &Tenant) -> Value {
    json!({
        "id": t.id,
        "name": t.name,
        "defaultModel": t.default_model,
        "defaultRegion": t.default_region,
        "quota": {
            "maxConcurrentRuns": t.max_concurrent_runs,
            "maxHandles": t.max_handles,
            "maxRunsPerDay": t.max_runs_per_day,
            "maxTokensPerDay": t.max_tokens_per_day,
            "maxQps": t.max_qps
        }
    })
}

fn key_json(k: &db::ApiKeyRow) -> Value {
    json!({
        "id": k.id,
        "tenantId": k.tenant_id,
        "prefix": k.key_prefix,
        "createdAt": k.created_at,
        "revokedAt": k.revoked_at,
        "revoked": k.revoked_at.is_some()
    })
}

fn admin_key_json(k: &db::AdminKeyRow) -> Value {
    json!({
        "id": k.id,
        "prefix": k.key_prefix,
        "createdAt": k.created_at,
        "revokedAt": k.revoked_at,
        "revoked": k.revoked_at.is_some()
    })
}

pub fn admin_session_status(row: &db::SessionRow) -> &'static str {
    if row.run_id.as_deref().is_some_and(|s| !s.is_empty()) {
        "running"
    } else if row.workspace_state.as_deref() == Some("snapshotted") {
        "snapshotted"
    } else if row.runtime_handle.is_some() {
        "ready"
    } else {
        "none"
    }
}

fn admin_session_json(row: &db::SessionRow) -> Value {
    let mut v = session_json(row, None);
    v["tenantId"] = json!(row.tenant_id);
    v["status"] = json!(admin_session_status(row));
    v["workspaceState"] = json!(row.workspace_state);
    v["instanceId"] = json!(row.instance_id);
    v["snapshotKey"] = json!(row.snapshot_key);
    v
}

pub fn mask_settings(mut settings: Value) -> Value {
    if let Some(obj) = settings.as_object_mut() {
        for k in SECRET_SETTING_KEYS {
            if obj.contains_key(*k) {
                obj.insert((*k).into(), json!("***"));
            }
        }
    }
    settings
}

pub fn merge_allowed_settings(existing: Value, patch: &Value) -> Result<Value, String> {
    let Some(src) = patch.as_object() else {
        return Err("settings patch must be a JSON object".into());
    };
    if src.is_empty() {
        return Err("empty patch".into());
    }
    let unknown: Vec<&String> = src
        .keys()
        .filter(|k| !ALLOWED_SETTING_KEYS.contains(&k.as_str()))
        .collect();
    if !unknown.is_empty() {
        return Err(format!(
            "unknown settings keys: {}; allowed: {}",
            unknown
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            ALLOWED_SETTING_KEYS.join(", ")
        ));
    }
    let mut merged = existing;
    if !merged.is_object() {
        merged = json!({});
    }
    if let Some(dst) = merged.as_object_mut() {
        for (k, v) in src {
            if SECRET_SETTING_KEYS.contains(&k.as_str()) && v.as_str() == Some("***") {
                continue;
            }
            dst.insert(k.clone(), v.clone());
        }
    }
    Ok(merged)
}

fn clamp_page(limit: Option<u32>, offset: Option<u32>) -> (i64, i64) {
    let limit = limit.unwrap_or(50).clamp(1, 200) as i64;
    let offset = offset.unwrap_or(0) as i64;
    (limit, offset.max(0))
}

async fn me(State(app): State<App>, headers: HeaderMap) -> Result<Json<Value>, Response> {
    require_admin(&app, &headers).await?;
    let via = if app.admin_token.is_some() {
        "token-or-key"
    } else {
        "admin-key"
    };
    Ok(Json(json!({
        "role": "admin",
        "instanceId": app.instance_id,
        "region": app.region,
        "auth": via
    })))
}

async fn overview(State(app): State<App>, headers: HeaderMap) -> Result<Json<Value>, Response> {
    require_admin(&app, &headers).await?;
    let counts = db::overview_counts(app.reader()).await.map_err(internal)?;
    let pg = app.pool.get().await.is_ok();
    let pool = app.executor.stats().await.ok();
    Ok(Json(json!({
        "instanceId": app.instance_id,
        "region": app.region,
        "postgres": pg,
        "redis": app.cache.available().await,
        "counts": {
            "tenants": counts.tenants,
            "sessions": counts.sessions,
            "running": counts.running,
            "snapshotted": counts.snapshotted,
            "allocated": counts.allocated,
            "keysActive": counts.keys_active
        },
        "pool": pool,
        "topology": app.executor.topology()
    })))
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ListQ {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    tenant_id: Option<String>,
    #[serde(default)]
    region: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    offset: Option<u32>,
}

async fn list_tenants(
    State(app): State<App>,
    headers: HeaderMap,
    Query(q): Query<ListQ>,
) -> Result<Json<Value>, Response> {
    require_admin(&app, &headers).await?;
    let query = q.q.as_deref().unwrap_or("").trim().to_string();
    let (limit, offset) = clamp_page(q.limit, q.offset);
    let total = db::count_tenants(app.reader(), &query)
        .await
        .map_err(internal)?;
    let rows = db::list_tenants(app.reader(), &query, limit, offset)
        .await
        .map_err(internal)?;
    Ok(Json(json!({
        "total": total,
        "tenants": rows.iter().map(|r| {
            let mut v = tenant_json(&r.tenant);
            v["createdAt"] = json!(r.created_at);
            v["sessionCount"] = json!(r.session_count);
            v["keyCount"] = json!(r.key_count);
            v
        }).collect::<Vec<_>>()
    })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateTenant {
    name: String,
    #[serde(default)]
    default_model: Option<String>,
    #[serde(default)]
    default_region: Option<String>,
    #[serde(default)]
    quota: Option<QuotaPatch>,
}

async fn create_tenant(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<CreateTenant>,
) -> Result<(StatusCode, Json<Value>), Response> {
    require_admin(&app, &headers).await?;
    let name = body.name.trim();
    if name.is_empty() {
        return Err(bad("name required"));
    }
    let raw = auth::generate_key();
    let t = db::create_tenant(&app.pool, name, &raw)
        .await
        .map_err(internal)?;
    if body.default_model.is_some() || body.default_region.is_some() {
        let _ = db::patch_tenant_meta(
            &app.pool,
            &t.id,
            None,
            body.default_model.as_deref(),
            body.default_region.as_deref(),
        )
        .await;
    }
    if let Some(q) = body.quota {
        apply_quota(&app, &t.id, &q).await?;
    }
    let t = db::load_tenant(&app.pool, &t.id)
        .await
        .map_err(internal)?
        .ok_or_else(not_found)?;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "tenant": tenant_json(&t),
            "key": raw,
            "warning": "明文 Key 只显示这一次"
        })),
    ))
}

async fn get_tenant(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, Response> {
    require_admin(&app, &headers).await?;
    let t = db::load_tenant(app.reader(), &id)
        .await
        .map_err(internal)?
        .ok_or_else(not_found)?;
    let keys = db::list_api_keys(app.reader(), &id)
        .await
        .map_err(internal)?;
    let (tokens, runs) = db::today_quota(&app.pool, &id).await.unwrap_or((0, 0));
    let history = db::quota_history(app.reader(), &id, 14)
        .await
        .unwrap_or_default();
    let sessions = db::count_sessions(app.reader(), &id).await.unwrap_or(0);
    let mut body = tenant_json(&t);
    body["settings"] = mask_settings(t.settings.clone());
    body["allowedKeys"] = json!(ALLOWED_SETTING_KEYS);
    body["keys"] = json!(keys.iter().map(key_json).collect::<Vec<_>>());
    body["sessionCount"] = json!(sessions);
    body["quota"]["runsToday"] = json!(runs);
    body["quota"]["tokensToday"] = json!(tokens);
    body["quota"]["ledger"] = json!(history
        .iter()
        .map(|(day, tok, r)| json!({"day": day, "tokens": tok, "runs": r}))
        .collect::<Vec<_>>());
    Ok(Json(body))
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct PatchTenant {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    default_model: Option<String>,
    #[serde(default)]
    default_region: Option<String>,
    #[serde(default)]
    quota: Option<QuotaPatch>,
}

async fn patch_tenant(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<PatchTenant>,
) -> Result<Json<Value>, Response> {
    require_admin(&app, &headers).await?;
    let t = db::load_tenant(&app.pool, &id)
        .await
        .map_err(internal)?
        .ok_or_else(not_found)?;
    let name = body.name.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let n = db::patch_tenant_meta(
        &app.pool,
        &t.id,
        name,
        body.default_model.as_deref(),
        body.default_region.as_deref(),
    )
    .await
    .map_err(internal)?;
    if n == 0 {
        return Err(not_found());
    }
    if let Some(q) = body.quota {
        apply_quota(&app, &t.id, &q).await?;
    }
    let t = db::load_tenant(&app.pool, &id)
        .await
        .map_err(internal)?
        .ok_or_else(not_found)?;
    Ok(Json(tenant_json(&t)))
}

async fn list_keys(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, Response> {
    require_admin(&app, &headers).await?;
    let _ = db::load_tenant(app.reader(), &id)
        .await
        .map_err(internal)?
        .ok_or_else(not_found)?;
    let keys = db::list_api_keys(app.reader(), &id)
        .await
        .map_err(internal)?;
    Ok(Json(json!({"keys": keys.iter().map(key_json).collect::<Vec<_>>()})))
}

async fn create_key(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<Value>), Response> {
    require_admin(&app, &headers).await?;
    let _ = db::load_tenant(&app.pool, &id)
        .await
        .map_err(internal)?
        .ok_or_else(not_found)?;
    let raw = auth::generate_key();
    let row = db::create_api_key(&app.pool, &id, &raw)
        .await
        .map_err(internal)?;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "key": raw,
            "record": key_json(&row),
            "warning": "明文 Key 只显示这一次"
        })),
    ))
}

async fn revoke_key(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, Response> {
    require_admin(&app, &headers).await?;
    let row = db::get_api_key(&app.pool, &id)
        .await
        .map_err(internal)?
        .ok_or_else(not_found)?;
    if row.revoked_at.is_some() {
        return Ok(Json(json!({"ok": true, "already": true})));
    }
    let ok = db::revoke_api_key(&app.pool, &id).await.map_err(internal)?;
    if !ok {
        return Err(not_found());
    }
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct QuotaPatch {
    #[serde(default)]
    max_concurrent_runs: Option<i32>,
    #[serde(default)]
    max_handles: Option<i32>,
    #[serde(default)]
    max_runs_per_day: Option<i32>,
    #[serde(default)]
    max_tokens_per_day: Option<i64>,
    #[serde(default)]
    max_qps: Option<i32>,
}

async fn apply_quota(app: &App, tenant_id: &str, q: &QuotaPatch) -> Result<(), Response> {
    for (label, v) in [
        ("maxConcurrentRuns", q.max_concurrent_runs.map(i64::from)),
        ("maxHandles", q.max_handles.map(i64::from)),
        ("maxRunsPerDay", q.max_runs_per_day.map(i64::from)),
        ("maxQps", q.max_qps.map(i64::from)),
    ] {
        if let Some(n) = v {
            if n < 0 {
                return Err(bad(format!("{label} must be >= 0")));
            }
        }
    }
    if let Some(n) = q.max_tokens_per_day {
        if n < 0 {
            return Err(bad("maxTokensPerDay must be >= 0"));
        }
    }
    let n = db::patch_tenant_quota(
        &app.pool,
        tenant_id,
        q.max_concurrent_runs,
        q.max_handles,
        q.max_runs_per_day,
        q.max_tokens_per_day,
        q.max_qps,
    )
    .await
    .map_err(internal)?;
    if n == 0 {
        return Err(not_found());
    }
    Ok(())
}

async fn get_quota(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, Response> {
    require_admin(&app, &headers).await?;
    let t = db::load_tenant(app.reader(), &id)
        .await
        .map_err(internal)?
        .ok_or_else(not_found)?;
    let (tokens, runs) = db::today_quota(&app.pool, &id).await.unwrap_or((0, 0));
    let history = db::quota_history(app.reader(), &id, 14)
        .await
        .unwrap_or_default();
    let mut q = tenant_json(&t)["quota"].clone();
    q["runsToday"] = json!(runs);
    q["tokensToday"] = json!(tokens);
    q["ledger"] = json!(history
        .iter()
        .map(|(day, tok, r)| json!({"day": day, "tokens": tok, "runs": r}))
        .collect::<Vec<_>>());
    Ok(Json(q))
}

async fn patch_quota(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<QuotaPatch>,
) -> Result<Json<Value>, Response> {
    require_admin(&app, &headers).await?;
    let _ = db::load_tenant(&app.pool, &id)
        .await
        .map_err(internal)?
        .ok_or_else(not_found)?;
    apply_quota(&app, &id, &body).await?;
    get_quota(State(app), headers, Path(id)).await
}

async fn get_settings(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, Response> {
    require_admin(&app, &headers).await?;
    let t = db::load_tenant(app.reader(), &id)
        .await
        .map_err(internal)?
        .ok_or_else(not_found)?;
    Ok(Json(json!({
        "settings": mask_settings(t.settings),
        "allowedKeys": ALLOWED_SETTING_KEYS,
        "secretKeys": SECRET_SETTING_KEYS
    })))
}

async fn patch_settings(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, Response> {
    require_admin(&app, &headers).await?;
    let t = db::load_tenant(&app.pool, &id)
        .await
        .map_err(internal)?
        .ok_or_else(not_found)?;
    let merged = merge_allowed_settings(t.settings, &body).map_err(bad)?;
    db::update_settings(&app.pool, &id, &merged)
        .await
        .map_err(internal)?;
    Ok(Json(json!({
        "ok": true,
        "settings": mask_settings(merged)
    })))
}

async fn list_sessions(
    State(app): State<App>,
    headers: HeaderMap,
    Query(q): Query<ListQ>,
) -> Result<Json<Value>, Response> {
    require_admin(&app, &headers).await?;
    let tenant = q.tenant_id.as_deref().unwrap_or("").trim().to_string();
    let region = q.region.as_deref().unwrap_or("").trim().to_string();
    let status = q.status.as_deref().unwrap_or("").trim().to_string();
    if !status.is_empty()
        && !["running", "snapshotted", "ready", "hot", "none"].contains(&status.as_str())
    {
        return Err(bad("status must be running|snapshotted|ready|hot|none"));
    }
    let (limit, offset) = clamp_page(q.limit, q.offset);
    let total = db::count_sessions_admin(app.reader(), &tenant, &region, &status)
        .await
        .map_err(internal)?;
    let rows = db::list_sessions_admin(app.reader(), &tenant, &region, &status, limit, offset)
        .await
        .map_err(internal)?;
    Ok(Json(json!({
        "total": total,
        "sessions": rows.iter().map(admin_session_json).collect::<Vec<_>>()
    })))
}

async fn get_session(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, Response> {
    require_admin(&app, &headers).await?;
    let row = db::get_session_any(app.reader(), &id)
        .await
        .map_err(internal)?
        .ok_or_else(not_found)?;
    let tree = db::load_tree(&app.pool, &row.tenant_id, &id).await.ok();
    let mut v = admin_session_json(&row);
    if let Some(tr) = &tree {
        v["tree"] = json!({"nodes": tr.nodes.len(), "tokens": tr.history_tokens()});
    }
    let run_id = Uuid::new_v4().to_string();
    v["openHint"] = json!({
        "protocol": "AG-UI HTTP+SSE",
        "method": "POST",
        "path": "/v1/agent",
        "accept": "text/event-stream",
        "auth": "Authorization: Bearer <tenant-api-key>",
        "bodyExample": {
            "threadId": id,
            "runId": run_id,
            "messages": [{"id": "m1", "role": "user", "content": "hello"}]
        },
        "cli": format!(
            "rupi cloud --url <control-plane> --api-key <tenant-key> prompt --session {id} 'hello'"
        )
    });
    Ok(Json(v))
}

async fn delete_session(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, Response> {
    require_admin(&app, &headers).await?;
    let row = db::get_session_any(&app.pool, &id)
        .await
        .map_err(internal)?
        .ok_or_else(not_found)?;
    teardown_session(&app, &row).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct DebugBody {
    prompt: String,
}

async fn debug_run(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<DebugBody>,
) -> Result<Response, Response> {
    require_admin(&app, &headers).await?;
    let prompt = body.prompt.trim();
    if prompt.is_empty() {
        return Err(bad("prompt required"));
    }
    let row = db::get_session_any(&app.pool, &id)
        .await
        .map_err(internal)?
        .ok_or_else(not_found)?;
    let t = db::load_tenant(&app.pool, &row.tenant_id)
        .await
        .map_err(internal)?
        .ok_or_else(not_found)?;
    let input = RunAgentInput {
        thread_id: id,
        run_id: Uuid::new_v4().to_string(),
        parent_run_id: None,
        state: None,
        messages: vec![agui::AguiMessage {
            id: Some(Uuid::new_v4().to_string()),
            role: "user".into(),
            content: Some(json!(prompt)),
            tool_calls: None,
        }],
        tools: vec![],
        context: vec![],
        forwarded_props: None,
        resume: vec![],
    };
    match run::start_run(app, t, input).await {
        Preflight::Status { code, body } => {
            let st = StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_REQUEST);
            Ok((st, Json(body)).into_response())
        }
        Preflight::Stream(rx, cancel) => {
            let inner = tokio_stream::wrappers::UnboundedReceiverStream::new(rx).map(|ev| {
                Ok::<_, Infallible>(Event::default().data(ev.to_sse_data()))
            });
            let stream = http::CancelOnDrop { inner, cancel };
            Ok(Sse::new(stream)
                .keep_alive(axum::response::sse::KeepAlive::default())
                .into_response())
        }
    }
}

async fn executors(State(app): State<App>, headers: HeaderMap) -> Result<Json<Value>, Response> {
    require_admin(&app, &headers).await?;
    let pool = app.executor.stats().await.ok();
    let nodes = app.executor.node_stats().await;
    let handles = db::handle_groups(app.reader()).await.map_err(internal)?;
    Ok(Json(json!({
        "controlPlane": {
            "instanceId": app.instance_id,
            "region": app.region
        },
        "pool": pool,
        "nodes": nodes,
        "topology": app.executor.topology(),
        "handles": handles.iter().map(|h| json!({
            "backend": h.backend,
            "region": h.region,
            "kind": h.kind,
            "allocated": h.allocated,
            "hot": h.hot,
            "snapshotted": h.snapshotted
        })).collect::<Vec<_>>()
    })))
}

async fn regions(State(app): State<App>, headers: HeaderMap) -> Result<Json<Value>, Response> {
    require_admin(&app, &headers).await?;
    let topo = app.executor.topology();
    let exec: Vec<String> = {
        let mut v: Vec<String> = topo.iter().map(|n| n.region.clone()).collect();
        v.sort();
        v.dedup();
        v
    };
    let sessions = db::list_session_regions(app.reader())
        .await
        .map_err(internal)?;
    let tenants = db::list_tenant_regions(app.reader())
        .await
        .map_err(internal)?;
    Ok(Json(json!({
        "controlPlane": app.region,
        "executorRegions": exec,
        "sessionRegions": sessions,
        "tenantDefaults": tenants,
        "topology": topo
    })))
}

async fn list_admin_keys(
    State(app): State<App>,
    headers: HeaderMap,
) -> Result<Json<Value>, Response> {
    require_admin(&app, &headers).await?;
    let keys = db::list_admin_keys(app.reader()).await.map_err(internal)?;
    Ok(Json(json!({
        "envTokenConfigured": app.admin_token.is_some(),
        "keys": keys.iter().map(admin_key_json).collect::<Vec<_>>()
    })))
}

async fn create_admin_key(
    State(app): State<App>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<Value>), Response> {
    require_admin(&app, &headers).await?;
    let raw = auth::generate_admin_key();
    let row = db::create_admin_key(&app.pool, &raw)
        .await
        .map_err(internal)?;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "key": raw,
            "record": admin_key_json(&row),
            "warning": "明文管理 Key 只显示这一次"
        })),
    ))
}

async fn revoke_admin_key(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, Response> {
    require_admin(&app, &headers).await?;
    let row = db::get_admin_key(&app.pool, &id)
        .await
        .map_err(internal)?
        .ok_or_else(not_found)?;
    if row.revoked_at.is_some() {
        return Ok(Json(json!({"ok": true, "already": true})));
    }
    let ok = db::revoke_admin_key(&app.pool, &id)
        .await
        .map_err(internal)?;
    if !ok {
        return Err(not_found());
    }
    Ok(Json(json!({"ok": true})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_whitelist_rejects_unknown() {
        let err = merge_allowed_settings(json!({}), &json!({"oauthClientId": "x"})).unwrap_err();
        assert!(err.contains("unknown"), "{err}");
    }

    #[test]
    fn settings_merge_keeps_secret_mask() {
        let merged = merge_allowed_settings(
            json!({"api_key": "real", "model": "a"}),
            &json!({"api_key": "***", "model": "b"}),
        )
        .unwrap();
        assert_eq!(merged["api_key"], "real");
        assert_eq!(merged["model"], "b");
    }
}
