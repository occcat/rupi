//! rupi CLI：coding agent 交互入口 + MCP / 记忆 / Skill / 会话管理子命令。

use clap::{Parser, Subcommand};
use rupi_agent::{AgentLoop, HeuristicReviewer, ReviewSuggestion};
use rupi_core::SessionTree;
use rupi_llm::{LlmProvider, MockProvider, OpenAiCompatProvider};
use rupi_memory::{MemoryManager, MemoryStore, SessionStore};
use rupi_skills::{SkillAccumulator, SkillRegistry};
use rupi_tools::ToolRegistry;
use std::path::PathBuf;
use std::sync::Arc;

/// 把 review 建议落盘：memory add 写 `MEMORY.md`，skill 草稿写 `~/.rupi/skills/<name>/`。
/// 已存在的 skill 跳过（不覆盖人工成果），失败只打印不中断聊天。
fn apply_suggestions(home: &PathBuf, pending: &Arc<std::sync::Mutex<Vec<ReviewSuggestion>>>) {
    let suggestions: Vec<ReviewSuggestion> = pending.lock().unwrap().drain(..).collect();
    if suggestions.is_empty() {
        return;
    }
    let store = MemoryStore::new(home.clone());
    let acc = SkillAccumulator::new(home.join("skills"));
    for s in suggestions {
        for m in &s.memory_ops {
            match store.apply_write("add", &m.entry) {
                Ok(_) => println!("[review] memory saved"),
                Err(e) => eprintln!("[review] memory save failed: {e:#}"),
            }
        }
        if let Some(d) = &s.skill_draft {
            match acc.propose(&d.name, &d.description, &d.steps) {
                Ok(dir) => println!("[review] skill drafted at {}", dir.display()),
                Err(e) => eprintln!("[review] skill draft skipped: {e:#}"),
            }
        }
    }
}

#[derive(Parser)]
#[command(
    name = "rupi",
    version,
    about = "rupi — Pi Agent 的 Rust 复刻：最小 Harness + MCP + 记忆 + Skills"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// 模型名（OpenAI-compatible）
    #[arg(long, default_value = "gpt-4o-mini")]
    model: String,
    #[arg(long, default_value_t = 20)]
    max_turns: u32,
    /// MCP server 配置 JSON 文件（数组）：[{"name":..,"command":..,"args":[..],"env":{..}}]
    #[arg(long)]
    mcp_config: Option<PathBuf>,
    /// 每轮结束后后台 review，给出记忆/Skill 沉淀建议
    #[arg(long, default_value_t = false)]
    review: bool,
    /// 把 review 建议直接落盘（memory add + skill 草稿）
    #[arg(long, default_value_t = false)]
    review_apply: bool,
    /// 会话压缩阈值（历史字符数，超限摘要最旧部分）
    #[arg(long, default_value_t = 60_000)]
    compress_threshold: usize,
    /// 压缩后保留的近期消息条数
    #[arg(long, default_value_t = 20)]
    compress_keep: usize,
}

#[derive(Subcommand)]
enum Cmd {
    /// 交互式聊天（默认）
    Chat,
    /// 全屏终端界面（ratatui）
    Tui,
    /// 显示记忆快照
    MemoryShow,
    /// 写入记忆
    MemoryWrite { op: String, entry: String },
    /// 列出 skills
    SkillsList,
    /// 加载 skill 全文
    SkillLoad { name: String },
    /// 从步骤提炼新 skill（自积累）
    SkillDistill {
        name: String,
        description: String,
        steps: Vec<String>,
    },
    /// 会话全文检索
    SessionSearch { query: String },
    /// MCP tools/list 探活
    McpList { command: String, args: Vec<String> },
}

