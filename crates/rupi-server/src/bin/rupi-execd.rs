//! 与 `rupi-runtime` 同入口，方便 `rupi-server` 集成测试拿到 `CARGO_BIN_EXE_rupi-execd`。

use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "rupi-execd", about = "Rupi remote executor daemon")]
struct Cli {
    #[arg(long, default_value = "127.0.0.1:8090")]
    listen: String,
    #[arg(long, default_value = "/tmp/rupi-execd")]
    root: PathBuf,
    #[arg(long, env = "RUPI_EXEC_TOKEN", default_value = "")]
    token: String,
    #[arg(long, env = "RUPI_EXEC_MAX", default_value_t = 64)]
    max_workspaces: u32,
    #[arg(long, env = "RUPI_EXEC_WARM", default_value_t = 2)]
    warm_pool: u32,
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
    rupi_runtime::execd::serve(rupi_runtime::execd::ExecdConfig {
        bind: cli.listen,
        root: cli.root,
        token: cli.token,
        max_workspaces: cli.max_workspaces,
        warm_pool: cli.warm_pool,
    })
    .await
}
