//! rupi CLI：coding agent 交互入口 + MCP / 记忆 / Skill / 会话管理子命令。

use clap::{Parser, Subcommand};
use rupi_agent::{AgentLoop, HeuristicReviewer, ReviewSuggestion, SubagentTool};
use rupi_core::SessionTree;
use rupi_llm::{LlmProvider, MockProvider};
use rupi_memory::{MemoryManager, MemoryProvider, MemoryStore, SessionStore};
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
    let store = memory_store(home, true);
    let acc = SkillAccumulator::new(home.join("skills"));
    for s in suggestions {
        for m in &s.memory_ops {
            match store.apply_write("add", &m.entry) {
                Ok(_) => println!("[review] memory saved"),
                Err(e) => eprintln!("[review] memory save failed: {e:#}"),
            }
        }
        for f in &s.failures {
            match store.record_failure(f) {
                Ok(()) => println!("[review] failure saved"),
                Err(e) => eprintln!("[review] failure save failed: {e:#}"),
            }
        }
        if let Some(d) = &s.skill_draft {
            if acc.exists(&d.name) {
                tracing::debug!("[review] skill {} already exists, skip", d.name);
                continue;
            }
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
    /// 模型：`name` 或 `provider/model[:thinking]`（openai|anthropic|gemini|openrouter|azure|bedrock|vertex）
    #[arg(long, default_value = "gpt-4o-mini")]
    model: String,
    /// 覆盖当前 provider 的 API key（仍可读对应环境变量）
    #[arg(long)]
    api_key: Option<String>,
    /// 强制 provider（覆盖模型名前缀与 `provider/` 段）
    #[arg(long)]
    provider: Option<String>,
    /// 打印 models.json 目录（内置 + ~/.rupi/models.json）后退出
    #[arg(long, default_value_t = false)]
    list_models: bool,
    #[arg(long, default_value_t = 20)]
    max_turns: u32,
    /// MCP server 配置 JSON 文件（数组）：[{"name":..,"command":..,"args":[..],"env":{..}}]
    #[arg(long)]
    mcp_config: Option<PathBuf>,
    /// 每轮结束后后台 review，给出记忆/Skill 沉淀建议（默认已开启启发式复盘，此 flag 保留兼容）
    #[arg(long, default_value_t = false)]
    review: bool,
    /// 关闭默认开启的每轮启发式复盘（--review-llm 的模型复盘不受此影响，需显式传才开）
    #[arg(long, default_value_t = false)]
    no_review: bool,
    /// 把 review 建议直接落盘（memory add + skill 草稿）
    #[arg(long, default_value_t = false)]
    review_apply: bool,
    /// 用模型做后台复盘（默认启发式离线 review；LLM 版烧 token 但提炼质量更高）
    #[arg(long, default_value_t = false)]
    review_llm: bool,
    /// 会话压缩阈值（历史字符数，超限摘要最旧部分；RUPI_COMPRESSION_OVERRIDES 可按模型覆盖）
    #[arg(long, default_value_t = 60_000)]
    compress_threshold: usize,
    /// 压缩后保留的近期消息条数（同上可按模型覆盖）
    #[arg(long, default_value_t = 20)]
    compress_keep: usize,
    /// 外部扩展目录（*.json manifests），默认 ~/.rupi/extensions
    #[arg(long)]
    ext_dir: Option<PathBuf>,
    /// 计划模式：只侦察不动手（禁 write/edit/bash）
    #[arg(long, default_value_t = false)]
    plan: bool,
    /// 启用 subagent 委托工具（模型可把子任务派给子会话，深度 guard 防递归）
    #[arg(long, default_value_t = false)]
    subagents: bool,
    /// 外部记忆 provider（jsonl：turns.jsonl 回放 + recall 工具）
    #[arg(long)]
    memory_provider: Option<String>,
    /// 恢复历史会话继续聊（sessions 命令看 id）
    #[arg(long)]
    resume: Option<String>,
    /// 工具并发执行（对标上游 toolExecution: parallel；默认串行，审批问询保序）
    #[arg(long, default_value_t = false)]
    parallel_tools: bool,
    /// 本次直接信任项目资源（跳过信任提问，不记住）
    #[arg(long, default_value_t = false)]
    trust_project: bool,
    /// 渐进式工具发现：只注入常驻工具 schema，其余按关键词搜出后再可见（省上下文）
    #[arg(long, default_value_t = false)]
    discover_tools: bool,
    /// Ask 裁决自动放行（危险：非交互/脚本用；交互模式默认问询）
    #[arg(long, default_value_t = false)]
    approve: bool,
    /// Ask 裁决一律拒绝且不问（与 --approve 互斥）
    #[arg(long, default_value_t = false)]
    no_approve: bool,
    /// 思考强度（off|low|medium|high|xhigh|max；也可写在 `--model provider/model:high`）
    #[arg(long)]
    thinking: Option<String>,
    /// 回合内禁用内建记忆（MEMORY.md/USER.md 不注入、memory 工具与指导块撤下；
    /// 对标 Hermes memory_enabled=false；显式记忆子命令与外部 provider 不受影响）
    #[arg(long, default_value_t = false)]
    no_memory: bool,
}

impl Cli {
    /// 后台复盘总开关：离开启发式默认开启（空建议零打扰，非空才打印）；
    /// `--review` 是旧显式开关，保留兼容；`--no-review` 关闭。
    /// 落盘仍需 `--review-apply` 显式授权（无自主写盘，对标 Hermes write_approval 精神）。
    fn review_enabled(&self) -> bool {
        !self.no_review || self.review
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// 交互式聊天（默认）
    Chat,
    /// 全屏终端界面（ratatui）
    Tui,
    /// 非交互执行一次并退出（对标 pi -p；问询默认拒绝，加 --approve 放行）
    Run {
        /// 任务描述（多词自动拼接，无需引号）
        prompt: Vec<String>,
        /// JSONL 事件流（对标 pi --mode json）：stdout 每行一个 AgentEvent，
        /// 末行 `run_result`（session_id / stop_reason / 最终文本）；出错时 `error` 行 + 非零退出
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// 显示记忆快照
    MemoryShow,
    /// 写入记忆（op: add/replace/remove/failure；scope: global/project）
    MemoryWrite {
        op: String,
        entry: String,
        #[arg(long, default_value = "global")]
        scope: String,
    },
    /// 列出 skills
    SkillsList,
    /// 列出自定义斜杠命令（commands/*.md）
    Commands,
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
    /// 记忆检索（MEMORY.md / failures 镜像）
    MemorySearch { query: String },
    /// 列出外部扩展工具
    ExtList,
    /// 列出最近会话
    Sessions,
    /// 查看会话明细
    SessionShow { id: String },
    /// OAuth 登录占位（本版只打印用法；完整设备码流未落地）
    Login {
        /// anthropic | openai | copilot | vertex | bedrock
        provider: Option<String>,
    },
    /// 打印模型目录（同 `--list-models`）
    Models,
    /// MCP 探活：tools/resources/prompts 三区段（stdio 命令或 `--url` 二选一）
    McpList {
        command: String,
        args: Vec<String>,
        /// StreamableHTTP 端点；给出即走 HTTP 而非 spawn stdio（此时 command/args 忽略）
        #[arg(long)]
        url: Option<String>,
    },
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

/// 上下文文件搜索起点：信任项目读 cwd 链，否则只读全局 home（cwd=home，
/// 祖先链退化为 home→根，与上游 agentDir 行为一致）。
fn context_cwd(load_project: bool, home: &std::path::Path) -> PathBuf {
    if load_project {
        std::env::current_dir().unwrap_or_else(|_| home.to_path_buf())
    } else {
        home.to_path_buf()
    }
}

/// 内建记忆 store：全局 `~/.rupi/memories` + 从 cwd 上溯 `.git` 的项目层（Hermes two-tier）。
/// `load_project` 为 false（项目信任被拒）时只挂全局层，项目 `MEMORY.md` 不读不写。
fn memory_store(home: &PathBuf, load_project: bool) -> MemoryStore {
    let mut s = MemoryStore::new(home.clone());
    if load_project {
        if let Ok(cwd) = std::env::current_dir() {
            if let Some(root) = MemoryStore::discover_project(&cwd) {
                s = s.with_project(root);
            }
        }
    }
    s
}

/// 项目资源探测（只读）：有项目根且现存本地资源时返回 (root, 清单），否则 None。
/// 发现语义与加载侧一致：cwd 相对路径。
fn project_resources() -> Option<(PathBuf, Vec<String>)> {
    let cwd = std::env::current_dir().ok()?;
    let root = match MemoryStore::discover_project(&cwd) {
        Some(r) => r,
        // 回退：无 .git 的普通目录自带 .rupi 资源也视为项目（否则永不设门）
        None if PathBuf::from(".rupi/skills").exists()
            || PathBuf::from(".rupi/commands").exists()
            || PathBuf::from(".rupi")
                .join(rupi_memory::MEMORY_FILE)
                .exists() =>
        {
            cwd.clone()
        }
        None => return None,
    };
    let mut resources: Vec<String> = vec![];
    let pm = root.join(".rupi").join(rupi_memory::MEMORY_FILE);
    if pm.exists() {
        resources.push(pm.display().to_string());
    }
    for rel in [".rupi/skills", ".rupi/commands"] {
        if PathBuf::from(rel).exists() {
            resources.push(rel.to_owned());
        }
    }
    if resources.is_empty() {
        return None;
    }
    Some((root, resources))
}

/// 项目信任门（对标上游 project_trust）：项目根有本地资源且未被记住时问一次。
/// 返回是否加载项目资源。无项目根 / 无项目资源 / 已记住 → true 不打扰；
/// 非交互（管道/EOF）默认跳过并提示。
fn load_project_resources(home: &PathBuf, cli: &Cli) -> bool {
    let (root, resources) = match project_resources() {
        Some(r) => r,
        None => return true,
    };
    let mut store = rupi_core::trust::TrustStore::open(home.join("trusted_projects"));
    if store.contains(&root) {
        return true;
    }
    if cli.trust_project {
        println!(
            "[trust] --trust-project: 本次加载项目资源 {}",
            root.display()
        );
        return true;
    }
    match rupi_core::trust::ask_trust_stdin(&root, &resources) {
        rupi_core::trust::TrustAnswer::Always => {
            if let Err(e) = store.add(&root) {
                eprintln!("[trust] 记住失败：{e:#}");
            }
            true
        }
        rupi_core::trust::TrustAnswer::Once => true,
        rupi_core::trust::TrustAnswer::Skip => {
            println!("[trust] 已跳过项目资源，只用全局记忆/skills/命令");
            false
        }
    }
}

fn skill_dirs(home: &PathBuf, load_project: bool) -> Vec<PathBuf> {
    let mut dirs = vec![builtin_skills_dir(), home.join("skills")];
    // 项目 skills 与项目记忆同门：信任被拒则不发现、不加载
    if load_project {
        dirs.push(PathBuf::from(".rupi/skills"));
    }
    dirs
}

/// 内建 skills 目录：从 exe 所在位置向上找 `skills/builtin`
///（`cargo run` 与安装后都对）；找不到回退 cwd 相对路径（仓库根跑二进制的老行为）。
///此前纯 `skills/builtin` 相对路径：换个 cwd 跑就静默丢失内建技能。
fn builtin_skills_dir() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent().map(|p| p.to_path_buf());
        for _ in 0..5 {
            match dir {
                Some(d) => {
                    let cand = d.join("skills/builtin");
                    if cand.is_dir() {
                        return cand;
                    }
                    dir = d.parent().map(|p| p.to_path_buf());
                }
                None => break,
            }
        }
    }
    PathBuf::from("skills/builtin")
}

/// 自定义命令目录：与 skills 同门，信任被拒只留全局 `~/commands`。
fn command_dirs_filtered(home: &PathBuf, load_project: bool) -> Vec<PathBuf> {
    if load_project {
        rupi_core::commands::command_dirs(home)
    } else {
        vec![home.join("commands")]
    }
}

/// 工作区沙箱根：启动时 cwd（canonicalize 消解符号链接），read/write/edit 约束其内。
fn sandbox_root() -> PathBuf {
    std::env::current_dir()
        .ok()
        .and_then(|p| p.canonicalize().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 沙箱工具表：文件工具约束在工作区内，相对路径按 root 解析（subagent 克隆继承）。
fn sandboxed_tools() -> ToolRegistry {
    let root = sandbox_root();
    // 诊断走 stderr：`run --json` 的 stdout 必须是纯 JSONL
    eprintln!("[sandbox workspace: {}]", root.display());
    ToolRegistry::with_sandboxed_builtins(&root)
}

fn ext_dir(home: &PathBuf, cli: &Cli) -> PathBuf {
    cli.ext_dir
        .clone()
        .unwrap_or_else(|| home.join("extensions"))
}

/// 启动时加载扩展并注册；返回 loader（REPL 每轮 `refresh()` 热重载）。
fn load_extensions(tools: &mut ToolRegistry, dir: &PathBuf) -> rupi_ext::ExtensionSet {
    let mut set = rupi_ext::ExtensionSet::new(dir.clone());
    let manifests = set.load_all();
    if !manifests.is_empty() {
        eprintln!(
            "[ext] {} extensions from {}",
            manifests.len(),
            dir.display()
        );
    }
    set.register(tools, manifests);
    set
}

fn provider_options(cli: &Cli) -> rupi_llm::ProviderOptions {
    rupi_llm::ProviderOptions {
        api_key: cli.api_key.clone(),
        provider: cli.provider.clone(),
    }
}

fn apply_cli_secrets(cli: &Cli) {
    let Some(k) = cli.api_key.as_deref() else {
        return;
    };
    match cli.provider.as_deref().map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("anthropic") => std::env::set_var("RUPI_ANTHROPIC_KEY", k),
        Some("gemini") => std::env::set_var("RUPI_GEMINI_KEY", k),
        Some("openrouter") => std::env::set_var("RUPI_OPENROUTER_KEY", k),
        Some("azure") => std::env::set_var("AZURE_OPENAI_API_KEY", k),
        Some("bedrock") => std::env::set_var("AWS_BEARER_TOKEN_BEDROCK", k),
        Some("vertex") => std::env::set_var("VERTEX_TOKEN", k),
        _ => std::env::set_var("RUPI_API_KEY", k),
    }
}

fn print_login_stub(provider: Option<&str>) {
    println!("OAuth login is stubbed in this release (no browser/device-code flow).");
    println!("Use an API key instead, e.g. `--api-key $KEY --model provider/model`.\n");
    let detail = match provider.map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("anthropic") | Some("claude") => {
            "anthropic: export ANTHROPIC_API_KEY or RUPI_ANTHROPIC_KEY\n  planned: Claude Pro/Max OAuth → ~/.rupi/oauth/anthropic.json"
        }
        Some("openai") | Some("codex") | Some("chatgpt") => {
            "openai/codex: export OPENAI_API_KEY or RUPI_API_KEY\n  planned: ChatGPT Codex OAuth → ~/.rupi/oauth/openai.json"
        }
        Some("copilot") | Some("github") => {
            "copilot: not implemented\n  planned: GitHub device-code → ~/.rupi/oauth/copilot.json"
        }
        Some("vertex") | Some("google") => {
            "vertex: export VERTEX_TOKEN or GOOGLE_OAUTH_ACCESS_TOKEN\n  (gcloud auth print-access-token is the current workaround)"
        }
        Some("bedrock") | Some("aws") => {
            "bedrock: export AWS_BEARER_TOKEN_BEDROCK or BEDROCK_API_KEY"
        }
        Some(other) => {
            println!("unknown login provider '{other}'.");
            ""
        }
        None => {
            "providers you can pass: anthropic | openai | copilot | vertex | bedrock\n  none of these OAuth flows are implemented yet."
        }
    };
    if !detail.is_empty() {
        println!("{detail}");
    }
}

async fn build_provider(
    model: &str,
    session_id: Option<&str>,
    opts: &rupi_llm::ProviderOptions,
) -> anyhow::Result<Box<dyn LlmProvider>> {
    let spec = rupi_llm::parse_model_spec(model);
    let mut p = match rupi_llm::provider_from_spec(&spec, opts) {
        Ok(p) => p,
        Err(e) => {
            let hint = spec
                .provider
                .as_deref()
                .unwrap_or_else(|| {
                    if spec.model.starts_with("claude-") {
                        "anthropic"
                    } else if spec.model.starts_with("gemini-") {
                        "gemini"
                    } else {
                        "openai"
                    }
                });
            let demo = match hint {
                "anthropic" => {
                    eprintln!("[rupi] {e:#} — using mock provider (demo mode)");
                    "demo mode：设置 RUPI_ANTHROPIC_KEY 后可接 Claude。已收到你的请求，工具链就绪。"
                }
                "gemini" | "vertex" => {
                    eprintln!("[rupi] {e:#} — using mock provider (demo mode)");
                    "demo mode：设置 RUPI_GEMINI_KEY / VERTEX_TOKEN 后可接 Gemini。已收到你的请求，工具链就绪。"
                }
                "openrouter" => {
                    eprintln!("[rupi] {e:#} — using mock provider (demo mode)");
                    "demo mode：设置 OPENROUTER_API_KEY 后可接 OpenRouter。"
                }
                "azure" => {
                    eprintln!("[rupi] {e:#} — using mock provider (demo mode)");
                    "demo mode：设置 AZURE_OPENAI_API_KEY + AZURE_OPENAI_ENDPOINT。"
                }
                "bedrock" => {
                    eprintln!("[rupi] {e:#} — using mock provider (demo mode)");
                    "demo mode：设置 AWS_BEARER_TOKEN_BEDROCK。"
                }
                _ => {
                    eprintln!(
                        "[rupi] no RUPI_API_KEY/OPENAI_API_KEY — using mock provider (demo mode)"
                    );
                    "demo mode：设置 RUPI_API_KEY 后可接真实模型。已收到你的请求，工具链就绪。"
                }
            };
            Box::new(MockProvider::new(vec![MockProvider::text_response(demo)]))
                as Box<dyn LlmProvider>
        }
    };
    rupi_llm::apply_session_settings(&mut *p, session_id);
    Ok(p)
}

fn emit_ext_hints(set: &rupi_ext::ExtensionSet) {
    for (source, hint) in set.drain_ui_hints() {
        eprintln!("[ui {source}/{}] {}", hint.kind, hint.message);
    }
}

fn user_turn(text: &str) -> rupi_core::Message {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    rupi_core::Message::from_blocks(
        rupi_core::Role::User,
        rupi_core::commands::expand_at_mentions_blocks(text, &cwd),
    )
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .try_init()
        .ok();
    let cli = Cli::parse();
    apply_cli_secrets(&cli);
    if cli.list_models {
        print!("{}", rupi_llm::format_catalog(&rupi_llm::load_models()));
        return Ok(());
    }
    let home = home_dir();

    match cli.cmd {
        Some(Cmd::MemoryShow) => {
            let store = memory_store(&home, true);
            let frozen = store.frozen_snapshot();
            println!(
                "--- MEMORY.md ---\n{}\n--- USER.md ---\n{}\n--- failures.md ---\n{}",
                frozen.memory, frozen.user, frozen.failures
            );
        }
        Some(Cmd::MemoryWrite { op, entry, scope }) => {
            let store = memory_store(&home, true);
            if op == "failure" {
                store.record_failure(&entry)?;
                println!("failure recorded.");
            } else {
                let live = store.apply_write_scoped(&scope, &op, &entry)?;
                println!("updated [{scope}]. live state:\n{live}");
            }
        }
        Some(Cmd::SkillsList) => {
            let reg = SkillRegistry::discover(&skill_dirs(&home, true));
            // 无技能时工具定义为空（渐进披露无入口），给提示而非光杆标题块
            if reg.tool_definitions().is_empty() {
                println!(
                    "no skills found. distill one with `skill-distill <name> <desc> <steps...>`"
                );
            } else {
                println!("{}", reg.index_block());
            }
        }
        Some(Cmd::Commands) => {
            println!(
                "{}",
                rupi_core::commands::index_block(&command_dirs_filtered(&home, true))
            );
        }
        Some(Cmd::SkillLoad { name }) => {
            let reg = SkillRegistry::discover(&skill_dirs(&home, true));
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
            let hits = store.search(&query, 10)?;
            if hits.is_empty() {
                println!("no matching sessions for `{query}`");
            }
            for (sid, snippet) in hits {
                println!("[{sid}] {snippet}");
            }
        }
        Some(Cmd::MemorySearch { query }) => {
            let store = SessionStore::open(&home)?;
            let hits = store.memory_search(&query, 10)?;
            if hits.is_empty() {
                println!("no matching memories for `{query}`");
            }
            for (target, snippet) in hits {
                println!("[{target}] {snippet}");
            }
        }
        Some(Cmd::ExtList) => {
            let dir = ext_dir(&home, &cli);
            let mut set = rupi_ext::ExtensionSet::new(dir.clone());
            let manifests = set.load_all();
            if manifests.is_empty() {
                println!("no extensions in {} (*.json manifests)", dir.display());
            }
            for m in manifests {
                println!(
                    "{} [{}] — {}",
                    m.name,
                    match m.protocol {
                        rupi_ext::ExtProtocol::Jsonrpc => "jsonrpc",
                        rupi_ext::ExtProtocol::Oneshot => "oneshot",
                    },
                    m.description
                );
            }
        }
        Some(Cmd::Login { provider }) => {
            print_login_stub(provider.as_deref());
        }
        Some(Cmd::Models) => {
            print!("{}", rupi_llm::format_catalog(&rupi_llm::load_models()));
        }
        Some(Cmd::Sessions) => {
            let store = SessionStore::open(&home)?;
            let sessions = store.list_sessions(20)?;
            if sessions.is_empty() {
                println!("no sessions yet — chat or run to create one");
            }
            for (id, profile, created, count) in sessions {
                println!("[{profile}] {id} {created} ({count} msgs)");
            }
        }
        Some(Cmd::SessionShow { id }) => {
            let store = SessionStore::open(&home)?;
            if !store.has_session(&id)? {
                println!("unknown session: {id} (see `sessions`)");
                return Ok(());
            }
            let summary = store.get_summary(&id).unwrap_or_default();
            if !summary.is_empty() {
                println!("== summary ==\n{summary}");
            }
            for (id, role, content, created) in store.session_messages(&id, 200)? {
                println!(
                    "== {role} @ {created} [{}] ==\n{content}",
                    &id[..8.min(id.len())]
                );
            }
        }
        Some(Cmd::McpList { command, args, url }) => {
            let mut cfg = rupi_mcp::McpServerConfig::new("probe", &command, args);
            cfg.url = url;
            let bridge = if cfg.url.is_some() {
                rupi_mcp::McpBridge::spawn_http(cfg).await?
            } else {
                rupi_mcp::McpBridge::spawn(cfg).await?
            };
            println!("== tools ==");
            for t in bridge.list_tools().await? {
                let d = rupi_mcp::mcp_tool_to_definition("mcp", &t);
                println!("{} — {}", d.name, d.description);
            }
            // 资源/模板是可选能力：server 不支持（MethodNotFound）只记 stderr，不炸整单
            println!("== resources ==");
            match bridge.list_resources().await {
                Ok(rs) => {
                    for r in rs {
                        println!("{} — {}", r.uri, r.name);
                    }
                }
                Err(e) => eprintln!("[mcp-list] resources unsupported: {e:#}"),
            }
            println!("== prompts ==");
            match bridge.list_prompts().await {
                Ok(ps) => {
                    for p in ps {
                        println!("{} — {}", p.name, p.description.as_deref().unwrap_or(""));
                    }
                }
                Err(e) => eprintln!("[mcp-list] prompts unsupported: {e:#}"),
            }
        }
        Some(Cmd::Run { ref prompt, json }) => {
            run_once(&cli, &home, &prompt.join(" "), json).await?;
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

/// 终端审批器：Ask 裁决时 stdin 问一句 `[y/a(ll session)/N]`，默认拒绝。
/// 选 a 的（工具 + 规则原因）本会话内不再打扰。
struct TerminalApprover {
    cache: rupi_agent::SessionApprovalCache,
}

impl Default for TerminalApprover {
    fn default() -> Self {
        Self {
            cache: rupi_agent::SessionApprovalCache::default(),
        }
    }
}

impl rupi_agent::Approver for TerminalApprover {
    fn approve(&self, tool: &str, args: &serde_json::Value, reason: &str) -> bool {
        self.cache.decide_with(tool, reason, || {
            eprintln!("[approve] {tool} {args} — {reason} [y(es once)/a(ll session)/N]");
            let mut line = String::new();
            if std::io::stdin().read_line(&mut line).is_err() {
                return rupi_agent::ApprovalAnswer::Deny;
            }
            rupi_agent::ApprovalAnswer::parse(&line)
        })
    }
}

/// 非交互审批器：Ask 按构造值一锤定音（--approve 全放行；run 默认不挂审批器=拒绝）。
struct AutoApprover(bool);

impl rupi_agent::Approver for AutoApprover {
    fn approve(&self, _tool: &str, _args: &serde_json::Value, _reason: &str) -> bool {
        self.0
    }
}

/// 思考强度解析：未传 flag 即 None（不干预）；非法值直接 bail 并列合法档。
fn thinking_for(cli: &Cli) -> anyhow::Result<Option<rupi_llm::ThinkingLevel>> {
    if let Some(s) = cli.thinking.as_deref() {
        return s
            .parse()
            .map(Some)
            .map_err(|e| anyhow::anyhow!("--thinking 解析失败: {e:#}"));
    }
    Ok(rupi_llm::parse_model_spec(&cli.model).thinking)
}

/// 三档审批装配：--approve 全放行 / --no-approve 全拒绝（互斥，错配直接 bail）
/// / 默认走各端交互审批器（REPL 问询 / TUI 弹窗 / run 无审批=拒绝）。
fn approver_for(
    cli: &Cli,
    fallback: Option<std::sync::Arc<dyn rupi_agent::Approver>>,
) -> anyhow::Result<Option<std::sync::Arc<dyn rupi_agent::Approver>>> {
    if cli.approve && cli.no_approve {
        anyhow::bail!("--approve 与 --no-approve 互斥");
    }
    if cli.approve {
        eprintln!("[approve] --approve: Ask 裁决自动放行");
        return Ok(Some(std::sync::Arc::new(AutoApprover(true))));
    }
    if cli.no_approve {
        return Ok(None);
    }
    Ok(fallback)
}

/// 默认规则：危险 bash 子串转人工（REPL 有审批器；非交互/TUI 无审批则拒绝）。
fn default_policy() -> rupi_agent::RulePolicy {
    rupi_agent::RulePolicy {
        bash_block: vec!["rm -rf /".into(), "mkfs".into(), "dd if=".into()],
        ..Default::default()
    }
}

/// 按模型压实覆盖：`RUPI_COMPRESSION_OVERRIDES` JSON
///（如 `{"anthropic/claude-sonnet-4-5": {"threshold_chars": 30000, "keep_last": 10}}`，
/// 对标上游 `compaction.modelOverrides`）；未设置/非法时回空表走全局阈值。
fn load_compression_overrides() -> std::collections::HashMap<String, rupi_agent::CompressionOverride>
{
    match std::env::var("RUPI_COMPRESSION_OVERRIDES") {
        Ok(s) => rupi_agent::parse_compression_overrides(&s),
        Err(_) => Default::default(),
    }
}

/// 可选的外部记忆 provider（当前支持 jsonl）。
async fn maybe_external_memory(
    cli: &Cli,
    home: &PathBuf,
    mem: &mut MemoryManager,
) -> anyhow::Result<()> {
    if cli.memory_provider.as_deref() == Some("jsonl") {
        let mut p = rupi_memory::JsonlProvider::new(10);
        p.initialize(home).await?;
        mem.register_external("jsonl".into(), Box::new(p))?;
        eprintln!("[memory] external provider: jsonl (+recall tool)");
    }
    Ok(())
}

/// 恢复历史会话：按库行顺序回填全部节点（user、含工具调用的 assistant、工具结果）——
/// 有 `blocks` 的行按结构回填，老库纯文本行退回文本，模型续聊时看到完整工具上下文。
fn restore_or_new(cli: &Cli, sess_db: &SessionStore) -> anyhow::Result<(SessionTree, String)> {
    if let Some(id) = &cli.resume {
        let msgs = sess_db.session_records(id, 500)?;
        if msgs.is_empty() {
            // 存在但零消息（建完即退）与完全未知要区分：前者续进同 id 空树，后者 bail
            if sess_db.has_session(id)? {
                eprintln!(
                    "[resume {}] session exists but empty, starting fresh under same id",
                    id
                );
                rupi_tools::export_session_id(id);
                return Ok((SessionTree::new(), id.clone()));
            }
            anyhow::bail!("unknown session: {id} (see `rupi sessions`)");
        }
        let mut s = SessionTree::new();
        for rec in msgs {
            // 沿用库行 id：跨进程短 id 稳定，/tree 所见即 /goto 可达
            s.push_with_id(rec.id.clone(), rec.to_message());
        }
        eprintln!("[resume {}] restored {} msgs", id, s.history().len());
        rupi_tools::export_session_id(id);
        // 压缩摘要预热：prompt 窗口直接带上旧摘要 + 近期，避免超长恢复历史全文送模型。
        // through 取首条，保证 guard 把它视为“已压缩过”，新增不足一窗时跳过重复压缩。
        let stored = sess_db.get_summary(id).unwrap_or_default();
        if !stored.is_empty() {
            if let Some(first) = s.current_path.first().cloned() {
                s.summary = Some(stored);
                s.summary_through = Some(first);
            }
        }
        Ok((s, id.clone()))
    } else {
        let sid = sess_db.create_session("default")?;
        eprintln!("[session {sid}] turns persist to sessions.db");
        rupi_tools::export_session_id(&sid);
        Ok((SessionTree::new(), sid))
    }
}

/// 回合落盘：本轮新增的全部节点（user、含工具调用的 assistant、工具结果、最终答复）
/// 逐条落 sessions.db —— `content` 存纯文本供 FTS/展示，`blocks` 存完整消息 JSON 供
/// `--resume` 结构化回填（此前只存 user 原文 + 最后一条助手文本，恢复后丢全部工具上下文）。
/// 行 id 沿用树节点 id，resume 后短 id 跨进程稳定，`/goto` 可用。失败只 warning，不断聊天。
fn persist_turn(store: &SessionStore, sid: &str, session: &SessionTree, before_len: usize) {
    for id in session.current_path.iter().skip(before_len) {
        let Some(node) = session.nodes.get(id) else {
            continue;
        };
        let role = role_label(&node.message.role);
        let blocks = serde_json::to_string(&node.message).ok();
        if let Err(e) =
            store.add_message_full(id, sid, role, &node.message.full_text(), blocks.as_deref())
        {
            tracing::warn!("persist {role} msg failed: {e:#}");
        }
    }
}

fn role_label(role: &rupi_core::Role) -> &'static str {
    match role {
        rupi_core::Role::System => "system",
        rupi_core::Role::User => "user",
        rupi_core::Role::Assistant => "assistant",
        rupi_core::Role::Tool => "tool",
    }
}

/// 非交互执行一次（对标 pi -p）：跑完即退出，回合落盘进会话库。
/// 无问询：Ask 无审批器即拒绝（除非 --approve）；项目资源默认跳过（除非 --trust-project）。
/// stdout 只走模型正文（可管道），诊断走 stderr。
async fn run_once(cli: &Cli, home: &PathBuf, prompt: &str, json: bool) -> anyhow::Result<()> {
    // 审批档位 + thinking 档位先验（错配直接 bail，不建会话不落盘）
    let approver = approver_for(cli, None)?;
    let thinking = thinking_for(cli)?;
    let mut tools = sandboxed_tools();
    let _mcp = if let Some(path) = &cli.mcp_config {
        let configs = rupi_mcp::load_configs(path)?;
        let manager = rupi_mcp::McpManager::spawn_all(&configs).await?;
        let names = manager.register_all(&mut tools).await;
        eprintln!("[mcp] {} tools: {}", names.len(), names.join(", "));
        Some(manager)
    } else {
        None
    };
    let ext_set = load_extensions(&mut tools, &ext_dir(home, cli));
    let ext_arcs = ext_set.extension_arcs();
    // 非交互不提问：有项目资源且未 --trust-project 则跳过并告知
    let load_project = match project_resources() {
        None => true,
        Some((root, _)) if cli.trust_project => {
            eprintln!(
                "[trust] --trust-project: 本次加载项目资源 {}",
                root.display()
            );
            true
        }
        Some(_) => {
            eprintln!("[trust] 非交互默认跳过项目资源（加 --trust-project 加载）");
            false
        }
    };
    let mut store = memory_store(home, load_project);
    if cli.no_memory {
        store.memory_enabled = false;
        store.user_profile_enabled = false;
    }
    let frozen = store.frozen_snapshot();
    let mut mem_mgr = MemoryManager::new(store);
    maybe_external_memory(cli, home, &mut mem_mgr).await?;
    let mem = Arc::new(mem_mgr);
    let skills = Arc::new(SkillRegistry::discover(&skill_dirs(home, load_project)));
    let sess_db = SessionStore::open(home)?;
    let (mut session, sid) = restore_or_new(cli, &sess_db)?;
    // provider 在会话 id 落定后构造：亲和头荷载即 sessions.db 会话 id，
    // --resume 同 id 即同一下游（实例级随机 id 只保同进程粘滞）。
    let provider: Arc<dyn LlmProvider> = build_provider(&cli.model, Some(&sid), &provider_options(cli)).await?.into();
    let mut agent = AgentLoop::new(cli.max_turns)
        .with_compression(cli.compress_threshold, cli.compress_keep)
        .with_compression_overrides(load_compression_overrides());
    // 项目上下文（AGENTS.md 系）守信任门：非信任只读全局 home，不读 cwd 链（防项目指令注入）。
    agent = agent.with_context_dirs(context_cwd(load_project, home), home.clone());
    agent = agent
        .with_policy(Arc::new(default_policy()))
        .with_plan_mode(cli.plan);
    if let Some(a) = approver {
        agent = agent.with_approver(a);
    }
    if cli.parallel_tools {
        agent = agent.with_tool_execution(rupi_agent::ToolExecution::Parallel);
    }
    if cli.discover_tools {
        agent = agent.with_discovery(rupi_agent::DiscoveryConfig::default());
    }
    if let Some(t) = thinking {
        eprintln!("[thinking] level: {t:?}");
        agent = agent.with_thinking(t);
    }
    if cli.subagents {
        let sub = SubagentTool::new(
            provider.clone(),
            Arc::new(tools.clone()),
            mem.clone(),
            frozen.clone(),
            skills.clone(),
            cli.max_turns,
        )
        .with_plan_mode(cli.plan)
        .with_thinking(agent.thinking);
        tools.register(Arc::new(sub));
        eprintln!("[subagents] subagent tool enabled");
    }
    // 后台 review（与 chat 同语义）：默认启发式复盘，非空建议打印，--review-apply 直接落盘
    let pending: Arc<std::sync::Mutex<Vec<ReviewSuggestion>>> =
        Arc::new(std::sync::Mutex::new(vec![]));
    if cli.review_enabled() {
        let pending_clone = pending.clone();
        let reviewer: Arc<dyn rupi_agent::Reviewer> = if cli.review_llm {
            Arc::new(rupi_agent::LlmReviewer::new(provider.clone()))
        } else {
            Arc::new(HeuristicReviewer::default())
        };
        agent = agent.with_reviewer(
            reviewer,
            Arc::new(move |s: ReviewSuggestion| {
                eprintln!(
                    "[review] memory_ops={} failures={} skill={}",
                    s.memory_ops.len(),
                    s.failures.len(),
                    s.skill_draft
                        .as_ref()
                        .map(|d| d.name.as_str())
                        .unwrap_or("-")
                );
                pending_clone.lock().unwrap().push(s);
            }),
        );
    }
    let before_len = session.current_path.len();
    // @path 引用展开（与 REPL 同语义，root 取 current_dir）：-p 也可内联文件。
    let user = user_turn(prompt);
    use std::io::Write as _;
    // --json：stdout 只走 JSONL 事件（AgentEvent 的 serde 形状，`type` 区分），人读诊断仍走 stderr
    let emit_json = |e: &rupi_core::AgentEvent| {
        if let Ok(line) = serde_json::to_string(e) {
            println!("{line}");
            let _ = std::io::stdout().flush();
        }
    };
    let res = agent
        .run_with_user(
            &*provider,
            &mut session,
            user,
            &tools,
            &*mem,
            &frozen,
            &*skills,
            &ext_arcs,
            &|e| {
                if json {
                    emit_json(&e);
                    return;
                }
                match e {
                    rupi_core::AgentEvent::TextDelta { delta } => {
                        print!("{delta}");
                        let _ = std::io::stdout().flush();
                    }
                    rupi_core::AgentEvent::ToolStart { name, .. } => {
                        eprintln!("\n[tool {name}]…")
                    }
                    rupi_core::AgentEvent::ToolEnd { name, is_error, .. } => {
                        eprintln!("\n[{name} {}]", if is_error { "error" } else { "ok" })
                    }
                    rupi_core::AgentEvent::MemoryRecall { detail } => {
                        eprintln!("{detail}")
                    }
                    rupi_core::AgentEvent::CompactionStart => {
                        eprintln!("\n[compacting]…")
                    }
                    rupi_core::AgentEvent::CompactionEnd { summarized, kept } => {
                        eprintln!("\n[compacted: summarized {summarized}, kept {kept}]")
                    }
                    rupi_core::AgentEvent::Usage {
                        input_tokens,
                        output_tokens,
                    } => {
                        eprintln!("\n[usage in={input_tokens} out={output_tokens}]")
                    }
                    rupi_core::AgentEvent::UiHint {
                        source,
                        kind,
                        message,
                    } => {
                        eprintln!("\n[ui {source}/{kind}] {message}")
                    }
                    _ => {}
                }
            },
            // 非交互 run：Ctrl-C 直接杀进程（现状），不做优雅中止
            &rupi_core::CancelFlag::new(),
        )
        .await;
    emit_ext_hints(&ext_set);
    let stop = match res {
        Ok(s) => s,
        Err(e) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({"type": "error", "message": format!("{e:#}")})
                );
            }
            return Err(e);
        }
    };
    if json {
        let text = session
            .current_path
            .iter()
            .skip(before_len)
            .filter_map(|id| session.nodes.get(id))
            .filter(|n| n.message.role == rupi_core::Role::Assistant)
            .last()
            .map(|n| n.message.full_text())
            .unwrap_or_default();
        println!(
            "{}",
            serde_json::json!({
                "type": "run_result",
                "session_id": sid,
                "stop_reason": stop,
                "text": text,
            })
        );
    } else {
        println!();
    }
    persist_turn(&sess_db, &sid, &session, before_len);
    if cli.review_apply {
        apply_suggestions(home, &pending);
    }
    Ok(())
}

async fn run_chat(cli: &Cli, home: &PathBuf) -> anyhow::Result<()> {
    let mut model = cli.model.clone();
    let pending: Arc<std::sync::Mutex<Vec<ReviewSuggestion>>> =
        Arc::new(std::sync::Mutex::new(vec![]));
    let mut agent = AgentLoop::new(cli.max_turns)
        .with_compression(cli.compress_threshold, cli.compress_keep)
        .with_compression_overrides(load_compression_overrides());
    agent = agent
        .with_policy(Arc::new(default_policy()))
        .with_plan_mode(cli.plan);
    if let Some(a) = approver_for(cli, Some(Arc::new(TerminalApprover::default())))? {
        agent = agent.with_approver(a);
    }
    if cli.parallel_tools {
        agent = agent.with_tool_execution(rupi_agent::ToolExecution::Parallel);
        println!("[parallel tools] tool calls in one turn run concurrently");
    }
    if cli.discover_tools {
        agent = agent.with_discovery(rupi_agent::DiscoveryConfig::default());
        println!(
            "[discover tools] only resident tool schemas injected; search_tools to reveal more"
        );
    }
    if cli.plan {
        println!("[plan mode] read-only: write/edit/bash disabled");
    }
    if let Some(t) = thinking_for(cli)? {
        println!("[thinking] level: {t:?}");
        agent = agent.with_thinking(t);
    }
    let mut tools = sandboxed_tools();
    // MCP-Direct：spawn 各 server 并把远端工具注册为原生工具（失败只 warning，不断主循环）；
    // 带变更观察启动：server 发 notifications/tools/list_changed 即进队，逐轮差量刷新
    let (mcp_tx, mut mcp_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let mcp = if let Some(path) = &cli.mcp_config {
        let configs = rupi_mcp::load_configs(path)?;
        let manager = rupi_mcp::McpManager::spawn_all_watched(&configs, mcp_tx).await?;
        let names = manager.register_all(&mut tools).await;
        println!("[mcp] {} tools: {}", names.len(), names.join(", "));
        Some(manager)
    } else {
        None
    };
    // 外部扩展：启动加载 + REPL 每轮自动热重载（/reload 手动触发）
    let ext_path = ext_dir(home, cli);
    let mut ext_set = load_extensions(&mut tools, &ext_path);
    // 项目信任门：未信任则项目记忆/skills/命令全部不加载（只用全局）
    let load_project = load_project_resources(home, cli);
    // 项目上下文（AGENTS.md 系）同样守信任门。
    agent = agent.with_context_dirs(context_cwd(load_project, home), home.clone());
    let mut store = memory_store(home, load_project);
    if cli.no_memory {
        store.memory_enabled = false;
        store.user_profile_enabled = false;
    }
    let frozen = store.frozen_snapshot();
    let mut mem_mgr = MemoryManager::new(store);
    maybe_external_memory(cli, home, &mut mem_mgr).await?;
    let mem = Arc::new(mem_mgr);
    let skills = Arc::new(SkillRegistry::discover(&skill_dirs(home, load_project)));
    let sess_db = SessionStore::open(home)?;
    let (mut session, sid) = restore_or_new(cli, &sess_db)?;
    // provider 与 reviewer 在会话 id 落定后装配：亲和头荷载即 sessions.db 会话 id
    let mut provider: Arc<dyn LlmProvider> = build_provider(&model, Some(&sid), &provider_options(cli)).await?.into();
    if cli.review_enabled() {
        let pending_clone = pending.clone();
        // --review-llm 用模型复盘（烧 token 但提炼质量更高），默认离线启发式
        let reviewer: Arc<dyn rupi_agent::Reviewer> = if cli.review_llm {
            Arc::new(rupi_agent::LlmReviewer::new(provider.clone()))
        } else {
            Arc::new(HeuristicReviewer::default())
        };
        agent = agent.with_reviewer(
            reviewer,
            Arc::new(move |s: ReviewSuggestion| {
                println!(
                    "\n[review] memory_ops={} failures={} skill={}",
                    s.memory_ops.len(),
                    s.failures.len(),
                    s.skill_draft
                        .as_ref()
                        .map(|d| d.name.as_str())
                        .unwrap_or("-")
                );
                for m in &s.memory_ops {
                    println!("[review] memory add: {}", m.entry);
                }
                for f in &s.failures {
                    println!("[review] failure: {f}");
                }
                if let Some(d) = &s.skill_draft {
                    println!("[review] skill draft: {} — {}", d.name, d.description);
                }
                pending_clone.lock().unwrap().push(s);
            }),
        );
    }
    if cli.subagents {
        let sub = SubagentTool::new(
            provider.clone(),
            Arc::new(tools.clone()),
            mem.clone(),
            frozen.clone(),
            skills.clone(),
            cli.max_turns,
        )
        .with_plan_mode(cli.plan)
        .with_thinking(agent.thinking);
        tools.register(Arc::new(sub));
        println!("[subagents] subagent tool enabled");
    }

    println!("rupi v0.1.0 — 输入 /quit 退出，Ctrl-C 中止本轮，/rewind 回退，/tree 看树，/goto <短id> 跳转，/compact 手动压实，/model [provider/model[:thinking]] 切换模型，/thinking [off|low|medium|high|xhigh|max] 思考强度，/reload 重载扩展，/plan 切换计划模式，/skills 看技能，/commands 看自定义命令");
    let stdin = std::io::stdin();
    let mut saved_summary = session.summary.clone().unwrap_or_default();
    let mut line = String::new();
    loop {
        line.clear();
        print!("> ");
        use std::io::Write as _;
        std::io::stdout().flush()?;
        if stdin.read_line(&mut line)? == 0 {
            break;
        }
        let mut input = line.trim().to_string();
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
        if input == "/commands" {
            println!(
                "{}",
                rupi_core::commands::index_block(&command_dirs_filtered(home, load_project))
            );
            let extra = ext_set.command_index();
            if !extra.is_empty() {
                println!("{extra}");
            }
            continue;
        }
        if input == "/reload" {
            let lines = rupi_ext::refresh_extensions(&mut tools, &mut ext_set);
            if lines.is_empty() {
                println!("[ext] no changes");
            } else {
                for l in lines {
                    println!("{l}");
                }
            }
            continue;
        }
        if input == "/plan" {
            agent.plan_mode = !agent.plan_mode;
            println!("[plan mode {}]", if agent.plan_mode { "on" } else { "off" });
            continue;
        }
        if input == "/model" || input.starts_with("/model ") {
            let arg = input.strip_prefix("/model").unwrap().trim();
            if arg.is_empty() {
                println!("[model {model}]");
            } else {
                match build_provider(arg, Some(&sid), &provider_options(cli)).await {
                    Ok(p) => {
                        provider = p.into();
                        model = arg.to_string();
                        if let Some(t) = rupi_llm::parse_model_spec(arg).thinking {
                            agent.thinking = Some(t);
                        }
                        println!("[model switched to {model}]");
                    }
                    Err(e) => eprintln!("[model] switch failed ({e:#}); staying on {model}"),
                }
            }
            continue;
        }
        // 思考强度会话内切换（对标上游 /thinking；只影响后续回合）
        if input == "/thinking" || input.starts_with("/thinking ") {
            let arg = input.strip_prefix("/thinking").unwrap().trim();
            if arg.is_empty() {
                match agent.thinking {
                    Some(t) => println!("[thinking {t:?}]"),
                    None => println!("[thinking default (provider default)]"),
                }
            } else {
                match arg.parse::<rupi_llm::ThinkingLevel>() {
                    Ok(t) => {
                        agent.thinking = Some(t);
                        println!("[thinking switched to {t:?}]");
                    }
                    Err(e) => eprintln!("[thinking] {e:#}; staying on current"),
                }
            }
            continue;
        }
        // `/rewind [短id]`：无参回退一步，有参回退到指定节点（与 /goto 同短 id 解析）。
        if input == "/rewind" || input.starts_with("/rewind ") {
            let arg = input.strip_prefix("/rewind").unwrap().trim();
            if arg.is_empty() {
                if session.current_path.len() >= 2 {
                    let target = session.current_path[session.current_path.len() - 2].clone();
                    session.rewind_to(&target);
                    println!("[rewound]");
                } else {
                    // 空历史给反馈（与 TUI 同文案）；此前静默 continue，用户以为卡死。
                    println!("[rewind] nothing to undo");
                }
            } else {
                match session.resolve_short_id(arg) {
                    Some(id) if session.rewind_to(&id) => {
                        println!("[rewound {}]", &id[..8.min(id.len())])
                    }
                    // 解析到但不在当前路径（废弃分支节点）：rewind 只回退游标，跨分支用 /goto。
                    Some(_) => println!("[rewind] node not on current path, use /goto"),
                    None => println!("[rewind] unknown or ambiguous node prefix: {arg}"),
                }
            }
            continue;
        }
        if input == "/tree" {
            print!("{}", session.tree_view());
            continue;
        }
        // 手动压实（对标上游 /compact）：阈值外的主动压缩，短历史给反馈不烧模型；
        // `/compact <prompt>` 追加自定义摘要指令（Additional focus）。
        if input == "/compact" || input.starts_with("/compact ") {
            let prompt = input
                .strip_prefix("/compact")
                .map(str::trim)
                .filter(|p| !p.is_empty());
            let before = session.summary.clone();
            agent
                .force_compress_with_prompt(&*provider, &mut session, &mem, &|e| match e {
                    rupi_core::AgentEvent::CompactionStart => eprintln!("[compacting]…"),
                    rupi_core::AgentEvent::CompactionEnd { summarized, kept } => {
                        eprintln!("[compacted: summarized {summarized}, kept {kept}]")
                    }
                    _ => {}
                }, prompt)
                .await;
            if session.summary != before && session.summary.is_some() {
                println!("[compacted]");
            } else {
                println!("[compact] nothing to compress");
            }
            continue;
        }
        // 裸 `/goto`（无参数）必须拦截给用法提示：此前漏进自定义命令查找，
        // 查不到就当普通消息发给模型，白烧一轮（TUI 同语义，见 dispatch_builtin）。
        if input == "/goto" {
            println!("[goto] usage: /goto <短id>（/tree 查看节点）");
            continue;
        }
        if let Some(prefix) = input.strip_prefix("/goto ") {
            let prefix = prefix.trim();
            match session.resolve_short_id(prefix) {
                Some(id) if session.goto_node(&id) => {
                    println!("[goto {}]", &id[..8.min(id.len())]);
                }
                _ => println!("[goto] unknown or ambiguous node prefix: {prefix}"),
            }
            continue;
        }
        // 每轮自动热检查：扩展目录有变即重载，无变零开销（一次 mtime 扫描）；
        // skill 注册表同轮刷新：上一轮蒸馏的新 skill 本轮即对模型可见（自积累闭环）
        for l in rupi_ext::refresh_extensions(&mut tools, &mut ext_set) {
            eprintln!("{l}");
        }
        // MCP 工具热刷新：server 发 notifications/tools/list_changed 即重列该 server 差量更新
        if let Some(m) = &mcp {
            while let Ok(srv) = mcp_rx.try_recv() {
                match m.refresh_server(&mut tools, &srv).await {
                    Ok(added) if !added.is_empty() => {
                        println!("[mcp] {srv} tools added: {}", added.join(", "))
                    }
                    Ok(_) => println!("[mcp] {srv} tools updated"),
                    Err(e) => eprintln!("[mcp] refresh {srv} failed: {e:#}"),
                }
            }
        }
        skills.refresh(&skill_dirs(home, load_project));
        // 自定义斜杠命令：内建优先（上已 continue），命中则展开为提示词；
        // 未命中再回退 skill 名（`/skillname args` 即调 skill）。
        let slash = rupi_core::commands::split(&input).map(|(n, a)| (n.to_owned(), a.to_owned()));
        if let Some((name, args)) = slash.as_ref() {
            if let Some(expanded) =
                rupi_core::commands::expand(&command_dirs_filtered(home, load_project), name, args)
            {
                println!("[command /{name}]");
                input = expanded;
            } else if let Some(expanded) = skills.expand_as_command(name, args) {
                println!("[skill /{name}]");
                input = expanded;
            } else if let Some(expanded) = ext_set.expand_command(name, args) {
                println!("[ext /{name}]");
                input = expanded;
            }
        }
        let user = user_turn(&input);
        let before_len = session.current_path.len();
        // 协作取消：Ctrl-C 只在 run 期间捕获（select 存活时），置位后循环在检查点
        // 优雅中止；空闲输入时无监听器，按默认行为杀进程（与现状一致）。
        let cancel = rupi_core::CancelFlag::new();
        // Box 拥有式持有：取消后仍需 await 到底，结束后显式 drop 释放 &mut session 借用。
        let ext_arcs = ext_set.extension_arcs();
        let mut fut = Box::pin(agent.run_with_user(
            &*provider,
            &mut session,
            user,
            &tools,
            &*mem,
            &frozen,
            &*skills,
            &ext_arcs,
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
                rupi_core::AgentEvent::MemoryRecall { detail } => {
                    println!("{detail}")
                }
                rupi_core::AgentEvent::CompactionStart => {
                    println!("\n[compacting]…")
                }
                rupi_core::AgentEvent::CompactionEnd { summarized, kept } => {
                    println!("\n[compacted: summarized {summarized}, kept {kept}]")
                }
                rupi_core::AgentEvent::Usage {
                    input_tokens,
                    output_tokens,
                } => {
                    println!("\n[usage in={input_tokens} out={output_tokens}]")
                }
                rupi_core::AgentEvent::UiHint {
                    source,
                    kind,
                    message,
                } => {
                    println!("\n[ui {source}/{kind}] {message}")
                }
                rupi_core::AgentEvent::RunEnd {
                    stop_reason: rupi_core::StopReason::Aborted,
                } => {
                    println!("\n[aborted]")
                }
                _ => {}
            },
            &cancel,
        ));
        let res = tokio::select! {
            r = &mut fut => r,
            _ = tokio::signal::ctrl_c() => {
                cancel.cancel();
                eprintln!("\n[abort requested — finishing current step…]");
                (&mut fut).await
            }
        };
        drop(fut); // future（含 &mut session 借用）在此释放，后续落盘再借
        // provider/网络错误不再终结 REPL（此前 `res?` 直接退出进程）：回滚本轮已入树的
        // 节点（user 及可能的半轮工具态），打印原因，回到提示符让用户重试或 /model 切换。
        if let Err(e) = res {
            for id in session.current_path.split_off(before_len) {
                session.nodes.remove(&id);
            }
            eprintln!("\n[error] {e:#}");
            println!();
            continue;
        }
        emit_ext_hints(&ext_set);
        persist_turn(&sess_db, &sid, &session, before_len);
        // 压缩摘要落盘（变化才写）
        if let Some(sum) = &session.summary {
            if *sum != saved_summary {
                if let Err(e) = sess_db.set_summary(&sid, sum) {
                    tracing::warn!("persist summary failed: {e:#}");
                } else {
                    saved_summary = sum.clone();
                }
            }
        }
        if cli.review_apply {
            apply_suggestions(home, &pending);
        }
        println!();
    }
    Ok(())
}

async fn run_tui(cli: &Cli, home: &PathBuf) -> anyhow::Result<()> {
    // thinking 档位先验：非法值在建会话前 bail，不污染会话库
    let thinking = thinking_for(cli)?;
    let mut tools = sandboxed_tools();
    let (mcp_tx, mcp_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let mcp = if let Some(path) = &cli.mcp_config {
        let configs = rupi_mcp::load_configs(path)?;
        let manager = rupi_mcp::McpManager::spawn_all_watched(&configs, mcp_tx).await?;
        let names = manager.register_all(&mut tools).await;
        eprintln!("[mcp] {} tools: {}", names.len(), names.join(", "));
        Some(manager)
    } else {
        None
    };
    let mut ext_set = load_extensions(&mut tools, &ext_dir(home, cli));
    // 项目信任门（全屏启动前 stdin 问一次，与 REPL 同语义）
    let load_project = load_project_resources(home, cli);
    let mut store = memory_store(home, load_project);
    if cli.no_memory {
        store.memory_enabled = false;
        store.user_profile_enabled = false;
    }
    let frozen = store.frozen_snapshot();
    let mut mem_mgr = MemoryManager::new(store);
    maybe_external_memory(cli, home, &mut mem_mgr).await?;
    let mem = Arc::new(mem_mgr);
    let skills = Arc::new(SkillRegistry::discover(&skill_dirs(home, load_project)));
    let sess_db = Arc::new(std::sync::Mutex::new(SessionStore::open(home)?));
    let (mut session, sid) = {
        let db = sess_db.lock().unwrap();
        restore_or_new(cli, &db)?
    };
    // provider 在会话 id 落定后构造（与 run/chat 同序，亲和头荷载即本会话 id）
    let mut provider: Arc<dyn LlmProvider> = build_provider(&cli.model, Some(&sid), &provider_options(cli)).await?.into();
    let mut agent = AgentLoop::new(cli.max_turns)
        .with_compression(cli.compress_threshold, cli.compress_keep)
        .with_compression_overrides(load_compression_overrides());
    // TUI 内审批：Ask 时暂停全屏问一句 [y/N]（与 REPL 同语义）；plan mode 同 REPL
    // 项目上下文守信任门（与 run/chat 同 helper）。
    agent = agent.with_context_dirs(context_cwd(load_project, home), home.clone());
    agent = agent
        .with_policy(Arc::new(default_policy()))
        .with_plan_mode(cli.plan);
    if let Some(a) = approver_for(cli, Some(Arc::new(rupi_tui::TuiApprover::default())))? {
        agent = agent.with_approver(a);
    }
    if cli.parallel_tools {
        agent = agent.with_tool_execution(rupi_agent::ToolExecution::Parallel);
        eprintln!("[parallel tools] tool calls in one turn run concurrently");
    }
    if cli.discover_tools {
        agent = agent.with_discovery(rupi_agent::DiscoveryConfig::default());
        eprintln!(
            "[discover tools] only resident tool schemas injected; search_tools to reveal more"
        );
    }
    if let Some(t) = thinking {
        eprintln!("[thinking] level: {t:?}");
        agent = agent.with_thinking(t);
    }
    if cli.subagents {
        let sub = SubagentTool::new(
            provider.clone(),
            Arc::new(tools.clone()),
            mem.clone(),
            frozen.clone(),
            skills.clone(),
            cli.max_turns,
        )
        .with_plan_mode(cli.plan)
        .with_thinking(agent.thinking);
        tools.register(Arc::new(sub));
        eprintln!("[subagents] subagent tool enabled");
    }
    let review_lines: Option<Arc<std::sync::Mutex<Vec<String>>>> = if cli.review_enabled() {
        Some(Arc::new(std::sync::Mutex::new(vec![])))
    } else {
        None
    };
    if let Some(buf) = review_lines.clone() {
        let home_clone = home.clone();
        let apply = cli.review_apply;
        let reviewer: Arc<dyn rupi_agent::Reviewer> = if cli.review_llm {
            Arc::new(rupi_agent::LlmReviewer::new(provider.clone()))
        } else {
            Arc::new(HeuristicReviewer::default())
        };
        agent = agent.with_reviewer(
            reviewer,
            Arc::new(move |s: ReviewSuggestion| {
                let mut lines = buf.lock().unwrap();
                for m in &s.memory_ops {
                    lines.push(format!("[review] memory add: {}", m.entry));
                }
                for f in &s.failures {
                    lines.push(format!("[review] failure: {f}"));
                }
                if let Some(d) = &s.skill_draft {
                    lines.push(format!(
                        "[review] skill draft: {} — {}",
                        d.name, d.description
                    ));
                }
                if apply {
                    let store = memory_store(&home_clone, true);
                    for m in &s.memory_ops {
                        match store.apply_write("add", &m.entry) {
                            Ok(_) => lines.push("[review] memory saved".into()),
                            Err(e) => lines.push(format!("[review] memory save failed: {e:#}")),
                        }
                    }
                    for f in &s.failures {
                        match store.record_failure(f) {
                            Ok(()) => lines.push("[review] failure saved".into()),
                            Err(e) => lines.push(format!("[review] failure save failed: {e:#}")),
                        }
                    }
                    if let Some(d) = &s.skill_draft {
                        let acc = SkillAccumulator::new(home_clone.join("skills"));
                        if acc.exists(&d.name) {
                            tracing::debug!("[review] skill {} already exists, skip", d.name);
                        } else {
                            match acc.propose(&d.name, &d.description, &d.steps) {
                                Ok(dir) => {
                                    lines.push(format!("[review] skill drafted at {}", dir.display()))
                                }
                                Err(e) => lines.push(format!("[review] skill draft skipped: {e:#}")),
                            }
                        }
                    }
                }
            }),
        );
    }
    let saved = Arc::new(std::sync::Mutex::new(
        session.summary.clone().unwrap_or_default(),
    ));
    // 当前会话 id 共享 cell：`/resume` 切换后落盘与后续亲和头同读此值，不再钉死启动 id。
    let sid_cell = Arc::new(std::sync::Mutex::new(sid.clone()));
    let sid_for_turn = sid_cell.clone();
    let sess_db_ctx = sess_db.clone();
    let ctx = rupi_tui::TuiContext {
        provider: &mut provider,
        agent: &mut agent,
        session: &mut session,
        tools: &mut tools,
        mem: &*mem,
        frozen: &frozen,
        skills: &*skills,
        mcp: mcp.as_ref(),
        mcp_rx: Some(mcp_rx),
        skill_dirs: skill_dirs(home, load_project),
        ext_set: Some(&mut ext_set),
        command_dirs: command_dirs_filtered(home, load_project),
        review_lines,
        session_id: sid_cell,
        sess_db: Some(sess_db_ctx),
        on_turn: Some(Arc::new(move |t: rupi_tui::TurnRecord| {
            let db = sess_db.lock().unwrap();
            let sid = sid_for_turn.lock().unwrap().clone();
            // 全部新增节点落盘（含工具调用/结果，blocks 存完整 JSON），与 REPL persist_turn 同语义
            for (id, msg) in &t.messages {
                let blocks = serde_json::to_string(msg).ok();
                if let Err(e) = db.add_message_full(
                    id,
                    &sid,
                    role_label(&msg.role),
                    &msg.full_text(),
                    blocks.as_deref(),
                ) {
                    tracing::warn!("persist {} msg failed: {e:#}", role_label(&msg.role));
                }
            }
            if let Some(sum) = &t.summary {
                if *sum != *saved.lock().unwrap() {
                    if let Err(e) = db.set_summary(&sid, sum) {
                        tracing::warn!("persist summary failed: {e:#}");
                    } else {
                        *saved.lock().unwrap() = sum.clone();
                    }
                }
            }
        })),
    };
    rupi_tui::launch(ctx).await
}