fn home_dir() -> PathBuf {
    std::env::var("RUPI_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs_home().join(".rupi"))
}

fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

fn skill_dirs(home: &PathBuf) -> Vec<PathBuf> {
    vec![
        PathBuf::from("skills/builtin"),
        home.join("skills"),
        PathBuf::from(".rupi/skills"),
    ]
}

async fn build_provider(model: &str) -> anyhow::Result<Box<dyn LlmProvider>> {
    match OpenAiCompatProvider::from_env(model.to_string()) {
        Ok(p) => Ok(Box::new(p)),
        Err(_) => {
            eprintln!("[rupi] no RUPI_API_KEY/OPENAI_API_KEY — using mock provider (demo mode)");
            Ok(Box::new(MockProvider::new(vec![
                MockProvider::text_response(
                    "demo mode：设置 RUPI_API_KEY 后可接真实模型。已收到你的请求，工具链就绪。",
                ),
            ])))
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .try_init()
        .ok();
    let cli = Cli::parse();
    let home = home_dir();

    match cli.cmd {
        Some(Cmd::MemoryShow) => {
            let store = MemoryStore::new(home);
            let frozen = store.frozen_snapshot();
            println!(
                "--- MEMORY.md ---\n{}\n--- USER.md ---\n{}",
                frozen.memory, frozen.user
            );
        }
        Some(Cmd::MemoryWrite { op, entry }) => {
            let store = MemoryStore::new(home);
            let live = store.apply_write(&op, &entry)?;
            println!("updated. live state:\n{live}");
        }
        Some(Cmd::SkillsList) => {
            let reg = SkillRegistry::discover(&skill_dirs(&home));
            println!("{}", reg.index_block());
        }
        Some(Cmd::SkillLoad { name }) => {
            let reg = SkillRegistry::discover(&skill_dirs(&home));
            match reg.load_skill(&name) {
                Some(body) => println!("{body}"),
                None => eprintln!("unknown skill: {name}"),
            }
        }
        Some(Cmd::SkillDistill {
            name,
            description,
            steps,
        }) => {
            let acc = SkillAccumulator::new(home.join("skills"));
            let dir = acc.propose(&name, &description, &steps)?;
            println!("skill drafted at {}", dir.display());
        }
        Some(Cmd::SessionSearch { query }) => {
            let store = SessionStore::open(&home)?;
            for (sid, snippet) in store.search(&query, 10)? {
                println!("[{sid}] {snippet}");
            }
        }
        Some(Cmd::McpList { command, args }) => {
            let cfg = rupi_mcp::McpServerConfig::new("probe", &command, args);
            let bridge = rupi_mcp::McpBridge::spawn(cfg).await?;
            for t in bridge.list_tools().await? {
                let d = rupi_mcp::mcp_tool_to_definition("mcp", &t);
                println!("{} — {}", d.name, d.description);
            }
        }
        Some(Cmd::Chat) | None => {
            run_chat(&cli, &home).await?;
        }
        Some(Cmd::Tui) => {
            run_tui(&cli, &home).await?;
        }
    }
    Ok(())
}

async fn run_chat(cli: &Cli, home: &PathBuf) -> anyhow::Result<()> {
    let provider = build_provider(&cli.model).await?;
    let pending: Arc<std::sync::Mutex<Vec<ReviewSuggestion>>> =
        Arc::new(std::sync::Mutex::new(vec![]));
    let mut agent =
        AgentLoop::new(cli.max_turns).with_compression(cli.compress_threshold, cli.compress_keep);
    if cli.review || cli.review_apply {
        let pending_clone = pending.clone();
        agent = agent.with_reviewer(
            Arc::new(HeuristicReviewer::default()),
            Arc::new(move |s: ReviewSuggestion| {
                println!(
                    "\n[review] memory_ops={} skill={}",
                    s.memory_ops.len(),
                    s.skill_draft
                        .as_ref()
                        .map(|d| d.name.as_str())
                        .unwrap_or("-")
                );
                for m in &s.memory_ops {
                    println!("[review] memory add: {}", m.entry);
                }
                if let Some(d) = &s.skill_draft {
                    println!("[review] skill draft: {} — {}", d.name, d.description);
                }
                pending_clone.lock().unwrap().push(s);
            }),
        );
    }
    let mut tools = ToolRegistry::with_builtins();
    // MCP-Direct：spawn 各 server 并把远端工具注册为原生工具（失败只 warning，不断主循环）
    let _mcp = if let Some(path) = &cli.mcp_config {
        let configs = rupi_mcp::load_configs(path)?;
        let manager = rupi_mcp::McpManager::spawn_all(&configs).await?;
        let names = manager.register_all(&mut tools, &configs).await;
        println!("[mcp] {} tools: {}", names.len(), names.join(", "));
        Some(manager)
    } else {
        None
    };
    let store = MemoryStore::new(home.clone());
    let frozen = store.frozen_snapshot();
    let mem = MemoryManager::new(store);
    let skills = SkillRegistry::discover(&skill_dirs(home));
    let mut session = SessionTree::new();

    println!("rupi v0.1.0 — 输入 /quit 退出，/rewind 回退，/skills 看技能");
    let stdin = std::io::stdin();
    let mut line = String::new();
    loop {
        line.clear();
        print!("> ");
        use std::io::Write as _;
        std::io::stdout().flush()?;
        if stdin.read_line(&mut line)? == 0 {
            break;
        }
        let input = line.trim().to_string();
        if input.is_empty() {
            continue;
        }
        if input == "/quit" {
            break;
        }
        if input == "/skills" {
            println!("{}", skills.index_block());
            continue;
        }
        if input == "/rewind" {
            if session.current_path.len() >= 2 {
                let target = session.current_path[session.current_path.len() - 2].clone();
                session.rewind_to(&target);
                println!("[rewound]");
            }
            continue;
        }
        agent
            .run(
                provider.as_ref(),
                &mut session,
                &input,
                &tools,
                &mem,
                &frozen,
                &skills,
                &[],
                &|e| match e {
                    rupi_core::AgentEvent::TextDelta { delta } => print!("{delta}"),
                    rupi_core::AgentEvent::ToolStart { name, .. } => println!("\n[tool {name}]…"),
                    rupi_core::AgentEvent::ToolEnd {
                        name,
                        content,
                        is_error,
                        ..
                    } => {
                        println!(
                            "\n[{name} {}]\n{content}",
                            if is_error { "error" } else { "ok" }
                        )
                    }
                    _ => {}
                },
            )
            .await?;
        if cli.review_apply {
            apply_suggestions(home, &pending);
        }
        println!();
    }
    Ok(())
}

async fn run_tui(cli: &Cli, home: &PathBuf) -> anyhow::Result<()> {
    let provider = build_provider(&cli.model).await?;
    let mut tools = ToolRegistry::with_builtins();
    let _mcp = if let Some(path) = &cli.mcp_config {
        let configs = rupi_mcp::load_configs(path)?;
        let manager = rupi_mcp::McpManager::spawn_all(&configs).await?;
        let names = manager.register_all(&mut tools, &configs).await;
        eprintln!("[mcp] {} tools: {}", names.len(), names.join(", "));
        Some(manager)
    } else {
        None
    };
    let store = MemoryStore::new(home.clone());
    let frozen = store.frozen_snapshot();
    let mem = MemoryManager::new(store);
    let skills = SkillRegistry::discover(&skill_dirs(home));
    let mut session = SessionTree::new();
    let mut agent =
        AgentLoop::new(cli.max_turns).with_compression(cli.compress_threshold, cli.compress_keep);
    let review_lines: Option<Arc<std::sync::Mutex<Vec<String>>>> = if cli.review || cli.review_apply
    {
        Some(Arc::new(std::sync::Mutex::new(vec![])))
    } else {
        None
    };
    if let Some(buf) = review_lines.clone() {
        let home_clone = home.clone();
        let apply = cli.review_apply;
        agent = agent.with_reviewer(
            Arc::new(HeuristicReviewer::default()),
            Arc::new(move |s: ReviewSuggestion| {
                let mut lines = buf.lock().unwrap();
                for m in &s.memory_ops {
                    lines.push(format!("[review] memory add: {}", m.entry));
                }
                if let Some(d) = &s.skill_draft {
                    lines.push(format!(
                        "[review] skill draft: {} — {}",
                        d.name, d.description
                    ));
                }
                if apply {
                    let store = MemoryStore::new(home_clone.clone());
                    for m in &s.memory_ops {
                        match store.apply_write("add", &m.entry) {
                            Ok(_) => lines.push("[review] memory saved".into()),
                            Err(e) => lines.push(format!("[review] memory save failed: {e:#}")),
                        }
                    }
                    if let Some(d) = &s.skill_draft {
                        let acc = SkillAccumulator::new(home_clone.join("skills"));
                        match acc.propose(&d.name, &d.description, &d.steps) {
                            Ok(dir) => {
                                lines.push(format!("[review] skill drafted at {}", dir.display()))
                            }
                            Err(e) => lines.push(format!("[review] skill draft skipped: {e:#}")),
                        }
                    }
                }
            }),
        );
    }
    let ctx = rupi_tui::TuiContext {
        provider: provider.as_ref(),
        agent: &agent,
        session: &mut session,
        tools: &tools,
        mem: &mem,
        frozen: &frozen,
        skills: &skills,
        review_lines,
    };
    rupi_tui::launch(ctx).await
}
