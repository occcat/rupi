//! 外部 sandbox 集群节点：工作区与用户命令只在本进程落地。

use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "rupi-sandboxd", about = "Rupi sandbox-cluster executor daemon")]
struct Cli {
    #[arg(long, default_value = "127.0.0.1:8190")]
    listen: String,
    #[arg(long, default_value = "/tmp/rupi-sandboxd")]
    root: PathBuf,
    #[arg(long, env = "RUPI_EXEC_TOKEN", default_value = "")]
    token: String,
    #[arg(long, env = "RUPI_SANDBOX_MAX", default_value_t = 64)]
    max_sandboxes: u32,
    #[arg(long, env = "RUPI_SANDBOX_WARM", default_value_t = 2)]
    warm_pool: u32,
    #[arg(long, env = "RUPI_REGION", default_value = "local")]
    region: String,
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
    std::fs::create_dir_all(&cli.root)?;
    rupi_runtime::sandbox::serve(rupi_runtime::sandbox::SandboxdConfig {
        bind: cli.listen,
        root: cli.root,
        token: cli.token,
        max_sandboxes: cli.max_sandboxes,
        warm_pool: cli.warm_pool,
        region: cli.region,
    })
    .await
}
