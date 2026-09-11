//! rupi CLI：coding agent 交互入口 + MCP / 记忆 / Skill / 会话管理子命令。

mod rpc;

use clap::{Parser, Subcommand};
use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;
use rupi_agent::{AgentLoop, HeuristicReviewer, ReviewSuggestion, SubagentTool};
use rupi_core::SessionTree;
use rupi_llm::{LlmProvider, MockProvider};
use rupi_memory::{MemoryManager, MemoryProvider, MemoryStore, SessionStore};
use rupi_skills::{SkillAccumulator, SkillRegistry};
use rupi_tools::ToolRegistry;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
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
    /// 模型：`name` 或 `provider/model[:thinking]`（openai|anthropic|gemini|openrouter|azure|bedrock|vertex）。
    /// 未传时由 settings.json 覆盖，再默认 `gpt-4o-mini`。
    #[arg(long)]
    model: Option<String>,
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
    /// 压实 reserveTokens：为模型回复预留的 token（`used > window - reserve` 触发）
    #[arg(long, alias = "reserve-tokens")]
    compress_threshold: Option<usize>,
    /// 压实 keepRecentTokens：从尾部保留、不摘要的近期 token
    #[arg(long, alias = "keep-recent-tokens")]
    compress_keep: Option<usize>,
    /// 继续最近一次会话（对标 pi --continue / -c）
    #[arg(long = "continue", short = 'c')]
    continue_session: bool,
    /// 不落盘（临时会话，对标 pi --no-session）
    #[arg(long)]
    no_session: bool,
    /// 会话展示名（`/name`、JSONL session_info）
    #[arg(long, short = 'n')]
    name: Option<String>,
    /// 工具白名单（逗号分隔；覆盖 settings.tools，含 memory/skill/mcp）
    #[arg(long)]
    tools: Option<String>,
    /// 从结果集排除工具（逗号分隔）
    #[arg(long)]
    exclude_tools: Option<String>,
    /// 关闭全部工具（对标 pi --no-tools）
    #[arg(long)]
    no_tools: bool,
    /// 替换默认系统提示（也可用 ~/.rupi/SYSTEM.md）
    #[arg(long)]
    system_prompt: Option<String>,
    /// 追加到系统提示（也可用 APPEND_SYSTEM.md）
    #[arg(long)]
    append_system_prompt: Option<String>,
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
    /// 对标 pi `--mode rpc`：stdin JSONL 命令，stdout JSONL 响应与事件
    #[arg(long)]
    mode: Option<String>,
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
    /// 从 git:/npm:/本地路径安装 skill、command、extension（对标 `pi install`）
    Install {
        /// `git:host/user/repo[@ref]`、`npm:@scope/pkg[@ver]`、https/ssh URL、或本地路径
        spec: String,
        /// 写入项目 `.rupi/`（默认 `$RUPI_HOME` / `~/.rupi`）
        #[arg(short = 'l', long)]
        local: bool,
        /// 只装一类：skill / command / extension（默认按包内资源全装）
        #[arg(long)]
        kind: Option<String>,
    },
    /// 卸载 `install` 写入的包
    Uninstall {
        spec: String,
        #[arg(short = 'l', long)]
        local: bool,
    },
    /// 列出已安装包
    Packages {
        #[arg(short = 'l', long)]
        local: bool,
    },
}

