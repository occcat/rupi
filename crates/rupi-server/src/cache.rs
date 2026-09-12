//! Redis 只做缓存。写先 Postgres，再删 key。挂了则跳过，直读库。

use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct Cache {
    redis: Option<ConnectionManager>,
    flights: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

impl Cache {
    pub async fn connect(url: &str) -> Self {
        match redis::Client::open(url) {
            Ok(c) => match ConnectionManager::new(c).await {
                Ok(mgr) => {
                    tracing::info!("redis cache ready");
                    Self {
                        redis: Some(mgr),
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
            redis: None,
            flights: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn available(&self) -> bool {
        self.redis.is_some()
    }

    pub fn sess_meta_key(tenant: &str, id: &str) -> String {
        format!("sess:{tenant}:{id}:meta")
    }
    pub fn sess_tree_key(tenant: &str, id: &str) -> String {
        format!("sess:{tenant}:{id}:tree")
    }
    pub fn mem_frozen_key(tenant: &str) -> String {
        format!("mem:{tenant}:frozen")
    }
    pub fn lease_key(thread: &str) -> String {
        format!("lease:run:{thread}")
    }
    pub fn rl_qps_key(tenant: &str) -> String {
        format!("rl:{tenant}:qps")
    }
    pub fn rl_conc_key(tenant: &str) -> String {
        format!("rl:{tenant}:conc")
    }

    async fn conn(&self) -> Option<ConnectionManager> {
        self.redis.clone()
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

    pub async fn invalidate_session(&self, tenant: &str, id: &str) {
        self.del(&Self::sess_meta_key(tenant, id)).await;
        self.del(&Self::sess_tree_key(tenant, id)).await;
    }

    pub async fn invalidate_memory(&self, tenant: &str) {
        self.del(&Self::mem_frozen_key(tenant)).await;
    }

    /// 同一 key 回填一把锁（进程内 singleflight）。
    pub async fn singleflight(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut g = self.flights.lock().await;
        g.entry(key.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
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
