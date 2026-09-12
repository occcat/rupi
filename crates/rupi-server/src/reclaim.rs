//! 闲置工作区：snapshot → 对象存储，卷还给执行池。任意副本都可做。

use crate::db::{self, SessionRow};
use crate::App;
use rupi_runtime::{AllocRequest, BackendKind, WorkspaceHandle};
use std::time::Duration;

pub async fn loop_forever(app: App) {
    if !app.idle.enabled {
        return;
    }
    let mut tick = tokio::time::interval(app.idle.tick);
    loop {
        tick.tick().await;
        let _ = db::reap_stale_runs(&app.pool, 180).await;
        if let Ok(tenants) = db::list_tenant_ids(&app.pool).await {
            for id in tenants {
                let _ = crate::quota::reconcile(&app.cache, &app.pool, &id).await;
            }
        }
        let _ = reclaim_once(&app).await;
    }
}

pub async fn reclaim_once(app: &App) -> usize {
    let idle = app.idle.idle_after;
    let rows = db::list_reclaim_candidates(&app.pool, None, idle, 8)
        .await
        .unwrap_or_default();
    let mut n = 0;
    for row in rows {
        if snapshot_and_release(app, &row).await.is_ok() {
            n += 1;
        }
    }
    n
}

/// 池满时抢占：优先本租户、不要求闲置时长（无 run 即可）。
pub async fn preempt_one(app: &App, tenant_id: &str) -> bool {
    let rows = db::list_reclaim_candidates(&app.pool, Some(tenant_id), Duration::ZERO, 4)
        .await
        .unwrap_or_default();
    for row in rows {
        if snapshot_and_release(app, &row).await.is_ok() {
            return true;
        }
    }
    let others = db::list_reclaim_candidates(&app.pool, None, Duration::ZERO, 4)
        .await
        .unwrap_or_default();
    for row in others {
        if snapshot_and_release(app, &row).await.is_ok() {
            return true;
        }
    }
    false
}

pub async fn snapshot_and_release(app: &App, sess: &SessionRow) -> anyhow::Result<()> {
    let (Some(backend), Some(hid)) = (&sess.runtime_backend, &sess.runtime_handle) else {
        anyhow::bail!("no handle");
    };
    if sess.run_id.as_deref().is_some_and(|s| !s.is_empty()) {
        anyhow::bail!("session has active run");
    }
    let handle = WorkspaceHandle {
        id: hid.clone(),
        backend: backend.clone(),
        region: sess.region.clone(),
        kind: sess.runtime_kind.clone(),
    };
    let blob = app.executor.snapshot(&handle).await?;
    let key = format!("ws/{}/{}/{}.tgz", sess.tenant_id, sess.id, handle.id);
    app.object_store.put(&key, &blob).await?;
    let _ = db::insert_snapshot(
        &app.pool,
        &sess.tenant_id,
        &sess.id,
        &key,
        blob.len() as i64,
    )
    .await;
    app.executor.release(&handle).await?;
    db::mark_snapshotted(&app.pool, &sess.tenant_id, &sess.id, &key).await?;
    app.cache
        .invalidate_session(&sess.tenant_id, &sess.id)
        .await;
    Ok(())
}

pub async fn ensure_hot(
    app: &App,
    tenant_id: &str,
    sess: &SessionRow,
) -> anyhow::Result<WorkspaceHandle> {
    let snapshotted = sess.workspace_state.as_deref() == Some("snapshotted")
        || (sess.runtime_handle.is_none() && sess.snapshot_key.is_some());
    if !snapshotted {
        if let (Some(b), Some(h)) = (&sess.runtime_backend, &sess.runtime_handle) {
            return Ok(WorkspaceHandle {
                id: h.clone(),
                backend: b.clone(),
                region: sess.region.clone(),
                kind: sess.runtime_kind.clone(),
            });
        }
    }
    let mut req = AllocRequest::new(tenant_id, &sess.id);
    if let Some(r) = sess.region.as_deref() {
        req = req.with_region(r);
    }
    if let Some(k) = sess.runtime_kind.as_deref().and_then(BackendKind::parse) {
        req = req.with_kind(k);
    }
    let mut last = None;
    for _ in 0..2 {
        match app.executor.alloc_pref(&req).await {
            Ok(wh) => {
                if let Some(key) = sess.snapshot_key.as_deref() {
                    match app.object_store.get(key).await {
                        Ok(blob) => {
                            app.executor.restore(&wh, &blob).await?;
                        }
                        Err(e) => {
                            tracing::warn!("snapshot missing ({key}): {e:#}; empty workspace");
                        }
                    }
                }
                db::mark_hot(
                    &app.pool,
                    tenant_id,
                    &sess.id,
                    &wh.backend,
                    &wh.id,
                    sess.snapshot_key.as_deref(),
                    wh.kind.as_deref(),
                    wh.region.as_deref(),
                )
                .await?;
                return Ok(wh);
            }
            Err(e) if rupi_runtime::is_pool_exhausted(&e) => {
                last = Some(e);
                if !preempt_one(app, tenant_id).await {
                    break;
                }
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| rupi_runtime::PoolExhausted.into()))
}
