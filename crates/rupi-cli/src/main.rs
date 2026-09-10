//! rupi CLI：coding agent 交互入口 + MCP / 记忆 / Skill / 会话管理子命令。

use clap::{Parser, Subcommand};
use rupi_agent::{AgentLoop, HeuristicReviewer, ReviewSuggestion, SubagentTool};
use rupi_core::{Message, SessionTree};
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
    /// 思考强度（对标上游 /thinking：off|low|medium|high；映射为各 provider 推理参数）
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
    let mut dirs = vec![PathBuf::from("skills/builtin"), home.join("skills")];
    // 项目 skills 与项目记忆同门：信任被拒则不发现、不加载
    if load_project {
        dirs.push(PathBuf::from(".rupi/skills"));
    }
    dirs
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
    println!("[sandbox workspace: {}]", root.display());
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
    rupi_ext::register_all(tools, manifests);
    set
}

/// 增量热重载：新增/修改重注册，删除注销。
fn refresh_extensions(tools: &mut ToolRegistry, set: &mut rupi_ext::ExtensionSet) {
    let (changed, removed) = set.refresh();
    for name in removed {
        tools.unregister(&name);
        eprintln!("[ext] removed {name}");
    }
    if !changed.is_empty() {
        let names: Vec<String> = changed.iter().map(|m| m.name.clone()).collect();
        rupi_ext::register_all(tools, changed);
        eprintln!("[ext] reloaded: {}", names.join(", "));
    }
}

