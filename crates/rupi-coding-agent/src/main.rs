use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use rupi_ai::{FauxProvider, FauxScript};
use rupi_coding_agent::harness::{default_options, resolve_model, Harness};
use rupi_coding_agent::UPSTREAM;
use serde_json::json;

#[derive(Parser, Debug)]
#[command(name = "rupi", about = "Rust recreation of Pi coding agent with MCP, Hermes memory, and skill self-accumulation")]
struct Cli {
    /// Model spec, e.g. openai/gpt-4o or anthropic/claude-sonnet-4-5
    #[arg(long, short)]
    model: Option<String>,

    /// Working directory
    #[arg(long)]
    cwd: Option<PathBuf>,

    /// One-shot prompt (print mode, matching `pi -p`)
    #[arg(long, short = 'p')]
    print: Option<String>,

    /// JSON output for print mode
    #[arg(long)]
    json: bool,

    /// Disable skill discovery/accumulation
    #[arg(long)]
    no_skills: bool,

    /// Disable memory
    #[arg(long)]
    no_memory: bool,

    /// Use the deterministic faux provider (tests / demos)
    #[arg(long)]
    faux: bool,

    /// Scripted faux replies, repeatable
    #[arg(long)]
    faux_text: Vec<String>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Print version and upstream alignment
    Version,
    /// List built-in models
    Models,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("rupi=info".parse().unwrap()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.command {
        Some(Commands::Version) => {
            println!("rupi {} (upstream {UPSTREAM})", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some(Commands::Models) => {
            for m in rupi_ai::list_models() {
                println!("{}/{}  ctx={}  api={:?}", m.provider, m.id, m.context_window, m.api);
            }
            return Ok(());
        }
        None => {}
    }

    let cwd = cli
        .cwd
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let mut options = default_options(cwd);
    if let Some(spec) = cli.model {
        options.model = resolve_model(&spec);
    }
    if cli.no_memory {
        options.settings.memory_enabled = false;
    }
    if cli.no_skills {
        options.settings.skill_accumulation = false;
        options.accumulate = false;
    }
    if cli.faux || !cli.faux_text.is_empty() {
        let scripts = if cli.faux_text.is_empty() {
            vec![FauxScript::text("Hello from rupi faux provider.")]
        } else {
            cli.faux_text.into_iter().map(FauxScript::text).collect()
        };
        options.faux = Some(Arc::new(FauxProvider::new(scripts)));
        options.model = resolve_model("faux/faux");
    }

    let mut harness = Harness::bootstrap(options).await?;

    if let Some(prompt) = cli.print {
        let out = harness.print(&prompt).await?;
        if cli.json {
            println!(
                "{}",
                json!({
                    "text": out.text,
                    "events": out.events.iter().map(|e| e.kind()).collect::<Vec<_>>(),
                    "accumulation": out.accumulation.map(|a| json!({
                        "memories": a.memories.len(),
                        "skills": a.skills.len(),
                    })),
                    "upstream": UPSTREAM,
                })
            );
        } else {
            println!("{}", out.text);
        }
        return Ok(());
    }

    println!("rupi — Pi {UPSTREAM} recreation. Type /quit to exit.");
    harness.repl().await?;
    Ok(())
}
