//! rupi CLI：coding agent 交互入口 + MCP / 记忆 / Skill / 会话管理子命令。

use clap::{Parser, Subcommand};
use rupi_agent::{AgentLoop, HeuristicReviewer, ReviewSuggestion, SubagentTool};
use rupi_core::{Message, SessionTree};
use rupi_llm::{LlmProvider, MockProvider, OpenAiCompatProvider};
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
    let store = memory_store(home);
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
    /// 每轮结束后后台 review，给出记忆/Skill 沉淀建议
    #[arg(long, default_value_t = false)]
    review: bool,
    /// 把 review 建议直接落盘（memory add + skill 草稿）
    #[arg(long, default_value_t = false)]
    review_apply: bool,
    /// 用模型做后台复盘（默认启发式离线 review；LLM 版烧 token 但提炼质量更高）
    #[arg(long, default_value_t = false)]
    review_llm: bool,
    /// 会话压缩阈值（历史字符数，超限摘要最旧部分）
    #[arg(long, default_value_t = 60_000)]
    compress_threshold: usize,
    /// 压缩后保留的近期消息条数
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
}

#[derive(Subcommand)]
enum Cmd {
    /// 交互式聊天（默认）
    Chat,
    /// 全屏终端界面（ratatui）
    Tui,
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
fn memory_store(home: &PathBuf) -> MemoryStore {
    let mut s = MemoryStore::new(home.clone());
    if let Ok(cwd) = std::env::current_dir() {
        if let Some(root) = MemoryStore::discover_project(&cwd) {
            s = s.with_project(root);
        }
    }
    s
}

fn skill_dirs(home: &PathBuf) -> Vec<PathBuf> {
    vec![
        PathBuf::from("skills/builtin"),
        home.join("skills"),
        PathBuf::from(".rupi/skills"),
    ]
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
            let store = memory_store(&home);
            let frozen = store.frozen_snapshot();
            println!(
                "--- MEMORY.md ---\n{}\n--- USER.md ---\n{}\n--- failures.md ---\n{}",
                frozen.memory, frozen.user, frozen.failures
            );
        }
        Some(Cmd::MemoryWrite { op, entry, scope }) => {
            let store = memory_store(&home);
            if op == "failure" {
                store.record_failure(&entry)?;
                println!("failure recorded.");
            } else {
                let live = store.apply_write_scoped(&scope, &op, &entry)?;
                println!("updated [{scope}]. live state:\n{live}");
            }
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
        Some(Cmd::MemorySearch { query }) => {
            let store = SessionStore::open(&home)?;
            for (target, snippet) in store.memory_search(&query, 10)? {
                println!("[{target}] {snippet}");
            }
        }
        Some(Cmd::ExtList) => {
            let dir = ext_dir(&home, &cli);
            let mut set = rupi_ext::ExtensionSet::new(dir);
            for m in set.load_all() {
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
                println!("== {role} @ {created} [{}] ==\n{content}", &id[..8.min(id.len())]);
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

/// 默认规则：危险 bash 子串转人工（REPL 有审批器；非交互/TUI 无审批则拒绝）。
fn default_policy() -> rupi_agent::RulePolicy {
    rupi_agent::RulePolicy {
        bash_block: vec!["rm -rf /".into(), "mkfs".into(), "dd if=".into()],
        ..Default::default()
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

async fn run_chat(cli: &Cli, home: &PathBuf) -> anyhow::Result<()> {
    let mut model = cli.model.clone();
    let mut provider: Arc<dyn LlmProvider> = build_provider(&model).await?.into();
    let pending: Arc<std::sync::Mutex<Vec<ReviewSuggestion>>> =
        Arc::new(std::sync::Mutex::new(vec![]));
    let mut agent =
        AgentLoop::new(cli.max_turns).with_compression(cli.compress_threshold, cli.compress_keep);
    agent = agent
        .with_policy(Arc::new(default_policy()))
        .with_approver(Arc::new(TerminalApprover::default()))
        .with_plan_mode(cli.plan);
    if cli.plan {
        println!("[plan mode] read-only: write/edit/bash disabled");
    }
    if cli.review || cli.review_apply {
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
    let mut tools = ToolRegistry::with_builtins();
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
    let store = memory_store(home);
    let frozen = store.frozen_snapshot();
    let mut mem_mgr = MemoryManager::new(store);
    maybe_external_memory(cli, home, &mut mem_mgr).await?;
    let mem = Arc::new(mem_mgr);
    let skills = Arc::new(SkillRegistry::discover(&skill_dirs(home)));
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
        .with_plan_mode(cli.plan);
        tools.register(Arc::new(sub));
        println!("[subagents] subagent tool enabled");
    }

    println!("rupi v0.1.0 — 输入 /quit 退出，/rewind 回退，/tree 看树，/goto <短id> 跳转，/model [名] 切换模型，/reload 重载扩展，/plan 切换计划模式，/skills 看技能");
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
        skills.refresh(&skill_dirs(home));
        // 自定义斜杠命令：内建优先（上已 continue），命中则展开为提示词
        let slash = rupi_core::commands::split(&input)
            .map(|(n, a)| (n.to_owned(), a.to_owned()));
        if let Some((name, args)) = slash.as_ref() {
            if let Some(expanded) = rupi_core::commands::expand(
                &rupi_core::commands::command_dirs(home),
                name,
                args,
            ) {
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
    let provider: Arc<dyn LlmProvider> = build_provider(&cli.model).await?.into();
    let mut tools = ToolRegistry::with_builtins();
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
    let store = memory_store(home);
    let frozen = store.frozen_snapshot();
    let mut mem_mgr = MemoryManager::new(store);
    maybe_external_memory(cli, home, &mut mem_mgr).await?;
    let mem = Arc::new(mem_mgr);
    let skills = Arc::new(SkillRegistry::discover(&skill_dirs(home)));
    let sess_db = Arc::new(std::sync::Mutex::new(SessionStore::open(home)?));
    let (mut session, sid) = {
        let db = sess_db.lock().unwrap();
        restore_or_new(cli, &db)?
    };
    let mut agent =
        AgentLoop::new(cli.max_turns).with_compression(cli.compress_threshold, cli.compress_keep);
    // TUI 内审批：Ask 时暂停全屏问一句 [y/N]（与 REPL 同语义）；plan mode 同 REPL
    agent = agent
        .with_policy(Arc::new(default_policy()))
        .with_approver(Arc::new(rupi_tui::TuiApprover::default()))
        .with_plan_mode(cli.plan);
    if cli.subagents {
        let sub = SubagentTool::new(
            provider.clone(),
            Arc::new(tools.clone()),
            mem.clone(),
            frozen.clone(),
            skills.clone(),
            cli.max_turns,
        )
        .with_plan_mode(cli.plan);
        tools.register(Arc::new(sub));
        eprintln!("[subagents] subagent tool enabled");
    }
    let review_lines: Option<Arc<std::sync::Mutex<Vec<String>>>> = if cli.review || cli.review_apply
    {
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
                    let store = memory_store(&home_clone);
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
        provider: &*provider,
        agent: &agent,
        session: &mut session,
        tools: &tools,
        mem: &*mem,
        frozen: &frozen,
        skills: &*skills,
        skill_dirs: skill_dirs(home),
        command_dirs: rupi_core::commands::command_dirs(home),
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
