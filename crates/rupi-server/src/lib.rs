//! 无状态云控制面：AG-UI + 薄 REST。不链 `rupi-tui`。

#![allow(clippy::too_many_arguments)]

pub mod agui;
pub mod auth;
pub mod cache;
pub mod db;
pub mod http;
pub mod memory;
pub mod quota;
pub mod reclaim;
pub mod run;
pub mod tools;

pub use cache::Cache;

use crate::db::{PgPool, Tenant};
use rupi_llm::{LlmProvider, MockProvider};
use rupi_runtime::{Executor, LocalObjectStore, ObjectStore, PoolNode, PoolScheduler};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;

pub type ProviderFactory = Arc<dyn Fn(&Tenant) -> Arc<dyn LlmProvider> + Send + Sync>;

#[derive(Clone)]
pub struct IdleConfig {
    pub enabled: bool,
    pub idle_after: Duration,
    pub tick: Duration,
}

impl Default for IdleConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            idle_after: Duration::from_secs(30 * 60),
            tick: Duration::from_secs(15),
        }
    }
}

#[derive(Clone)]
pub struct App {
    pub pool: PgPool,
    pub read_pool: Option<PgPool>,
    pub cache: Cache,
    pub executor: Arc<dyn Executor>,
    pub object_store: Arc<dyn ObjectStore>,
    pub provider_factory: ProviderFactory,
    pub instance_id: String,
    pub idle: IdleConfig,
    /// 租户级 mock 剧本必须跨 run 复用，否则每轮都从第一条重新开始。
    mock_providers: Arc<Mutex<HashMap<String, Arc<MockProvider>>>>,
}

impl App {
    pub fn new(
        pool: PgPool,
        cache: Cache,
        executor: Arc<dyn Executor>,
        provider_factory: ProviderFactory,
    ) -> Self {
        Self {
            pool,
            read_pool: None,
            cache,
            executor,
            object_store: Arc::new(LocalObjectStore::new(
                std::env::temp_dir().join(format!("rupi-obj-{}", std::process::id())),
            )),
            provider_factory,
            instance_id: uuid::Uuid::new_v4().to_string(),
            idle: IdleConfig::default(),
            mock_providers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn with_instance_id(mut self, id: impl Into<String>) -> Self {
        self.instance_id = id.into();
        self
    }

    pub fn with_object_store(mut self, store: Arc<dyn ObjectStore>) -> Self {
        self.object_store = store;
        self
    }

    pub fn with_idle(mut self, idle: IdleConfig) -> Self {
        self.idle = idle;
        self
    }

    pub fn with_read_pool(mut self, pool: PgPool) -> Self {
        self.read_pool = Some(pool);
        self
    }

    pub fn reader(&self) -> &PgPool {
        self.read_pool.as_ref().unwrap_or(&self.pool)
    }

    pub fn provider_for(&self, tenant: &Tenant) -> Arc<dyn LlmProvider> {
        {
            let g = self.mock_providers.lock().unwrap();
            if let Some(p) = g.get(&tenant.id) {
                return p.clone();
            }
        }
        if let Some(p) = run::cached_mock(tenant) {
            self.mock_providers
                .lock()
                .unwrap()
                .insert(tenant.id.clone(), p.clone());
            p
        } else {
            (self.provider_factory)(tenant)
        }
    }
}

pub struct CloudConfig {
    pub database_url: String,
    pub database_read_url: Option<String>,
    pub redis_url: String,
    pub executor_url: String,
    pub executor_urls: Vec<String>,
    pub executor_token: String,
    pub bind: String,
    pub instance_id: String,
    pub snapshot_dir: String,
    pub idle_secs: u64,
}

pub async fn connect_app(cfg: &CloudConfig) -> anyhow::Result<App> {
    let pool = db::connect(&cfg.database_url).await?;
    db::migrate(&pool).await?;
    let read_pool = match &cfg.database_read_url {
        Some(u) if !u.is_empty() => Some(db::connect(u).await?),
        _ => None,
    };
    let cache = Cache::connect(&cfg.redis_url).await;
    let urls = if cfg.executor_urls.is_empty() {
        vec![cfg.executor_url.clone()]
    } else {
        cfg.executor_urls.clone()
    };
    let nodes: Vec<PoolNode> = urls
        .iter()
        .enumerate()
        .map(|(i, u)| {
            let id = if urls.len() == 1 {
                "remote-http".into()
            } else {
                format!("remote-http-{}", i + 1)
            };
            PoolNode {
                id,
                executor: Arc::new(rupi_runtime::http::HttpExecutor::new(
                    u.clone(),
                    cfg.executor_token.clone(),
                )),
            }
        })
        .collect();
    let executor: Arc<dyn Executor> = Arc::new(PoolScheduler::new(nodes));
    let store: Arc<dyn ObjectStore> = Arc::new(LocalObjectStore::new(&cfg.snapshot_dir));
    let mut app = App::new(
        pool,
        cache,
        executor,
        Arc::new(|t| run::default_provider(t)),
    )
    .with_instance_id(cfg.instance_id.clone())
    .with_object_store(store)
    .with_idle(IdleConfig {
        enabled: true,
        idle_after: Duration::from_secs(cfg.idle_secs.max(1)),
        tick: Duration::from_secs(15),
    });
    if let Some(r) = read_pool {
        app = app.with_read_pool(r);
    }
    Ok(app)
}

pub async fn serve(cfg: CloudConfig) -> anyhow::Result<()> {
    let app = connect_app(&cfg).await?;
    let (_addr, h) = spawn(app, &cfg.bind).await?;
    h.await?
}

pub async fn spawn(
    app: App,
    bind: &str,
) -> anyhow::Result<(std::net::SocketAddr, tokio::task::JoinHandle<anyhow::Result<()>>)> {
    let listener = TcpListener::bind(bind).await?;
    let addr = listener.local_addr()?;
    tracing::info!("rupi-server listen {addr} instance={}", app.instance_id);
    let bg = app.clone();
    tokio::spawn(async move {
        reclaim::loop_forever(bg).await;
    });
    let router = http::router(app);
    let handle = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .map_err(|e| anyhow::anyhow!(e))
    });
    Ok((addr, handle))
}
