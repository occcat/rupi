//! 无状态云控制面：AG-UI + 薄 REST。不链 `rupi-tui`。

#![allow(clippy::too_many_arguments)]

pub mod admin;
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
use rupi_runtime::{
    parse_endpoint_list, BackendKind, Executor, LocalObjectStore, ObjectStore, PoolNode,
    PoolScheduler, SandboxExecutor,
};
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
    pub region: String,
    pub idle: IdleConfig,
    /// 环境变量 / `--admin-token` 共享口令。空则只认库里的 admin Key。
    pub admin_token: Option<String>,
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
            region: "local".into(),
            idle: IdleConfig::default(),
            admin_token: None,
            mock_providers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn with_instance_id(mut self, id: impl Into<String>) -> Self {
        self.instance_id = id.into();
        self
    }

    pub fn with_region(mut self, region: impl Into<String>) -> Self {
        self.region = region.into();
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

    pub fn with_admin_token(mut self, token: impl Into<String>) -> Self {
        let t = token.into();
        self.admin_token = if t.trim().is_empty() {
            None
        } else {
            Some(t)
        };
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
    pub sandbox_urls: Vec<String>,
    pub executor_token: String,
    pub bind: String,
    pub instance_id: String,
    pub region: String,
    pub snapshot_dir: String,
    pub idle_secs: u64,
    pub admin_token: String,
}

pub async fn connect_app(cfg: &CloudConfig) -> anyhow::Result<App> {
    let pool = db::connect(&cfg.database_url).await?;
    db::migrate(&pool).await?;
    let read_pool = match &cfg.database_read_url {
        Some(u) if !u.is_empty() => Some(db::connect(u).await?),
        _ => None,
    };
    let cache = Cache::connect(&cfg.redis_url).await;
    let executor = build_executor(cfg);
    let store: Arc<dyn ObjectStore> = Arc::new(LocalObjectStore::new(&cfg.snapshot_dir));
    let mut app = App::new(
        pool,
        cache,
        executor,
        Arc::new(|t| run::default_provider(t)),
    )
    .with_instance_id(cfg.instance_id.clone())
    .with_region(cfg.region.clone())
    .with_object_store(store)
    .with_idle(IdleConfig {
        enabled: true,
        idle_after: Duration::from_secs(cfg.idle_secs.max(1)),
        tick: Duration::from_secs(15),
    })
    .with_admin_token(cfg.admin_token.clone());
    if let Some(r) = read_pool {
        app = app.with_read_pool(r);
    }
    Ok(app)
}

fn node_id(kind: &str, region: &str, index: usize, total: usize) -> String {
    if total == 1 && (region.is_empty() || region == "local") {
        kind.to_string()
    } else if total == 1 {
        format!("{kind}:{region}")
    } else {
        format!("{kind}:{region}:{index}")
    }
}

fn build_executor(cfg: &CloudConfig) -> Arc<dyn Executor> {
    let mut nodes = Vec::new();
    let exec_raw = if !cfg.executor_urls.is_empty() {
        cfg.executor_urls.join(",")
    } else {
        cfg.executor_url.clone()
    };
    let exec_eps = parse_endpoint_list(&exec_raw, &cfg.region);
    let n_exec = exec_eps.len();
    for (i, (region, url)) in exec_eps.into_iter().enumerate() {
        if url.is_empty() {
            continue;
        }
        nodes.push(
            PoolNode::new(
                node_id("remote-http", &region, i + 1, n_exec),
                Arc::new(rupi_runtime::http::HttpExecutor::new(
                    url,
                    cfg.executor_token.clone(),
                )),
            )
            .with_region(region)
            .with_kind(BackendKind::RemoteHttp),
        );
    }
    let sb_raw = cfg.sandbox_urls.join(",");
    let sb_eps = parse_endpoint_list(&sb_raw, &cfg.region);
    let n_sb = sb_eps.len();
    for (i, (region, url)) in sb_eps.into_iter().enumerate() {
        if url.is_empty() {
            continue;
        }
        nodes.push(
            PoolNode::new(
                node_id("sandbox", &region, i + 1, n_sb),
                Arc::new(SandboxExecutor::new(url, cfg.executor_token.clone())),
            )
            .with_region(region)
            .with_kind(BackendKind::Sandbox),
        );
    }
    Arc::new(PoolScheduler::new(nodes))
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
    tracing::info!(
        "rupi-server listen {addr} instance={} region={}",
        app.instance_id,
        app.region
    );
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
