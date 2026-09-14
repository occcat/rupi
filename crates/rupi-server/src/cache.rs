//! Redis 只做缓存。写先 Postgres，再删或覆盖 key。挂了则跳过，直读库。
//!
//! 水平扩展：
//! - key 带 `{tenant}` hash tag，Redis Cluster 同槽
//! - 单机用 `ConnectionManager`；Cluster 用 `cluster_async`（`--redis-cluster` /
//!   `REDIS_CLUSTER` / `redis-cluster://` / 逗号多 seed）
//! - 回填用 Redis `SET NX` 跨副本 singleflight
//! - 不存在的 session 负向短 TTL
//! - `kill()` 让所有副本克隆立刻降级

use redis::aio::ConnectionManager;
use redis::cluster::ClusterClient;
use redis::cluster_async::ClusterConnection;
use redis::AsyncCommands;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, RwLock};

pub const SESS_META_TTL_SECS: u64 = 300;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RedisMode {
    Standalone,
    Cluster,
}

#[derive(Clone)]
enum RedisHandle {
    Standalone(ConnectionManager),
    Cluster(ClusterConnection),
}

#[derive(Clone)]
pub struct Cache {
    redis: Arc<RwLock<Option<RedisHandle>>>,
    flights: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    requested: RedisMode,
}

impl Cache {
    pub async fn connect(url: &str) -> Self {
        Self::connect_with(url, wants_cluster(url)).await
    }

    pub async fn connect_with(url: &str, cluster: bool) -> Self {
        if cluster {
            Self::connect_cluster(url).await
        } else {
            Self::connect_standalone(url).await
        }
    }

    async fn connect_standalone(url: &str) -> Self {
        match redis::Client::open(url) {
            Ok(c) => match tokio::time::timeout(CONNECT_TIMEOUT, ConnectionManager::new(c)).await {
                Ok(Ok(mgr)) => {
                    tracing::info!("redis cache ready");
                    Self::ready(RedisHandle::Standalone(mgr), RedisMode::Standalone)
                }
                Ok(Err(e)) => {
                    tracing::warn!("redis unavailable ({e:#}); degrade to postgres-only");
                    Self::disabled_mode(RedisMode::Standalone)
                }
                Err(_) => {
                    tracing::warn!("redis connect timed out; degrade to postgres-only");
                    Self::disabled_mode(RedisMode::Standalone)
                }
            },
            Err(e) => {
                tracing::warn!("redis url invalid ({e:#}); degrade");
                Self::disabled_mode(RedisMode::Standalone)
            }
        }
    }

    async fn connect_cluster(url: &str) -> Self {
        let nodes = cluster_nodes(url);
        if nodes.is_empty() {
            tracing::warn!("redis cluster url empty; degrade");
            return Self::disabled_mode(RedisMode::Cluster);
        }
        match ClusterClient::new(nodes) {
            Ok(c) => match tokio::time::timeout(CONNECT_TIMEOUT, c.get_async_connection()).await {
                Ok(Ok(conn)) => {
                    tracing::info!("redis cluster cache ready");
                    Self::ready(RedisHandle::Cluster(conn), RedisMode::Cluster)
                }
                Ok(Err(e)) => {
                    tracing::warn!("redis cluster unavailable ({e:#}); degrade to postgres-only");
                    Self::disabled_mode(RedisMode::Cluster)
                }
                Err(_) => {
                    tracing::warn!("redis cluster connect timed out; degrade to postgres-only");
                    Self::disabled_mode(RedisMode::Cluster)
                }
            },
            Err(e) => {
                tracing::warn!("redis cluster url invalid ({e:#}); degrade");
                Self::disabled_mode(RedisMode::Cluster)
            }
        }
    }

    fn ready(handle: RedisHandle, requested: RedisMode) -> Self {
        Self {
            redis: Arc::new(RwLock::new(Some(handle))),
            flights: Arc::new(Mutex::new(HashMap::new())),
            requested,
        }
    }

    pub fn disabled() -> Self {
        Self::disabled_mode(RedisMode::Standalone)
    }

    fn disabled_mode(requested: RedisMode) -> Self {
        Self {
            redis: Arc::new(RwLock::new(None)),
            flights: Arc::new(Mutex::new(HashMap::new())),
            requested,
        }
    }