async fn build_provider(model: &str) -> anyhow::Result<Box<dyn LlmProvider>> {
    // 路由收敛到 rupi_llm::provider_for_model（与 TUI /model 同源）；缺 key 回 mock，
    // 各家提示沿用此前的文案（claude-/gemini- 带原错误，其余走固定缺 key 行）。
    match rupi_llm::provider_for_model(model) {
        Ok(p) => Ok(p),
        Err(e) => {
            let demo = if model.starts_with("claude-") {
                eprintln!("[rupi] {e:#} — using mock provider (demo mode)");
                "demo mode：设置 RUPI_ANTHROPIC_KEY 后可接 Claude。已收到你的请求，工具链就绪。"
            } else if model.starts_with("gemini-") {
                eprintln!("[rupi] {e:#} — using mock provider (demo mode)");
                "demo mode：设置 RUPI_GEMINI_KEY 后可接 Gemini。已收到你的请求，工具链就绪。"
            } else {
                eprintln!(
                    "[rupi] no RUPI_API_KEY/OPENAI_API_KEY — using mock provider (demo mode)"
                );
                "demo mode：设置 RUPI_API_KEY 后可接真实模型。已收到你的请求，工具链就绪。"
            };
            Ok(Box::new(MockProvider::new(vec![
                MockProvider::text_response(demo),
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
            println!("{}", reg.index_block());
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
            for (sid, snippet) in store.search(&query, 10)? {
                println!("[{sid}] {snippet}");
            }
        }
        Some(Cmd::MemorySearch { query }) => {
            let store = SessionStore::open(&home)?;
            for (target, snippet) in store.memory_search(&query, 10)? {
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
                println!("{} — {}", m.name, m.description);
            }
        }
        Some(Cmd::Sessions) => {
            let store = SessionStore::open(&home)?;
            for (id, profile, created, count) in store.list_sessions(20)? {
                println!("[{profile}] {id} {created} ({count} msgs)");
            }
        }
        Some(Cmd::SessionShow { id }) => {
            let store = SessionStore::open(&home)?;
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
        Some(Cmd::McpList { command, args }) => {
            let cfg = rupi_mcp::McpServerConfig::new("probe", &command, args);
            let bridge = rupi_mcp::McpBridge::spawn(cfg).await?;
            for t in bridge.list_tools().await? {
                let d = rupi_mcp::mcp_tool_to_definition("mcp", &t);
                println!("{} — {}", d.name, d.description);
            }
        }
        Some(Cmd::Run { ref prompt }) => {
            run_once(&cli, &home, &prompt.join(" ")).await?;
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
    cli.thinking
        .as_deref()
        .map(|s| {
            s.parse()
                .map_err(|e| anyhow::anyhow!("--thinking 解析失败: {e:#}"))
        })
        .transpose()
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

/// 恢复历史会话：按序回填 user/assistant 文本继续聊。
/// 工具中间态不落盘，恢复的是 transcript（对应行数可能少于原树节点数）。
fn restore_or_new(cli: &Cli, sess_db: &SessionStore) -> anyhow::Result<(SessionTree, String)> {
    if let Some(id) = &cli.resume {
        let msgs = sess_db.session_messages(id, 500)?;
        if msgs.is_empty() {
            anyhow::bail!("unknown or empty session: {id} (see `rupi sessions`)");
        }
        let mut s = SessionTree::new();
        for (id, role, content, _) in msgs {
            let msg = match role.as_str() {
                "assistant" => Message::text(rupi_core::Role::Assistant, content),
                _ => Message::text(rupi_core::Role::User, content),
            };
            // 沿用库行 id：跨进程短 id 稳定，/tree 所见即 /goto 可达
            s.push_with_id(id, msg);
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

/// 回合落盘：user 原文 + 本轮最后一条助手答复。失败只 warning，不断聊天。
/// 行 id 沿用树节点 id（`before_len` 为本轮前路径长度，用户节点即 `current_path[before_len]`），
/// resume 回填后短 id 跨进程稳定，`/goto` 可用；找不到则回退随机 id。
fn persist_turn(
    store: &SessionStore,
    sid: &str,
    user: &str,
    session: &SessionTree,
    before_len: usize,
) {
    let user_id = session
        .current_path
        .get(before_len)
        .cloned()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    if let Err(e) = store.add_message_with_id(&user_id, sid, "user", user) {
        tracing::warn!("persist user msg failed: {e:#}");
    }
    let (asst_id, assistant) = session
        .current_path
        .iter()
        .skip(before_len)
        .filter_map(|id| session.nodes.get(id))
        .filter(|n| n.message.role == rupi_core::Role::Assistant)
        .last()
        .map(|n| (n.id.clone(), n.message.full_text()))
        .unwrap_or_else(|| (uuid::Uuid::new_v4().to_string(), String::new()));
    if let Err(e) = store.add_message_with_id(&asst_id, sid, "assistant", &assistant) {
        tracing::warn!("persist assistant msg failed: {e:#}");
    }
}

/// 非交互执行一次（对标 pi -p）：跑完即退出，回合落盘进会话库。
/// 无问询：Ask 无审批器即拒绝（除非 --approve）；项目资源默认跳过（除非 --trust-project）。
/// stdout 只走模型正文（可管道），诊断走 stderr。
async fn run_once(cli: &Cli, home: &PathBuf, prompt: &str) -> anyhow::Result<()> {
    // 审批档位 + thinking 档位先验（错配直接 bail，不建会话不落盘）
    let approver = approver_for(cli, None)?;
    let thinking = thinking_for(cli)?;
    let provider: Arc<dyn LlmProvider> = build_provider(&cli.model).await?.into();
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
    let _ext_set = load_extensions(&mut tools, &ext_dir(home, cli));
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
    let mut agent = AgentLoop::new(cli.max_turns)
        .with_compression(cli.compress_threshold, cli.compress_keep)
        .with_compression_overrides(load_compression_overrides());
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
    use std::io::Write as _;
    agent
        .run(
            &*provider,
            &mut session,
            prompt,
            &tools,
            &*mem,
            &frozen,
            &*skills,
            &[],
            &|e| match e {
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
                _ => {}
            },
        )
        .await?;
    println!();
    persist_turn(&sess_db, &sid, prompt, &session, before_len);
    if cli.review_apply {
        apply_suggestions(home, &pending);
    }
    Ok(())
}

async fn run_chat(cli: &Cli, home: &PathBuf) -> anyhow::Result<()> {
    let mut model = cli.model.clone();
    let mut provider: Arc<dyn LlmProvider> = build_provider(&model).await?.into();
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
    let mut tools = sandboxed_tools();
    // MCP-Direct：spawn 各 server 并把远端工具注册为原生工具（失败只 warning，不断主循环）
    let _mcp = if let Some(path) = &cli.mcp_config {
        let configs = rupi_mcp::load_configs(path)?;
        let manager = rupi_mcp::McpManager::spawn_all(&configs).await?;
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

    println!("rupi v0.1.0 — 输入 /quit 退出，/rewind 回退，/tree 看树，/goto <短id> 跳转，/model [名] 切换模型，/thinking [off|low|medium|high] 思考强度，/reload 重载扩展，/plan 切换计划模式，/skills 看技能，/commands 看自定义命令");
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
            continue;
        }
        if input == "/reload" {
            refresh_extensions(&mut tools, &mut ext_set);
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
                match build_provider(arg).await {
                    Ok(p) => {
                        provider = p.into();
                        model = arg.to_string();
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
        if input == "/rewind" {
            if session.current_path.len() >= 2 {
                let target = session.current_path[session.current_path.len() - 2].clone();
                session.rewind_to(&target);
                println!("[rewound]");
            }
            continue;
        }
        if input == "/tree" {
            print!("{}", session.tree_view());
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
        refresh_extensions(&mut tools, &mut ext_set);
        skills.refresh(&skill_dirs(home, load_project));
        // 自定义斜杠命令：内建优先（上已 continue），命中则展开为提示词
        let slash = rupi_core::commands::split(&input).map(|(n, a)| (n.to_owned(), a.to_owned()));
        if let Some((name, args)) = slash.as_ref() {
            if let Some(expanded) =
                rupi_core::commands::expand(&command_dirs_filtered(home, load_project), name, args)
            {
                println!("[command /{name}]");
                input = expanded;
            }
        }
        let before_len = session.current_path.len();
        agent
            .run(
                &*provider,
                &mut session,
                &input,
                &tools,
                &*mem,
                &frozen,
                &*skills,
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
        persist_turn(&sess_db, &sid, &input, &session, before_len);
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
    let mut provider: Arc<dyn LlmProvider> = build_provider(&cli.model).await?.into();
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
    let _ext_set = load_extensions(&mut tools, &ext_dir(home, cli));
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
    let mut agent = AgentLoop::new(cli.max_turns)
        .with_compression(cli.compress_threshold, cli.compress_keep)
        .with_compression_overrides(load_compression_overrides());
    // TUI 内审批：Ask 时暂停全屏问一句 [y/N]（与 REPL 同语义）；plan mode 同 REPL
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
    let saved = Arc::new(std::sync::Mutex::new(
        session.summary.clone().unwrap_or_default(),
    ));
    let ctx = rupi_tui::TuiContext {
        provider: &mut provider,
        agent: &mut agent,
        session: &mut session,
        tools: &tools,
        mem: &*mem,
        frozen: &frozen,
        skills: &*skills,
        skill_dirs: skill_dirs(home, load_project),
        command_dirs: command_dirs_filtered(home, load_project),
        review_lines,
        on_turn: Some(Arc::new(move |t: rupi_tui::TurnRecord| {
            let db = sess_db.lock().unwrap();
            let persist = |node: &Option<String>, role: &str, content: &str| {
                let res = match node {
                    Some(id) => db.add_message_with_id(id, &sid, role, content),
                    None => db.add_message(&sid, role, content),
                };
                if let Err(e) = res {
                    tracing::warn!("persist {role} msg failed: {e:#}");
                }
            };
            persist(&t.user_node, "user", &t.user);
            persist(&t.assistant_node, "assistant", &t.assistant);
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
