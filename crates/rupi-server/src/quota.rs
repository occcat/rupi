use crate::cache::Cache;
use crate::db::{self, PgPool, Tenant};

#[derive(Debug)]
pub enum Admit {
    Ok,
    TooMany,
}

/// 入院：并发 run + 粗 QPS。Redis 挂了则收紧（只靠 Postgres 乐观锁 + 拒绝超额 QPS）。
pub async fn admit_run(cache: &Cache, pool: &PgPool, tenant: &Tenant) -> Admit {
    let max = tenant.max_concurrent_runs.max(1) as i64;
    if cache.available() {
        let n = cache
            .incr_ex(&Cache::rl_conc_key(&tenant.id), 600)
            .await
            .unwrap_or(max + 1);
        if n > max {
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
        // 无 Redis：收紧，仅允许 1 个并发（靠 sessions.run_id 乐观锁在调用方再拦）。
        if tenant.max_concurrent_runs < 1 {
            return Admit::TooMany;
        }
        let _ = db::bump_quota_run(pool, &tenant.id).await;
        Admit::Ok
    }
}

pub async fn release_run(cache: &Cache, tenant_id: &str) {
    cache.decr(&Cache::rl_conc_key(tenant_id)).await;
}

pub async fn acquire_lease(cache: &Cache, pool: &PgPool, tenant_id: &str, thread: &str, run_id: &str) -> bool {
    if cache.available() {
        let ok = cache
            .set_nx_ex(&Cache::lease_key(thread), run_id, 180)
            .await;
        if !ok {
            return false;
        }
    }
    db::set_run_id(pool, tenant_id, thread, Some(run_id))
        .await
        .unwrap_or(false)
}

pub async fn release_lease(cache: &Cache, pool: &PgPool, tenant_id: &str, thread: &str) {
    cache.del(&Cache::lease_key(thread)).await;
    let _ = db::set_run_id(pool, tenant_id, thread, None).await;
}
