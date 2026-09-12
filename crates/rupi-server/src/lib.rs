//! 无状态云控制面：AG-UI + 薄 REST。不链 `rupi-tui`。

#![allow(clippy::too_many_arguments)]

pub mod agui;
pub mod auth;
pub mod cache;
pub mod db;
pub mod http;
pub mod memory;
pub mod quota;
pub mod run;
pub mod tools;

pub use cache::Cache;

use crate::db::{PgPool, Tenant};
use rupi_llm::LlmProvider;
use rupi_runtime::Executor;
use std::sync::Arc;
use tokio::net::TcpListener;

pub type ProviderFactory = Arc<dyn Fn(&Tenant) -> Arc<dyn LlmProvider> + Send + Sync>;

#[derive(Clone)]
pub struct App {
    pub pool: PgPool,
    pub cache: Cache,
    pub executor: Arc<dyn Executor>,
    pub provider_factory: ProviderFactory,
}

impl App {
    pub fn provider_for(&self, tenant: &Tenant) -> Arc<dyn LlmProvider> {
        (self.provider_factory)(tenant)
    }
}

pub struct CloudConfig {
    pub database_url: String,
    pub redis_url: String,
    pub executor_url: String,
    pub executor_token: String,
    pub bind: String,
}

pub async fn connect_app(cfg: &CloudConfig) -> anyhow::Result<App> {
    let pool = db::connect(&cfg.database_url).await?;
    db::migrate(&pool).await?;
    let cache = Cache::connect(&cfg.redis_url).await;
    let executor: Arc<dyn Executor> = Arc::new(rupi_runtime::http::HttpExecutor::new(
        cfg.executor_url.clone(),
        cfg.executor_token.clone(),
    ));
    Ok(App {
        pool,
        cache,
        executor,
        provider_factory: Arc::new(|t| run::default_provider(t)),
    })
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
    tracing::info!("rupi-server listen {addr}");
    let router = http::router(app);
    let handle = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .map_err(|e| anyhow::anyhow!(e))
    });
    Ok((addr, handle))
}
