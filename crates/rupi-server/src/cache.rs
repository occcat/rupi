//! Redis 只做缓存。写先 Postgres，再删 key。挂了则跳过，直读库。
//!
//! 水平扩展：
//! - key 带 `{tenant}` hash tag，Redis Cluster 同槽
//! - 回填用 Redis `SET NX` 跨副本 singleflight
//! - 不存在的 session 负向短 TTL
//! - `kill()` 让所有副本克隆立刻降级

use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

#[derive(Clone)]
pub struct Cache {
    redis: Arc<RwLock<Option<ConnectionManager>>>,
    flights: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

impl Cache {
    pub async fn connect(url: &str) -> Self {
        match redis::Client::open(url) {
            Ok(c) => match ConnectionManager::new(c).await {
                Ok(mgr) => {
                    tracing::info!("redis cache ready");
                    Self {
                        redis: Arc::new(RwLock::new(Some(mgr))),
                        flights: Arc::new(Mutex::new(HashMap::new())),
                    }
                }
                Err(e) => {
                    tracing::warn!("redis unavailable ({e:#}); degrade to postgres-only");
                    Self::disabled()
                }
            },
            Err(e) => {
                tracing::warn!("redis url invalid ({e:#}); degrade");
                Self::disabled()
            }
        }
    }

    pub fn disabled() -> Self {
        Self {
            redis: Arc::new(RwLock::new(None)),
            flights: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 测试/演练：丢掉 Redis，后续读走 Postgres，限流收紧。
    pub async fn kill(&self) {
        *self.redis.write().await = None;
    }

    pub async fn available(&self) -> bool {
        self.redis.read().await.is_some()
    }

    /// Redis Cluster 同槽：`{tenant}:…`
    fn tag(tenant: &str) -> String {
        format!("{{{tenant}}}")
    }

    pub fn sess_meta_key(tenant: &str, id: &str) -> String {
        format!("{}:sess:{id}:meta", Self::tag(tenant))
    }
    pub fn sess_tree_key(tenant: &str, id: &str) -> String {
        format!("{}:sess:{id}:tree", Self::tag(tenant))
    }
    pub fn mem_frozen_key(tenant: &str) -> String {
        format!("{}:mem:frozen", Self::tag(tenant))
    }
    pub fn mem_frozen_session_key(tenant: &str, session: &str) -> String {
        format!("{}:mem:{session}:frozen", Self::tag(tenant))
    }
    pub fn lease_key(tenant: &str, thread: &str) -> String {
        format!("{}:lease:run:{thread}", Self::tag(tenant))
    }
    pub fn rl_qps_key(tenant: &str) -> String {
        format!("{}:rl:qps", Self::tag(tenant))
    }
    pub fn rl_conc_key(tenant: &str) -> String {
        format!("{}:rl:conc", Self::tag(tenant))
    }
    pub fn fill_key(kind: &str, tenant: &str, id: &str) -> String {
        format!("{}:fill:{kind}:{id}", Self::tag(tenant))
    }
    pub fn neg_session_key(tenant: &str, id: &str) -> String {
        format!("{}:neg:sess:{id}", Self::tag(tenant))
    }

    async fn conn(&self) -> Option<ConnectionManager> {
        self.redis.read().await.clone()
    }

    pub async fn get(&self, key: &str) -> Option<String> {
        let mut c = self.conn().await?;
        c.get::<_, Option<String>>(key).await.ok().flatten()
    }

    pub async fn set_ex(&self, key: &str, val: &str, ttl: u64) -> bool {
        let Some(mut c) = self.conn().await else {
            return false;
        };
        c.set_ex::<_, _, ()>(key, val, ttl).await.is_ok()
    }

    pub async fn del(&self, key: &str) {
        if let Some(mut c) = self.conn().await {
            let _: Result<(), _> = c.del(key).await;
        }
    }

    pub async fn expire(&self, key: &str, ttl: u64) -> bool {
        let Some(mut c) = self.conn().await else {
            return false;
        };
        c.expire::<_, bool>(key, ttl as i64).await.unwrap_or(false)
    }

    pub async fn invalidate_session(&self, tenant: &str, id: &str) {
        self.del(&Self::sess_meta_key(tenant, id)).await;
        self.del(&Self::sess_tree_key(tenant, id)).await;
        self.del(&Self::neg_session_key(tenant, id)).await;
        self.del(&Self::mem_frozen_session_key(tenant, id)).await;
    }

    pub async fn invalidate_memory(&self, tenant: &str) {
        self.del(&Self::mem_frozen_key(tenant)).await;
    }

    pub async fn remember_missing_session(&self, tenant: &str, id: &str) {
        let _ = self
            .set_ex(&Self::neg_session_key(tenant, id), "1", 15)
            .await;
    }

    pub async fn is_missing_session(&self, tenant: &str, id: &str) -> bool {
        self.get(&Self::neg_session_key(tenant, id)).await.is_some()
    }

    pub async fn clear_missing_session(&self, tenant: &str, id: &str) {
        self.del(&Self::neg_session_key(tenant, id)).await;
    }

    /// 进程内一把锁 + Redis NX 跨副本。返回 (local_guard, 是否拿到跨副本锁)。
    pub async fn singleflight(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut g = self.flights.lock().await;
        g.entry(key.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    pub async fn fill_lock(&self, key: &str, ttl: u64) -> bool {
        if !self.available().await {
            return true;
        }
        self.set_nx_ex(key, "1", ttl).await
    }

    pub async fn fill_unlock(&self, key: &str) {
        self.del(key).await;
    }

    pub async fn incr_ex(&self, key: &str, ttl: u64) -> Option<i64> {
        let mut c = self.conn().await?;
        let n: i64 = c.incr(key, 1).await.ok()?;
        if n == 1 {
            let _: Result<(), _> = c.expire(key, ttl as i64).await;
        }
        Some(n)
    }

    pub async fn decr(&self, key: &str) {
        if let Some(mut c) = self.conn().await {
            let _: Result<i64, _> = c.decr(key, 1).await;
        }
    }

    pub async fn set_nx_ex(&self, key: &str, val: &str, ttl: u64) -> bool {
        let Some(mut c) = self.conn().await else {
            return false;
        };
        let r: Result<Option<String>, _> = redis::cmd("SET")
            .arg(key)
            .arg(val)
            .arg("NX")
            .arg("EX")
            .arg(ttl)
            .query_async(&mut c)
            .await;
        matches!(r, Ok(Some(_)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_use_hash_tags() {
        let k = Cache::lease_key("t1", "th");
        assert!(k.starts_with("{t1}:"), "{k}");
        assert!(Cache::sess_tree_key("t1", "s").contains("{t1}"));
        assert_ne!(Cache::lease_key("a", "x"), Cache::lease_key("b", "x"));
    }
}
