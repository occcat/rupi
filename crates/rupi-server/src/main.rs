//! `rupi-server`：无状态控制面。不链 TUI。

use clap::Parser;
use rupi_server::{auth, db, CloudConfig};

#[derive(Parser)]
#[command(name = "rupi-server", about = "Rupi cloud control plane")]
struct Cli {
    #[arg(long, env = "RUPI_LISTEN", default_value = "127.0.0.1:8080")]
    listen: String,
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,
    /// 只读副本，可选。hydrate / 写路径仍走主库。
    #[arg(long, env = "DATABASE_READ_URL")]
    database_read_url: Option<String>,
    #[arg(long, env = "REDIS_URL", default_value = "redis://127.0.0.1:6379")]
    redis_url: String,
    #[arg(long, env = "RUPI_EXECUTOR_URL", default_value = "")]
    executor_url: String,
    /// 逗号分隔的多个 execd。可写 `region=url`。优先于单个 URL。
    #[arg(long, env = "RUPI_EXECUTOR_URLS", default_value = "")]
    executor_urls: String,
    /// 第二种后端：sandbox 集群。逗号分隔，可写 `region=url`。
    #[arg(long, env = "RUPI_SANDBOX_URLS", default_value = "")]
    sandbox_urls: String,
    #[arg(long, env = "RUPI_EXEC_TOKEN", default_value = "")]
    executor_token: String,
    /// 本控制面所在区域（无状态副本可跨区；执行按会话 region 调度）。
    #[arg(long, env = "RUPI_REGION", default_value = "local")]
    region: String,
    #[arg(long, env = "RUPI_INSTANCE_ID")]
    instance_id: Option<String>,
    #[arg(long, env = "RUPI_SNAPSHOT_DIR", default_value = "/tmp/rupi-snapshots")]
    snapshot_dir: String,
    #[arg(long, env = "RUPI_IDLE_SECS", default_value_t = 1800)]
    idle_secs: u64,
    /// 启动时建一个租户并打印明文 Key（只用于本地/CI）。
    #[arg(long)]
    bootstrap_tenant: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let cli = Cli::parse();
    let urls: Vec<String> = cli
        .executor_urls
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let sandbox_urls: Vec<String> = cli
        .sandbox_urls
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let cfg = CloudConfig {
        database_url: cli.database_url,
        database_read_url: cli.database_read_url,
        redis_url: cli.redis_url,
        executor_url: cli.executor_url,
        executor_urls: urls,
        sandbox_urls,
        executor_token: cli.executor_token,
        bind: cli.listen,
        instance_id: cli
            .instance_id
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        region: cli.region,
        snapshot_dir: cli.snapshot_dir,
        idle_secs: cli.idle_secs,
    };
    if let Some(name) = cli.bootstrap_tenant {
        let pool = db::connect(&cfg.database_url).await?;
        db::migrate(&pool).await?;
        let key = auth::generate_key();
        let t = db::create_tenant(&pool, &name, &key).await?;
        println!("tenant_id={} key={key} name={}", t.id, t.name);
        return Ok(());
    }
    rupi_server::serve(cfg).await
}