fn home_dir() -> PathBuf {
    std::env::var("RUPI_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs_home().join(".rupi"))
}

fn pkg_opts(
    home: &PathBuf,
    local: bool,
    kind: Option<&str>,
) -> anyhow::Result<rupi_pkg::InstallOpts> {
    Ok(rupi_pkg::InstallOpts {
        home: home.clone(),
        cwd: std::env::current_dir().unwrap_or_else(|_| home.clone()),
        local,
        kind: rupi_pkg::ResourceKind::parse(kind)?,
    })
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
/// `.rupi/*` 仍按 cwd 相对路径；`.pi/skills` / `.agents/skills` 与发现侧同款上溯。
fn project_resources() -> Option<(PathBuf, Vec<String>)> {
    let cwd = std::env::current_dir().ok()?;
    let shared = rupi_skills::existing_project_skill_dirs(&cwd, &dirs_home());
    let root = match MemoryStore::discover_project(&cwd) {
        Some(r) => r,
        // 回退：无 .git 的普通目录自带 .rupi 或 Pi/Agent Skills 资源也视为项目
        None if PathBuf::from(".rupi/skills").exists()
            || PathBuf::from(".rupi/commands").exists()
            || PathBuf::from(".rupi")
                .join(rupi_memory::MEMORY_FILE)
                .exists()
            || !shared.is_empty() =>
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
    for p in shared {
        resources.push(p.display().to_string());
    }
    if resources.is_empty() {
        return None;
    }
    Some((root, resources))
}

/// 项目信任门（对标上游 project_trust）：项目根有本地资源且未被记住时问一次。
/// 返回是否加载项目资源。无项目根 / 无项目资源 / 已记住 → true 不打扰；
/// 非交互（管道/EOF）默认跳过并提示。
fn load_project_resources(home: &PathBuf, cli: &Cli, settings: &rupi_config::Settings) -> bool {
    let (root, resources) = match project_resources() {
        Some(r) => r,
        None => return true,
    };
    if cli.trust_project {
        println!(
            "[trust] --trust-project: 本次加载项目资源 {}",
            root.display()
        );
        return true;
    }
    match settings.project_trust() {
        rupi_config::ProjectTrust::Always => return true,
        rupi_config::ProjectTrust::Never => {
            println!("[trust] defaultProjectTrust=never：跳过项目资源");
            return false;
        }
        rupi_config::ProjectTrust::Ask => {}
    }
    let mut store = rupi_core::trust::TrustStore::open(home.join("trusted_projects"));
    if store.contains(&root) {
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
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    // 项目 skills（含 `.pi/skills` / `.agents/skills` 上溯）与项目记忆同门：
    // 信任被拒则不发现、不加载；全局 `~/.pi` / `~/.agents` 仍可见。
    rupi_skills::skill_search_dirs(builtin_skills_dir(), home, &dirs_home(), &cwd, load_project)
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
    rupi_core::commands::command_dirs_filtered(home, load_project)
}

/// 工作区沙箱根：启动时 cwd（canonicalize 消解符号链接），read/write/edit 约束其内。
fn sandbox_root() -> PathBuf {
    std::env::current_dir()
        .ok()
        .and_then(|p| p.canonicalize().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 沙箱工具表：文件工具约束在工作区内，相对路径按 root 解析（subagent 克隆继承）。
fn sandboxed_tools_filtered(builtin_allow: Option<&[String]>) -> ToolRegistry {
    let root = sandbox_root();
    // 诊断走 stderr：`run --json` 的 stdout 必须是纯 JSONL
    eprintln!("[sandbox workspace: {}]", root.display());
    let mut r = ToolRegistry::with_sandboxed_builtins(&root);
    if let Some(allow) = builtin_allow {
        r.retain(|n| allow.iter().any(|a| a == n));
    }
    r
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
    match cli
        .provider
        .as_deref()
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
    {
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
            let hint = spec.provider.as_deref().unwrap_or_else(|| {
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

#[tokio::main(flavor = "current_thread")]
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
    if cli.mode.as_deref() == Some("rpc") {
        run_rpc(&cli, &home).await?;
        return Ok(());
    }

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
            for (id, profile, created, count, name) in sessions {
                if name.is_empty() {
                    println!("[{profile}] {id} {created} ({count} msgs)");
                } else {
                    println!("[{profile}] {id} {created} ({count} msgs) {name}");
                }
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
        Some(Cmd::Install { spec, local, kind }) => {
            let opts = pkg_opts(&home, local, kind.as_deref())?;
            let report = rupi_pkg::install(&spec, &opts).await?;
            println!("{}", rupi_pkg::format_report(&report));
        }
        Some(Cmd::Uninstall { spec, local }) => {
            let opts = pkg_opts(&home, local, None)?;
            let rec = rupi_pkg::uninstall(&spec, &opts)?;
            println!("removed {} ({})", rec.name, rec.id);
        }
        Some(Cmd::Packages { local }) => {
            let opts = pkg_opts(&home, local, None)?;
            let rows = rupi_pkg::list_installed(&opts);
            if rows.is_empty() {
                println!(
                    "no packages. install with `rupi install git:host/user/repo` or `npm:@scope/pkg`"
                );
            }
            for p in rows {
                println!(
                    "{}  {}  skills=[{}] commands=[{}] ext=[{}]",
                    p.id,
                    p.spec,
                    p.skills.join(","),
                    p.commands.join(","),
                    p.extensions.join(",")
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

/// settings.json + flag 合并后的运行时配置。
struct Resolved {
    model: String,
    thinking: Option<rupi_llm::ThinkingLevel>,
    reserve_tokens: usize,
    keep_recent_tokens: usize,
    context_window: Option<usize>,
    compaction_enabled: bool,
    /// `Some` = `--tools`/`--no-tools` 全量白名单。
    tool_allow: Option<Vec<String>>,
    tool_exclude: Vec<String>,
    /// 仅过滤内建七件套（settings.tools，且未被 --tools 覆盖时）。
    builtin_allow: Option<Vec<String>>,
    system: rupi_config::SystemPromptFiles,
    #[allow(dead_code)]
    theme: String,
    overrides: std::collections::HashMap<String, rupi_agent::CompressionOverride>,
    persist: bool,
    settings: rupi_config::Settings,
}

impl Resolved {
    fn load(cli: &Cli, home: &PathBuf, load_project: bool) -> anyhow::Result<Self> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let settings = rupi_config::Settings::load(home, &cwd);
        let model = cli
            .model
            .clone()
            .or_else(|| settings.model.clone())
            .unwrap_or_else(|| "gpt-4o-mini".into());
        let thinking_raw = cli.thinking.clone().or_else(|| settings.thinking.clone());
        let thinking = thinking_raw
            .as_deref()
            .map(|s| {
                s.parse()
                    .map_err(|e| anyhow::anyhow!("thinking 解析失败: {e:#}"))
            })
            .transpose()?
            .or_else(|| rupi_llm::parse_model_spec(&model).thinking);
        let reserve_tokens = cli
            .compress_threshold
            .unwrap_or_else(|| settings.reserve_tokens());
        let keep_recent_tokens = cli
            .compress_keep
            .unwrap_or_else(|| settings.keep_recent_tokens());
        let tool_allow = if cli.no_tools {
            Some(Vec::new())
        } else {
            cli.tools
                .as_deref()
                .map(rupi_config::parse_tool_list)
                .filter(|v| !v.is_empty() || cli.tools.is_some())
        };
        let mut tool_exclude = settings.exclude_tools.clone();
        if let Some(raw) = &cli.exclude_tools {
            tool_exclude.extend(rupi_config::parse_tool_list(raw));
        }
        let builtin_allow = if tool_allow.is_some() {
            None
        } else {
            settings.tools.clone()
        };
        let system = rupi_config::load_system_prompt_files(
            home,
            &cwd,
            cli.system_prompt.as_deref(),
            cli.append_system_prompt.as_deref(),
            load_project,
        );
        Ok(Self {
            model,
            thinking,
            reserve_tokens,
            keep_recent_tokens,
            context_window: settings.compaction.context_window,
            compaction_enabled: settings.compaction_enabled(),
            tool_allow,
            tool_exclude,
            builtin_allow,
            system,
            theme: settings.theme().to_string(),
            overrides: merge_compression_overrides(&settings),
            persist: !cli.no_session,
            settings,
        })
    }
}

fn merge_compression_overrides(
    settings: &rupi_config::Settings,
) -> std::collections::HashMap<String, rupi_agent::CompressionOverride> {
    let mut m = std::collections::HashMap::new();
    for (k, v) in &settings.compaction.model_overrides {
        m.insert(
            k.clone(),
            rupi_agent::CompressionOverride {
                reserve_tokens: v.reserve_tokens,
                keep_recent_tokens: v.keep_recent_tokens,
                context_window: v.context_window,
            },
        );
    }
    for (k, v) in load_compression_overrides() {
        m.insert(k, v);
    }
    m
}

fn apply_prompt(agent: &mut AgentLoop, sys: &rupi_config::SystemPromptFiles) {
    if let Some(r) = &sys.replace {
        agent.builder.base = r.clone();
    }
    if !sys.append.is_empty() {
        agent.builder.append = sys.append.clone();
    }
}

fn apply_tool_filter(tools: &mut ToolRegistry, rt: &Resolved) {
    tools.retain(|n| rupi_config::tool_allowed(n, rt.tool_allow.as_deref(), &rt.tool_exclude));
}

fn apply_queue_settings(inbox: &rupi_agent::MessageInbox, settings: &rupi_config::Settings) {
    if let Some(m) = settings
        .steering_mode
        .as_deref()
        .and_then(rupi_agent::QueueMode::parse)
    {
        inbox.set_steering_mode(m);
    }
    if let Some(m) = settings
        .follow_up_mode
        .as_deref()
        .and_then(rupi_agent::QueueMode::parse)
    {
        inbox.set_follow_up_mode(m);
    }
}

fn sync_tool_env(sid: &str, provider: &dyn LlmProvider, thinking: Option<rupi_llm::ThinkingLevel>) {
    rupi_tools::export_session_context(rupi_tools::SessionContext {
        session_id: sid.to_string(),
        session_file: String::new(),
        provider: provider.name().to_string(),
        model: provider.model_id().unwrap_or("").to_string(),
        reasoning_level: thinking.map(|t| t.as_str().to_string()).unwrap_or_default(),
    });
}

fn handle_settings_repl(
    input: &str,
    settings: &mut rupi_config::Settings,
    home: &PathBuf,
    inbox: Option<&rupi_agent::MessageInbox>,
) -> bool {
    let Some(cmd) = rupi_config::parse_settings_slash(input) else {
        return false;
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let path = rupi_config::write_target(home, &cwd);
    match cmd {
        rupi_config::SettingsSlash::Show => {
            println!("{}", rupi_config::format_settings(settings, &path));
        }
        rupi_config::SettingsSlash::Set { key, value } => {
            if value.is_empty() {
                println!("[settings] usage: /settings <key> <value>");
                return true;
            }
            match rupi_config::apply_setting(settings, &key, &value) {
                Ok((jk, jv)) => match rupi_config::persist_patch(&path, &jk, jv) {
                    Ok(()) => {
                        if let Some(inbox) = inbox {
                            apply_queue_settings(inbox, settings);
                        }
                        println!("[settings] {jk} = {value} (saved {})", path.display());
                    }
                    Err(e) => println!("[settings] write failed: {e:#}"),
                },
                Err(e) => println!("[settings] {e:#}"),
            }
        }
    }
    true
}

fn configure_agent(mut agent: AgentLoop, rt: &Resolved) -> AgentLoop {
    agent = agent
        .with_compression(rt.reserve_tokens, rt.keep_recent_tokens)
        .with_compaction_enabled(rt.compaction_enabled)
        .with_compression_overrides(rt.overrides.clone())
        .with_tool_filter(rt.tool_allow.clone(), rt.tool_exclude.clone());
    if let Some(w) = rt.context_window {
        agent = agent.with_context_window(w);
    }
    apply_prompt(&mut agent, &rt.system);
    agent
}

fn cwd_string() -> String {
    std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".into())
}

/// 思考强度解析：未传 flag 即 None（不干预）；非法值直接 bail 并列合法档。
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
    if cli.no_session {
        let sid = uuid::Uuid::new_v4().to_string();
        eprintln!("[session {sid}] ephemeral (--no-session, not persisted)");
        rupi_tools::export_session_id(&sid);
        let mut s = SessionTree::new();
        s.id = sid.clone();
        return Ok((s, sid));
    }
    if cli.resume.is_some() && cli.continue_session {
        eprintln!("[continue] ignored because --resume was set");
    }
    if cli.resume.is_none() && cli.continue_session {
        match sess_db.latest_session(Some(&cwd_string()))? {
            Some(id) => {
                eprintln!("[continue] latest session {id}");
                return restore_session(sess_db, &id);
            }
            None => eprintln!("[continue] no prior session, starting new"),
        }
    }
    if let Some(id) = &cli.resume {
        return restore_session(sess_db, id);
    } else {
        let sid =
            sess_db.create_session_ex("default", cli.name.as_deref(), Some(&cwd_string()), None)?;
        eprintln!("[session {sid}] turns persist to sessions.db");
        rupi_tools::export_session_id(&sid);
        let mut s = SessionTree::new();
        s.id = sid.clone();
        Ok((s, sid))
    }
}

fn restore_session(sess_db: &SessionStore, id: &str) -> anyhow::Result<(SessionTree, String)> {
    let msgs = sess_db.session_records(id, 500)?;
    if msgs.is_empty() {
        if sess_db.has_session(id)? {
            eprintln!("[resume {id}] session exists but empty, starting fresh under same id");
            rupi_tools::export_session_id(id);
            let mut s = SessionTree::new();
            s.id = id.to_string();
            return Ok((s, id.to_string()));
        }
        anyhow::bail!("unknown session: {id} (see `rupi sessions`)");
    }
    let mut s = SessionTree::new();
    for rec in msgs {
        s.push_with_id(rec.id.clone(), rec.to_message());
    }
    eprintln!("[resume {id}] restored {} msgs", s.history().len());
    rupi_tools::export_session_id(id);
    let stored = sess_db.get_summary(id).unwrap_or_default();
    if !stored.is_empty() {
        if let Some(first) = s.current_path.first().cloned() {
            s.summary = Some(stored);
            s.summary_through = Some(first);
        }
    }
    s.id = id.to_string();
    Ok((s, id.to_string()))
}

/// 回合落盘：本轮新增的全部节点（user、含工具调用的 assistant、工具结果、最终答复）
/// 逐条落 sessions.db —— `content` 存纯文本供 FTS/展示，`blocks` 存完整消息 JSON 供
/// `--resume` 结构化回填（此前只存 user 原文 + 最后一条助手文本，恢复后丢全部工具上下文）。
/// 行 id 沿用树节点 id，resume 后短 id 跨进程稳定，`/goto` 可用。失败只 warning，不断聊天。
fn turn_rows(session: &SessionTree, before_len: usize) -> Vec<rupi_memory::SessionMessageRow> {
    session
        .current_path
        .iter()
        .skip(before_len)
        .filter_map(|id| session.nodes.get(id).map(|n| (id, n)))
        .map(|(id, node)| rupi_memory::SessionMessageRow {
            id: id.clone(),
            role: role_label(&node.message.role).into(),
            content: node.message.full_text(),
            blocks: serde_json::to_string(&node.message).ok(),
        })
        .collect()
}

async fn persist_turn_if(
    persist: bool,
    store: &Arc<Mutex<SessionStore>>,
    sid: &str,
    session: &SessionTree,
    before_len: usize,
    summary: Option<&str>,
) {
    if !persist {
        return;
    }
    let rows = turn_rows(session, before_len);
    let sid = sid.to_string();
    let summary = summary.map(str::to_string);
    let store = store.clone();
    match tokio::task::spawn_blocking(move || {
        store
            .lock()
            .unwrap()
            .persist_turn(&sid, &rows, summary.as_deref())
    })
    .await
    {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => tracing::warn!("persist turn failed: {e:#}"),
        Err(e) => tracing::warn!("persist turn join failed: {e:#}"),
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

/// REPL/TUI 共用的会话互操作斜杠。处理了返回 true。
async fn handle_session_slash(
    input: &str,
    sess_db: &Arc<Mutex<SessionStore>>,
    session: &mut SessionTree,
    sid: &mut String,
    provider: &mut Arc<dyn LlmProvider>,
    persist: bool,
    opts: &rupi_llm::ProviderOptions,
) -> anyhow::Result<bool> {
    let t = input.trim();
    if t == "/export" || t.starts_with("/export ") {
        let arg = t.strip_prefix("/export").unwrap_or("").trim();
        let html = arg.ends_with(".html") || arg == "html";
        let path = if arg.is_empty() || arg == "html" || arg == "jsonl" {
            let ext = if html { "html" } else { "jsonl" };
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(format!("rupi-session-{}.{ext}", &sid[..8.min(sid.len())]))
        } else {
            PathBuf::from(arg)
        };
        let sid_q = sid.clone();
        let db = sess_db.clone();
        let name =
            tokio::task::spawn_blocking(move || db.lock().unwrap().get_name(&sid_q).ok().flatten())
                .await
                .ok()
                .flatten();
        let body = if html {
            rupi_memory::export_tree_html(session, name.as_deref().unwrap_or(sid))
        } else {
            rupi_memory::export_tree_jsonl(session, &cwd_string(), name.as_deref(), None)
        };
        tokio::fs::write(&path, body).await?;
        println!("[export] {}", path.display());
        return Ok(true);
    }
    if t == "/import" {
        println!("[import] usage: /import <file.jsonl>");
        return Ok(true);
    }
    if let Some(path) = t.strip_prefix("/import ") {
        let path = path.trim();
        let raw = tokio::fs::read_to_string(path).await?;
        let cwd = cwd_string();
        let db = sess_db.clone();
        let (new_id, tree) = tokio::task::spawn_blocking(move || {
            let db = db.lock().unwrap();
            rupi_memory::import_into_store(&db, &raw, &cwd)
        })
        .await
        .map_err(|e| anyhow::anyhow!("import join: {e}"))??;
        *session = tree;
        *sid = new_id.clone();
        rupi_tools::export_session_id(&new_id);
        if let Ok(p) = build_provider(
            provider.model_id().unwrap_or("gpt-4o-mini"),
            Some(&new_id),
            opts,
        )
        .await
        {
            *provider = p.into();
        }
        println!("[imported {new_id}] {} msgs", session.history().len());
        return Ok(true);
    }
    if t == "/fork" || t == "/clone" {
        let path_only = t == "/fork";
        if !persist {
            println!(
                "[{}] --no-session: staying ephemeral",
                t.trim_start_matches('/')
            );
            return Ok(true);
        }
        let new_tree = rupi_memory::remap_tree(session, path_only);
        let cwd = cwd_string();
        let parent = sid.clone();
        let db = sess_db.clone();
        let new_id = tokio::task::spawn_blocking(move || {
            let db = db.lock().unwrap();
            let new_id = db.create_session_ex(
                if path_only { "fork" } else { "clone" },
                None,
                Some(&cwd),
                Some(&parent),
            )?;
            rupi_memory::persist_tree(&db, &new_id, &new_tree)?;
            Ok::<_, anyhow::Error>((new_id, new_tree))
        })
        .await
        .map_err(|e| anyhow::anyhow!("fork/clone join: {e}"))??;
        let (new_id, new_tree) = new_id;
        *session = new_tree;
        session.id = new_id.clone();
        *sid = new_id.clone();
        rupi_tools::export_session_id(&new_id);
        println!(
            "[{} {}] {} nodes",
            if path_only { "forked" } else { "cloned" },
            &new_id[..8.min(new_id.len())],
            session.nodes.len()
        );
        return Ok(true);
    }
    if t == "/name" || t.starts_with("/name ") {
        let arg = t.strip_prefix("/name").unwrap_or("").trim();
        if arg.is_empty() {
            let sid_q = sid.clone();
            let db = sess_db.clone();
            let n = tokio::task::spawn_blocking(move || {
                db.lock().unwrap().get_name(&sid_q).ok().flatten()
            })
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
            if n.is_empty() {
                println!("[name] (unset)");
            } else {
                println!("[name] {n}");
            }
        } else if persist {
            let sid_q = sid.clone();
            let db = sess_db.clone();
            let name = arg.to_string();
            tokio::task::spawn_blocking(move || db.lock().unwrap().set_name(&sid_q, &name))
                .await
                .map_err(|e| anyhow::anyhow!("name join: {e}"))??;
            println!("[name] {arg}");
        } else {
            println!("[name] --no-session: not persisted ({arg})");
        }
        return Ok(true);
    }
    Ok(false)
}

/// `--mode rpc`：装配与 `run` 同构的会话，再走 JSONL 协议循环。
async fn run_rpc(cli: &Cli, home: &PathBuf) -> anyhow::Result<()> {
    let _ = approver_for(cli, None)?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let pre = rupi_config::Settings::load(home, &cwd);
    rupi_llm::load_extra_providers(&home.join("providers.json"));
    let load_project = match project_resources() {
        None => true,
        Some((_root, _))
            if cli.trust_project || pre.project_trust() == rupi_config::ProjectTrust::Always =>
        {
            true
        }
        Some(_) if pre.project_trust() == rupi_config::ProjectTrust::Never => {
            eprintln!("[trust] defaultProjectTrust=never：跳过项目资源");
            false
        }
        Some(_) => {
            eprintln!("[trust] rpc 默认跳过项目资源（加 --trust-project 加载）");
            false
        }
    };
    let rt = Resolved::load(cli, home, load_project)?;
    let thinking = rt.thinking;
    let mut tools = sandboxed_tools_filtered(rt.builtin_allow.as_deref());
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
    let mut store = memory_store(home, load_project);
    if cli.no_memory {
        store.memory_enabled = false;
        store.user_profile_enabled = false;
    }
    let frozen = store.frozen_snapshot();
    let mut mem_mgr = MemoryManager::new(store);
    maybe_external_memory(cli, home, &mut mem_mgr).await?;
    let skills = SkillRegistry::discover(&skill_dirs(home, load_project));
    let sess_db = SessionStore::open(home)?;
    let (session_tree, sid) = restore_or_new(cli, &sess_db)?;
    let provider: Arc<dyn LlmProvider> =
        build_provider(&rt.model, Some(&sid), &provider_options(cli))
            .await?
            .into();
    let inbox = Arc::new(rupi_agent::MessageInbox::new());
    let mut agent = configure_agent(AgentLoop::new(cli.max_turns), &rt)
        .with_inbox(inbox)
        .with_context_dirs(context_cwd(load_project, home), home.clone())
        .with_policy(Arc::new(default_policy()))
        .with_plan_mode(cli.plan);
    if cli.parallel_tools {
        agent = agent.with_tool_execution(rupi_agent::ToolExecution::Parallel);
    }
    if cli.discover_tools {
        agent = agent.with_discovery(rupi_agent::DiscoveryConfig::default());
    }
    if let Some(t) = thinking {
        agent = agent.with_thinking(t);
    }
    apply_tool_filter(&mut tools, &rt);
    let store = Arc::new(Mutex::new(sess_db));
    let mut sess = rupi_agent::AgentSession::from_parts(
        provider,
        agent,
        session_tree,
        tools,
        mem_mgr,
        frozen,
        skills,
        sid.clone(),
    );
    sess.extensions = ext_set.extension_arcs();
    sess.session_name = cli.name.clone();
    sess.sess_db = Some(store.clone());
    sess.persist = rt.persist;
    sess.command_dirs = command_dirs_filtered(home, load_project);
    sess.provider_opts = provider_options(cli);
    apply_queue_settings(&sess.inbox, &rt.settings);
    sync_tool_env(&sess.session_id, &*sess.provider, sess.agent.thinking);
    eprintln!("[rpc] session {sid} — JSONL on stdin/stdout");
    let sess = rpc::serve(sess).await?;
    persist_turn_if(
        rt.persist,
        &store,
        &sess.session_id,
        &sess.session,
        0,
        sess.session.summary.as_deref(),
    )
    .await;
    Ok(())
}

/// 非交互执行一次（对标 pi -p）：跑完即退出，回合落盘进会话库。
/// 无问询：Ask 无审批器即拒绝（除非 --approve）；项目资源默认跳过（除非 --trust-project）。
/// stdout 只走模型正文（可管道），诊断走 stderr。
async fn run_once(cli: &Cli, home: &PathBuf, prompt: &str, json: bool) -> anyhow::Result<()> {
    // 审批档位先验（错配直接 bail，不建会话不落盘）
    let approver = approver_for(cli, None)?;
    // 非交互不提问：有项目资源且未 --trust-project 则跳过并告知
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let pre = rupi_config::Settings::load(home, &cwd);
    rupi_llm::load_extra_providers(&home.join("providers.json"));
    let load_project = match project_resources() {
        None => true,
        Some((root, _))
            if cli.trust_project || pre.project_trust() == rupi_config::ProjectTrust::Always =>
        {
            eprintln!(
                "[trust] {}: 本次加载项目资源 {}",
                if cli.trust_project {
                    "--trust-project"
                } else {
                    "defaultProjectTrust=always"
                },
                root.display()
            );
            true
        }
        Some(_) if pre.project_trust() == rupi_config::ProjectTrust::Never => {
            eprintln!("[trust] defaultProjectTrust=never：跳过项目资源");
            false
        }
        Some(_) => {
            eprintln!("[trust] 非交互默认跳过项目资源（加 --trust-project 加载）");
            false
        }
    };
    let rt = Resolved::load(cli, home, load_project)?;
    let thinking = rt.thinking;
    let mut tools = sandboxed_tools_filtered(rt.builtin_allow.as_deref());
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
    let sess_db = Arc::new(Mutex::new(SessionStore::open(home)?));
    let (mut session, sid) = restore_or_new(cli, &sess_db.lock().unwrap())?;
    // provider 在会话 id 落定后构造：亲和头荷载即 sessions.db 会话 id，
    // --resume 同 id 即同一下游（实例级随机 id 只保同进程粘滞）。
    let provider: Arc<dyn LlmProvider> =
        build_provider(&rt.model, Some(&sid), &provider_options(cli))
            .await?
            .into();
    let mut agent = configure_agent(AgentLoop::new(cli.max_turns), &rt);
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
        .inherit_from(&agent);
        tools.register(Arc::new(sub));
        eprintln!("[subagents] subagent tool enabled");
    }
    apply_tool_filter(&mut tools, &rt);
    sync_tool_env(&sid, &*provider, agent.thinking);
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
                        let mut m = rupi_agent::TokenMeter::new(&rt.model);
                        if let Some(w) = rt.context_window {
                            m.set_context_window(w);
                        }
                        m.note_usage(input_tokens, output_tokens, input_tokens);
                        eprintln!("\n[{}]", m.footer(m.calibrate(input_tokens)));
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
    persist_turn_if(
        rt.persist,
        &sess_db,
        &sid,
        &session,
        before_len,
        session.summary.as_deref(),
    )
    .await;
    if cli.review_apply {
        let home = home.clone();
        let pending = pending.clone();
        if let Err(e) =
            tokio::task::spawn_blocking(move || apply_suggestions(&home, &pending)).await
        {
            tracing::warn!("review apply join failed: {e}");
        }
    }
    Ok(())
}

async fn run_chat(cli: &Cli, home: &PathBuf) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    rupi_llm::load_extra_providers(&home.join("providers.json"));
    let pre = rupi_config::Settings::load(home, &cwd);
    let load_project = load_project_resources(home, cli, &pre);
    let rt = Resolved::load(cli, home, load_project)?;
    let mut model = rt.model.clone();
    let pending: Arc<std::sync::Mutex<Vec<ReviewSuggestion>>> =
        Arc::new(std::sync::Mutex::new(vec![]));
    let inbox = Arc::new(rupi_agent::MessageInbox::new());
    apply_queue_settings(&inbox, &rt.settings);
    let mut agent = configure_agent(AgentLoop::new(cli.max_turns), &rt).with_inbox(inbox.clone());
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
    if let Some(t) = rt.thinking {
        println!("[thinking] level: {t:?}");
        agent = agent.with_thinking(t);
    }
    let mut tools = sandboxed_tools_filtered(rt.builtin_allow.as_deref());
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
    // 项目信任门已在启动时问过（load_project）。
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
    let sess_db = Arc::new(Mutex::new(SessionStore::open(home)?));
    let (mut session, mut sid) = restore_or_new(cli, &sess_db.lock().unwrap())?;
    // provider 与 reviewer 在会话 id 落定后装配：亲和头荷载即 sessions.db 会话 id
    let mut provider: Arc<dyn LlmProvider> =
        build_provider(&model, Some(&sid), &provider_options(cli))
            .await?
            .into();
    sync_tool_env(&sid, &*provider, agent.thinking);
    let mut settings = rt.settings.clone();
    let meter = std::sync::Arc::new(std::sync::Mutex::new({
        let mut m = rupi_agent::TokenMeter::new(&model);
        if let Some(w) = rt.context_window {
            m.set_context_window(w);
        }
        m
    }));
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
        .inherit_from(&agent);
        tools.register(Arc::new(sub));
        println!("[subagents] subagent tool enabled");
    }
    apply_tool_filter(&mut tools, &rt);

    println!("rupi v0.1.0 — 输入 /quit 退出，Ctrl-C 中止本轮，/rewind 回退，/tree 看树，/goto <短id> 跳转，/compact 手动压实，/model [provider/model[:thinking]] 切换模型，/thinking [off|low|medium|high|xhigh|max] 思考强度，/settings 改 steeringMode/followUpMode/defaultProjectTrust/externalEditor/enabledModels，/reload 重载扩展，/plan 切换计划模式，/skills 看技能，/commands 看自定义命令，/export /import /fork /clone /name");
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
        if handle_settings_repl(&input, &mut settings, home, Some(&inbox)) {
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
                        meter.lock().unwrap().set_model(&model);
                        sync_tool_env(&sid, &*provider, agent.thinking);
                        ext_set.emit_event(&rupi_core::AgentEvent::ModelChange {
                            provider: provider.name().to_string(),
                            model: model.clone(),
                        });
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
                        rupi_tools::export_reasoning_level(t.as_str());
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
                .force_compress_with_prompt(
                    &*provider,
                    &mut session,
                    &mem,
                    &|e| match e {
                        rupi_core::AgentEvent::CompactionStart => eprintln!("[compacting]…"),
                        rupi_core::AgentEvent::CompactionEnd { summarized, kept } => {
                            eprintln!("[compacted: summarized {summarized}, kept {kept}]")
                        }
                        _ => {}
                    },
                    prompt,
                )
                .await;
            if session.summary != before && session.summary.is_some() {
                println!("[compacted]");
                persist_turn_if(
                    rt.persist,
                    &sess_db,
                    &sid,
                    &session,
                    session.current_path.len(),
                    session.summary.as_deref(),
                )
                .await;
                if let Some(sum) = session.summary.as_deref() {
                    saved_summary = sum.to_string();
                }
            } else {
                println!("[compact] nothing to compress");
            }
            continue;
        }
        // 裸 `/goto`（无参数）必须拦截给用法提示：此前漏进自定义命令查找，
        // 查不到就当普通消息发给模型，白烧一轮（TUI 同语义，见 dispatch_builtin）。
        if handle_session_slash(
            &input,
            &sess_db,
            &mut session,
            &mut sid,
            &mut provider,
            rt.persist,
            &provider_options(cli),
        )
        .await?
        {
            continue;
        }
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
        // 未命中再回退 skill 名（`/skillname args` 即调 skill；`/skill:name` 同义）。
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
        let on_event = |e| match e {
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
                meter
                    .lock()
                    .unwrap()
                    .note_usage(input_tokens, output_tokens, input_tokens);
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
        };
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
            &on_event,
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
        let new_summary = session.summary.as_deref().filter(|s| *s != saved_summary);
        persist_turn_if(
            rt.persist,
            &sess_db,
            &sid,
            &session,
            before_len,
            new_summary,
        )
        .await;
        if let Some(sum) = new_summary {
            saved_summary = sum.to_string();
        }
        {
            let m = meter.lock().unwrap();
            println!("\n[{}]", m.footer(session.history_tokens()));
        }
        if cli.review_apply {
            let home = home.clone();
            let pending = pending.clone();
            if let Err(e) =
                tokio::task::spawn_blocking(move || apply_suggestions(&home, &pending)).await
            {
                tracing::warn!("review apply join failed: {e}");
            }
        }
        println!();
    }
    Ok(())
}

async fn run_tui(cli: &Cli, home: &PathBuf) -> anyhow::Result<()> {
    // 项目信任门（全屏启动前 stdin 问一次，与 REPL 同语义）
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    rupi_llm::load_extra_providers(&home.join("providers.json"));
    let pre = rupi_config::Settings::load(home, &cwd);
    let load_project = load_project_resources(home, cli, &pre);
    let rt = Resolved::load(cli, home, load_project)?;
    let thinking = rt.thinking;
    let mut tools = sandboxed_tools_filtered(rt.builtin_allow.as_deref());
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
    let mut provider: Arc<dyn LlmProvider> =
        build_provider(&rt.model, Some(&sid), &provider_options(cli))
            .await?
            .into();
    let inbox = std::sync::Arc::new(rupi_agent::MessageInbox::new());
    apply_queue_settings(&inbox, &rt.settings);
    let mut agent = configure_agent(AgentLoop::new(cli.max_turns), &rt).with_inbox(inbox);
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
        .inherit_from(&agent);
        tools.register(Arc::new(sub));
        eprintln!("[subagents] subagent tool enabled");
    }
    apply_tool_filter(&mut tools, &rt);
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
                                Ok(dir) => lines
                                    .push(format!("[review] skill drafted at {}", dir.display())),
                                Err(e) => {
                                    lines.push(format!("[review] skill draft skipped: {e:#}"))
                                }
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
    let persist_turns = rt.persist;
    let mut settings = rt.settings.clone();
    sync_tool_env(&sid, &*provider, agent.thinking);
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
        meter: Some(Arc::new(std::sync::Mutex::new({
            let mut m = rupi_agent::TokenMeter::new(&rt.model);
            if let Some(w) = rt.context_window {
                m.set_context_window(w);
            }
            m
        }))),
        persist: rt.persist,
        settings: Some(&mut settings),
        settings_home: home.clone(),
        settings_cwd: cwd.clone(),
        on_turn: Some(Arc::new(move |t: rupi_tui::TurnRecord| {
            if !persist_turns {
                return;
            }
            let db = sess_db.lock().unwrap();
            let sid = sid_for_turn.lock().unwrap().clone();
            // 本轮全部新增节点一次事务落盘（含工具调用/结果，blocks 存完整 JSON）
            let rows: Vec<rupi_memory::SessionMessageRow> = t
                .messages
                .iter()
                .map(|(id, msg)| rupi_memory::SessionMessageRow {
                    id: id.clone(),
                    role: role_label(&msg.role).into(),
                    content: msg.full_text(),
                    blocks: serde_json::to_string(msg).ok(),
                })
                .collect();
            let summary = t.summary.as_ref().and_then(|sum| {
                if *sum != *saved.lock().unwrap() {
                    Some(sum.clone())
                } else {
                    None
                }
            });
            if let Err(e) = db.persist_turn(&sid, &rows, summary.as_deref()) {
                tracing::warn!("persist turn failed: {e:#}");
            } else if let Some(sum) = summary {
                *saved.lock().unwrap() = sum;
            }
        })),
    };
    rupi_tui::launch(ctx).await
}
