use crate::cache::Cache;
use crate::db::{self, PgPool, Tenant};
use serde_json::json;

#[derive(Debug)]
pub enum Admit {
    Ok,
    TooMany,
}

pub const LEASE_TTL_SECS: u64 = 180;
pub const HEARTBEAT_SECS: u64 = 15;

#[derive(Debug, Clone)]
pub struct LeaseOwner {
    pub run_id: String,
    pub instance_id: String,
}

fn lease_payload(run_id: &str, instance_id: &str) -> String {
    json!({"runId": run_id, "instanceId": instance_id}).to_string()
}

fn parse_lease(raw: &str) -> Option<LeaseOwner> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    Some(LeaseOwner {
        run_id: v.get("runId")?.as_str()?.to_string(),
        instance_id: v.get("instanceId")?.as_str()?.to_string(),
    })
}

/// 入院：日账本（Postgres）+ 并发。Redis 挂了则收紧到最多 1 个并发。
pub async fn admit_run(cache: &Cache, pool: &PgPool, tenant: &Tenant) -> Admit {
    let (tokens, runs) = db::today_quota(pool, &tenant.id).await.unwrap_or((0, 0));
    if tenant.max_runs_per_day > 0 && runs >= tenant.max_runs_per_day as i64 {
        return Admit::TooMany;
    }
    if tenant.max_tokens_per_day > 0 && tokens >= tenant.max_tokens_per_day {
        return Admit::TooMany;
    }

    let max = tenant.max_concurrent_runs.max(0) as i64;
    if max == 0 {
        return Admit::TooMany;
    }
    let pg_active = db::count_active_runs(pool, &tenant.id).await.unwrap_or(max);

    if cache.available().await {
        let n = cache
            .incr_ex(&Cache::rl_conc_key(&tenant.id), 600)
            .await
            .unwrap_or(max + 1);
        if n < pg_active {
            let _ = cache
                .set_ex(
                    &Cache::rl_conc_key(&tenant.id),
                    &pg_active.to_string(),
                    600,
                )
                .await;
            if pg_active >= max {
                cache.decr(&Cache::rl_conc_key(&tenant.id)).await;
                return Admit::TooMany;
            }
        }
        if n > max || pg_active >= max {
            cache.decr(&Cache::rl_conc_key(&tenant.id)).await;
            return Admit::TooMany;
        }
        let qps = cache
            .incr_ex(&Cache::rl_qps_key(&tenant.id), 1)
            .await
            .unwrap_or(0);
        if qps > 8 {
            cache.decr(&Cache::rl_conc_key(&tenant.id)).await;
            return Admit::TooMany;
        }
        let _ = db::bump_quota_run(pool, &tenant.id).await;
        Admit::Ok
    } else {
        // 无 Redis：收紧，只允许 1 个并发（再靠 sessions.run_id 乐观锁）。
        let cap = 1i64.min(max);
        if pg_active >= cap {
            return Admit::TooMany;
        }
        let _ = db::bump_quota_run(pool, &tenant.id).await;
        Admit::Ok
    }
}

pub async fn release_run(cache: &Cache, tenant_id: &str) {
    cache.decr(&Cache::rl_conc_key(tenant_id)).await;
}

/// Redis NX + Postgres 乐观锁。任一步失败都回滚，避免双持。
pub async fn acquire_lease(
    cache: &Cache,
    pool: &PgPool,
    tenant_id: &str,
    thread: &str,
    run_id: &str,
    instance_id: &str,
) -> bool {
    acquire_lease_with_ttl(
        cache,
        pool,
        tenant_id,
        thread,
        run_id,
        instance_id,
        LEASE_TTL_SECS,
    )
    .await
}

pub async fn acquire_lease_with_ttl(
    cache: &Cache,
    pool: &PgPool,
    tenant_id: &str,
    thread: &str,
    run_id: &str,
    instance_id: &str,
    ttl: u64,
) -> bool {
    let _ = db::reap_stale_runs(pool, 180).await;
    let key = Cache::lease_key(tenant_id, thread);
    let payload = lease_payload(run_id, instance_id);
    if cache.available().await && !cache.set_nx_ex(&key, &payload, ttl).await {
        return false;
    }
    match db::set_run_id(pool, tenant_id, thread, Some(run_id), Some(instance_id)).await {
        Ok(true) => true,
        _ => {
            cache.del(&key).await;
            false
        }
    }
}

/// 只清自己持有的 run，避免晚到的 finish 抹掉另一副本的新租约。
pub async fn release_lease(
    cache: &Cache,
    pool: &PgPool,
    tenant_id: &str,
    thread: &str,
    run_id: &str,
    instance_id: &str,
) {
    let key = Cache::lease_key(tenant_id, thread);
    if let Some(raw) = cache.get(&key).await {
        if let Some(owner) = parse_lease(&raw) {
            if owner.run_id == run_id && owner.instance_id == instance_id {
                cache.del(&key).await;
            }
        } else if raw == run_id {
            cache.del(&key).await;
        }
    }
    let _ = db::clear_run_id(pool, tenant_id, thread, run_id).await;
}

pub async fn heartbeat(
    cache: &Cache,
    pool: &PgPool,
    tenant_id: &str,
    thread: &str,
    run_id: &str,
    instance_id: &str,
) -> bool {
    let key = Cache::lease_key(tenant_id, thread);
    if cache.available().await {
        match cache.get(&key).await {
            Some(raw) => {
                let ok = parse_lease(&raw)
                    .map(|o| o.run_id == run_id && o.instance_id == instance_id)
                    .unwrap_or(false);
                if !ok {
                    return false;
                }
                if !cache.expire(&key, LEASE_TTL_SECS).await {
                    return false;
                }
            }
            None => {
                // Redis 丢了 key：尝试重新占上，失败则认输。
                if !cache
                    .set_nx_ex(&key, &lease_payload(run_id, instance_id), LEASE_TTL_SECS)
                    .await
                {
                    return false;
                }
            }
        }
    }
    db::touch_run(pool, tenant_id, thread, run_id)
        .await
        .unwrap_or(false)
}

/// 把 Redis 并发计数扳回 Postgres 权威。
pub async fn reconcile(cache: &Cache, pool: &PgPool, tenant_id: &str) -> i64 {
    let n = db::count_active_runs(pool, tenant_id).await.unwrap_or(0);
    let _ = cache
        .set_ex(&Cache::rl_conc_key(tenant_id), &n.to_string(), 600)
        .await;
    n
}