    pub fn requested_mode(&self) -> RedisMode {
        self.requested
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

    async fn conn(&self) -> Option<RedisHandle> {
        self.redis.read().await.clone()
    }

    pub async fn get(&self, key: &str) -> Option<String> {
        match self.conn().await? {
            RedisHandle::Standalone(mut c) => c.get::<_, Option<String>>(key).await.ok().flatten(),
            RedisHandle::Cluster(mut c) => c.get::<_, Option<String>>(key).await.ok().flatten(),
        }
    }

    pub async fn set_ex(&self, key: &str, val: &str, ttl: u64) -> bool {
        match self.conn().await {
            Some(RedisHandle::Standalone(mut c)) => {
                c.set_ex::<_, _, ()>(key, val, ttl).await.is_ok()
            }
            Some(RedisHandle::Cluster(mut c)) => c.set_ex::<_, _, ()>(key, val, ttl).await.is_ok(),
            None => false,
        }
    }

    pub async fn del(&self, key: &str) {
        match self.conn().await {
            Some(RedisHandle::Standalone(mut c)) => {
                let _: Result<(), _> = c.del(key).await;
            }
            Some(RedisHandle::Cluster(mut c)) => {
                let _: Result<(), _> = c.del(key).await;
            }
            None => {}
        }
    }

    pub async fn expire(&self, key: &str, ttl: u64) -> bool {
        match self.conn().await {
            Some(RedisHandle::Standalone(mut c)) => {
                c.expire::<_, bool>(key, ttl as i64).await.unwrap_or(false)
            }
            Some(RedisHandle::Cluster(mut c)) => {
                c.expire::<_, bool>(key, ttl as i64).await.unwrap_or(false)
            }
            None => false,
        }
    }

    pub async fn put_session_meta(&self, tenant: &str, id: &str, meta: &str, ttl: u64) -> bool {
        self.set_ex(&Self::sess_meta_key(tenant, id), meta, ttl)
            .await
    }

    pub async fn get_session_meta(&self, tenant: &str, id: &str) -> Option<String> {
        self.get(&Self::sess_meta_key(tenant, id)).await
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
        match self.conn().await? {
            RedisHandle::Standalone(mut c) => incr_ex(&mut c, key, ttl).await,
            RedisHandle::Cluster(mut c) => incr_ex(&mut c, key, ttl).await,
        }
    }

    pub async fn decr(&self, key: &str) {
        match self.conn().await {
            Some(RedisHandle::Standalone(mut c)) => {
                let _: Result<i64, _> = c.decr(key, 1).await;
            }
            Some(RedisHandle::Cluster(mut c)) => {
                let _: Result<i64, _> = c.decr(key, 1).await;
            }
            None => {}
        }
    }

    pub async fn set_nx_ex(&self, key: &str, val: &str, ttl: u64) -> bool {
        match self.conn().await {
            Some(RedisHandle::Standalone(mut c)) => set_nx_ex(&mut c, key, val, ttl).await,
            Some(RedisHandle::Cluster(mut c)) => set_nx_ex(&mut c, key, val, ttl).await,
            None => false,
        }
    }
}

async fn incr_ex<C: AsyncCommands>(c: &mut C, key: &str, ttl: u64) -> Option<i64> {
    let n: i64 = c.incr(key, 1).await.ok()?;
    if n == 1 {
        let _: Result<(), _> = c.expire(key, ttl as i64).await;
    }
    Some(n)
}

async fn set_nx_ex<C: redis::aio::ConnectionLike + Send>(
    c: &mut C,
    key: &str,
    val: &str,
    ttl: u64,
) -> bool {
    let r: Result<Option<String>, _> = redis::cmd("SET")
        .arg(key)
        .arg(val)
        .arg("NX")
        .arg("EX")
        .arg(ttl)
        .query_async(c)
        .await;
    matches!(r, Ok(Some(_)))
}

/// URL 带 Cluster 方案、多 seed，或调用方显式要求。
pub fn wants_cluster(url: &str) -> bool {
    let t = url.trim();
    if t.is_empty() {
        return false;
    }
    let lower = t.to_ascii_lowercase();
    lower.starts_with("redis-cluster://")
        || lower.starts_with("rediss-cluster://")
        || lower.starts_with("cluster+redis://")
        || lower.starts_with("cluster+rediss://")
        || t.contains(',')
}

/// `--redis-cluster`：把单机 URL 改成 Cluster 方案，便于 `connect()` 识别。
pub fn prefer_cluster_url(url: &str) -> String {
    if wants_cluster(url) {
        return url.to_string();
    }
    if let Some(rest) = strip_prefix_ci(url, "rediss://") {
        return format!("rediss-cluster://{rest}");
    }
    if let Some(rest) = strip_prefix_ci(url, "redis://") {
        return format!("redis-cluster://{rest}");
    }
    url.to_string()
}

pub fn cluster_nodes(url: &str) -> Vec<String> {
    url.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(normalize_node_url)
        .collect()
}

fn normalize_node_url(url: &str) -> String {
    if let Some(rest) = strip_prefix_ci(url, "redis-cluster://") {
        return format!("redis://{rest}");
    }
    if let Some(rest) = strip_prefix_ci(url, "rediss-cluster://") {
        return format!("rediss://{rest}");
    }
    if let Some(rest) = strip_prefix_ci(url, "cluster+redis://") {
        return format!("redis://{rest}");
    }
    if let Some(rest) = strip_prefix_ci(url, "cluster+rediss://") {
        return format!("rediss://{rest}");
    }
    url.to_string()
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() >= prefix.len() && s[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redis_url() -> String {
        std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into())
    }

    #[test]
    fn keys_use_hash_tags() {
        let k = Cache::lease_key("t1", "th");
        assert!(k.starts_with("{t1}:"), "{k}");
        assert!(Cache::sess_tree_key("t1", "s").contains("{t1}"));
        assert!(Cache::sess_meta_key("t1", "s").starts_with("{t1}:sess:s:meta"));
        assert_ne!(Cache::lease_key("a", "x"), Cache::lease_key("b", "x"));
    }

    #[test]
    fn cluster_url_selects_cluster_mode() {
        assert!(wants_cluster("redis-cluster://127.0.0.1:7000"));
        assert!(wants_cluster("rediss-cluster://example:6379"));
        assert!(wants_cluster("cluster+redis://a:1"));
        assert!(wants_cluster("redis://a:6379,redis://b:6379"));
        assert!(!wants_cluster("redis://127.0.0.1:6379"));
        assert!(!wants_cluster("rediss://127.0.0.1:6379"));
        assert_eq!(
            prefer_cluster_url("redis://127.0.0.1:6379"),
            "redis-cluster://127.0.0.1:6379"
        );
        assert_eq!(
            prefer_cluster_url("rediss://h:6379"),
            "rediss-cluster://h:6379"
        );
        assert_eq!(
            cluster_nodes("redis-cluster://127.0.0.1:7000,redis://127.0.0.1:7001"),
            vec![
                "redis://127.0.0.1:7000".to_string(),
                "redis://127.0.0.1:7001".to_string()
            ]
        );
    }

    #[test]
    fn cluster_client_builds_from_cluster_url() {
        let nodes = cluster_nodes("redis-cluster://127.0.0.1:7000,redis://127.0.0.1:7001");
        assert!(
            ClusterClient::new(nodes).is_ok(),
            "cluster-async ClusterClient must accept hash-tagged seed URLs"
        );
    }

    #[test]
    fn disabled_session_meta_is_a_noop() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let cache = Cache::disabled();
            assert!(!cache
                .put_session_meta("t", "s", "{\"id\":\"s\"}", SESS_META_TTL_SECS)
                .await);
            assert!(cache.get_session_meta("t", "s").await.is_none());
        });
    }

    #[tokio::test]
    async fn sess_meta_roundtrip_and_invalidate() {
        let cache = Cache::connect(&redis_url()).await;
        if !cache.available().await {
            return;
        }
        assert_eq!(cache.requested_mode(), RedisMode::Standalone);
        let tenant = format!("meta-{}", uuid::Uuid::new_v4());
        let id = "sess-1";
        let payload = r#"{"sessionId":"sess-1","name":"n","model":"m"}"#;
        assert!(
            cache
                .put_session_meta(&tenant, id, payload, SESS_META_TTL_SECS)
                .await
        );
        assert_eq!(
            cache.get_session_meta(&tenant, id).await.as_deref(),
            Some(payload)
        );
        cache.invalidate_session(&tenant, id).await;
        assert!(cache.get_session_meta(&tenant, id).await.is_none());
    }

    #[tokio::test]
    async fn cluster_flag_uses_cluster_client() {
        let url = redis_url();
        let standalone = Cache::connect_with(&url, false).await;
        if !standalone.available().await {
            return;
        }
        let via_url = Cache::connect(&prefer_cluster_url(&url)).await;
        assert_eq!(via_url.requested_mode(), RedisMode::Cluster);
        // 单机 Redis 没有 CLUSTER SLOTS，Cluster 客户端必须走这条路并降级。
        assert!(
            !via_url.available().await,
            "cluster client must handshake CLUSTER, not reuse ConnectionManager"
        );
        let via_flag = Cache::connect_with(&url, true).await;
        assert_eq!(via_flag.requested_mode(), RedisMode::Cluster);
        assert!(!via_flag.available().await);
    }
}
