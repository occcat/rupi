//! rupi — Rust port of the Pi coding agent harness, plus Hermes-style memory and skill self-accumulation.

mod args;
mod config;
mod run;

use anyhow::Result;
use args::{parse_args, print_help};
use config::AppPaths;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("rupi=info".parse().unwrap()),
        )
        .with_writer(std::io::stderr)
        .init();

    if let Err(e) = real_main().await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

async fn real_main() -> Result<()> {
    let args = parse_args(std::env::args().skip(1).collect());
    if args.help {
        print_help();
        return Ok(());
    }
    if args.version {
        println!("rupi {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let paths = AppPaths::resolve(args.session_dir.clone())?;
    paths.ensure()?;
    run::run(args, paths).await
}
