//! ratatui 终端主循环：消息区 + 输入框 + 状态栏。
//! 并发模型：空闲外循环收键；提交后进内循环，用 `select!` 同时驱动 agent future、
//! 排空事件 channel、响应滚动/退出——流式 delta 到达即渲染。

use crate::complete;
use crate::view::{ChatView, InputBuffer, Line};
use anyhow::Context;
use crossterm::{
    event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::{Stream, StreamExt as _};
use ratatui::{
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line as RLine, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Terminal,
};
use rupi_agent::AgentLoop;
use rupi_core::{commands, AgentEvent, Extension, Message, Role, SessionTree};
use rupi_llm::LlmProvider;
use rupi_mcp::McpManager;
use rupi_memory::{FrozenMemory, MemoryManager, SessionStore};
use rupi_skills::SkillRegistry;
use rupi_tools::{export_session_id, ToolRegistry};
use std::io::Stdout;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// TUI 运行上下文：与 REPL 版 `run_chat` 同构的装配。
/// `agent` / `provider` 可变：TUI 内建命令（/plan /thinking /model）会话内切换，
/// 其余只读引用（mem/skills 等切换命令不碰）。
pub struct TuiContext<'a> {
    pub provider: &'a mut Arc<dyn LlmProvider>,
    pub agent: &'a mut AgentLoop,
    pub session: &'a mut SessionTree,
    pub tools: &'a mut ToolRegistry,
    pub mem: &'a MemoryManager,
    pub frozen: &'a FrozenMemory,
    pub skills: &'a SkillRegistry,
    /// MCP 热刷新：manager 活着才有意义；`mcp_rx` 排空收 server 名后差量刷新工具表。
    /// tools 取 `&mut` 即为此：TUI 也要逐轮应用远端工具变更（与 REPL 同语义）。
    pub mcp: Option<&'a McpManager>,
    pub mcp_rx: Option<mpsc::UnboundedReceiver<String>>,
    /// skill 发现目录：每轮发送前 `refresh`，会话内新蒸馏 skill 即时可见（与 REPL 同闭环）。
    pub skill_dirs: Vec<std::path::PathBuf>,
    /// 扩展热重载器：`/reload` 显式触发（与 REPL 同语义）；`None`（单测/内嵌）则提示不可用。
    pub ext_set: Option<&'a mut rupi_ext::ExtensionSet>,
    /// 自定义斜杠命令目录：发送前展开（与 REPL 同语义）。
    pub command_dirs: Vec<std::path::PathBuf>,
    /// review 建议行缓冲（agent 回调写入，UI 每帧排空为 System 行）。`--review` 时装配。
    pub review_lines: Option<Arc<std::sync::Mutex<Vec<String>>>>,
    /// sessions.db 会话 id（共享 cell：`/resume` 切换后落盘回调与亲和头同读此值，
    /// 与 REPL `--resume` 同语义，调用方装配）。
    pub session_id: Arc<Mutex<String>>,
    /// 会话库（`/sessions` 列表与 `/resume` 重建用）；`None`（单测/内嵌）则提示不可用。
    pub sess_db: Option<Arc<Mutex<SessionStore>>>,
    /// 回合落盘回调（调用方做会话持久化）。`--review` 无关，默认装配。
    pub on_turn: Option<Arc<dyn Fn(TurnRecord) + Send + Sync>>,
}

/// 一轮问答记录（传给 `on_turn`）。
#[derive(Debug, Clone, Default)]
pub struct TurnRecord {
    pub user: String,
    pub assistant: String,
    /// 本轮结束时的会话压缩摘要（无压缩则为 None）。
    pub summary: Option<String>,
    /// 树节点 id（落盘沿用，resume 后短 id 稳定）；缺失时调用方回退随机 id。
    pub user_node: Option<String>,
    pub assistant_node: Option<String>,
    /// 本轮新增的全部节点（id + 完整消息）：user、含工具调用的 assistant、工具结果、
    /// 最终答复。调用方逐条落盘（blocks JSON），`/resume` 才能结构化回填工具上下文。
    pub messages: Vec<(String, Message)>,
}

struct Guard;
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
    }
}

/// TUI 内审批器：Ask 裁决时暂停全屏 UI 回主屏问一句 `[y/a(ll session)/N]`，默认拒绝。
/// 与 REPL 的 `TerminalApprover` 同语义；失败（无 TTY / 读不到行）一律拒绝。
/// 选 a 的（工具 + 规则原因）本会话内不再打扰。
pub struct TuiApprover {
    cache: rupi_agent::SessionApprovalCache,
}

impl Default for TuiApprover {
    fn default() -> Self {
        Self {
            cache: rupi_agent::SessionApprovalCache::default(),
        }
    }
}

impl rupi_agent::Approver for TuiApprover {
    fn approve(&self, tool: &str, args: &serde_json::Value, reason: &str) -> bool {
        use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
        if self.cache.is_approved(tool, reason) {
            return true;
        }
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
        eprintln!("[approve] {tool} {args} — {reason} [y(es once)/a(ll session)/N]");
        let mut line = String::new();
        let answer = if std::io::stdin().read_line(&mut line).is_ok() {
            rupi_agent::ApprovalAnswer::parse(&line)
        } else {
            rupi_agent::ApprovalAnswer::Deny
        };
        let _ = execute!(std::io::stdout(), EnterAlternateScreen);
        let _ = enable_raw_mode();
        match answer {
            rupi_agent::ApprovalAnswer::Deny => false,
            rupi_agent::ApprovalAnswer::Once => true,
            rupi_agent::ApprovalAnswer::Session => {
                self.cache.approve_session(tool, reason);
                true
            }
        }
    }
}

/// 启动 TUI（async：在现有 tokio runtime 内跑）。非 TTY 直接报错，调用方回落 REPL。
pub async fn launch(ctx: TuiContext<'_>) -> anyhow::Result<()> {
    if !crossterm::tty::IsTty::is_tty(&std::io::stdin()) {
        anyhow::bail!("no TTY detected; use `rupi chat` (REPL) instead");
    }
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let _guard = Guard; // panic/返回时必恢复终端
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    run_loop(&mut terminal, ctx).await
}

/// 回合内按键收集：运行中继续打字则排队为 follow-up（对标上游 `followUpMode`
/// 默认全量注入），本轮结束后自动作为下一轮输入发出；`Esc` 中止 + 排队并存
/// （先停本轮再跑排队≈转向）。返回 true 表示该键已消费。
/// 纯逻辑（无终端依赖），可单测。调用方在 false 时走原有分支（Ctrl-C/Esc/滚动）。
fn collect_followup(buf: &mut String, key: &KeyEvent) -> bool {
    match key.code {
        // 修饰键组合（Ctrl-C 等）留给调用方，不吞
        KeyCode::Char(c) if key.modifiers.is_empty() => {
            buf.push(c);
            true
        }
        KeyCode::Enter => {
            buf.push('\n');
            true
        }
        KeyCode::Backspace => {
            buf.pop();
            true
        }
        _ => false,
    }
}

/// 内建斜杠派发结果：Quit 退出主循环；Done 顯示一行系统消息并等下一输入；
/// Compact 手动压实（async，调用方执行后推行反馈）；Resume 切会话（调用方重建树，
/// 回填亲和头后推行反馈）；Pass 非内建，调用方走自定义展开/发送。
/// 纯逻辑（无终端依赖），可单测。
enum Builtin {
    Quit,
    Done(String),
    Compact(Option<String>),
    Resume(String),
    Pass,
}

/// 最近会话列表块：与 CLI `sessions` 同列（profile/id/时间/消息数），当前会话标 `*`。
/// 纯函数（可单测）；库不可用由调用方拦截，这里只管渲染。
pub(crate) fn sessions_block(
    store: &SessionStore,
    current: &str,
    limit: usize,
) -> anyhow::Result<String> {
    let rows = store.list_sessions(limit)?;
    if rows.is_empty() {
        return Ok("no sessions yet — chat or run to create one".into());
    }
    let mut out = String::from("recent sessions (`/resume <id|短前缀>` 切换):");
    for (id, profile, created, count) in rows {
        let mark = if id == current { "*" } else { " " };
        out.push_str(&format!(
            "\n{mark}[{profile}] {} {created} ({count} msgs)",
            &id[..8.min(id.len())]
        ));
    }
    Ok(out)
}

/// `/resume` 参数解析：完整 id 或唯一短前缀（与 `/goto` 短 id 同心智）。
/// 返回完整 id；未知/歧义/指回当前会话一律返回展示文案（调用方 Done）。
pub(crate) fn resolve_session_arg(
    store: &SessionStore,
    arg: &str,
    current: &str,
) -> Result<String, String> {
    if arg.is_empty() {
        return Err("[resume] usage: /resume <id|短前缀>（/sessions 查看）".into());
    }
    let rows = store
        .list_sessions(100)
        .map_err(|e| format!("[resume] list failed: {e:#}"))?;
    let hits: Vec<&String> = rows
        .iter()
        .map(|(id, _, _, _)| id)
        .filter(|id| *id == arg || id.starts_with(arg))
        .collect();
    match hits.as_slice() {
        [] => Err(format!("[resume] unknown session: {arg} (see `/sessions`)")),
        [one] if one.as_str() == current => Err("[resume] already on this session".into()),
        [one] => Ok((*one).clone()),
        _ => Err(format!(
            "[resume] ambiguous prefix `{arg}` ({} hits, see `/sessions`)",
            hits.len()
        )),
    }
}

/// 会话重建：按库行顺序回填全部节点 + 压缩摘要预热（与 CLI `restore_or_new` 同语义：
/// 有 `blocks` 的行按结构回填工具调用/结果，老库纯文本行退回文本；行 id 沿用库行 id）。
/// 返回 (树, 消息数)；空会话（存在但零消息）返回空树，调用方提示后可直接续聊。
pub(crate) fn replay_session(
    store: &SessionStore,
    id: &str,
) -> anyhow::Result<(SessionTree, usize)> {
    let msgs = store.session_records(id, 500)?;
    let mut s = SessionTree::new();
    for rec in msgs {
        s.push_with_id(rec.id.clone(), rec.to_message());
    }
    let n = s.history().len();
    let stored = store.get_summary(id).unwrap_or_default();
    if !stored.is_empty() {
        if let Some(first) = s.current_path.first().cloned() {
            s.summary = Some(stored);
            s.summary_through = Some(first);
        }
    }
    Ok((s, n))
}

#[allow(clippy::too_many_arguments)]
fn dispatch_builtin(
    text: &str,
    agent: &mut AgentLoop,
    session: &mut SessionTree,
    provider: &mut Arc<dyn LlmProvider>,
    skills: &SkillRegistry,
    command_dirs: &[std::path::PathBuf],
    session_id: &str,
    tools: &mut ToolRegistry,
    ext_set: Option<&mut rupi_ext::ExtensionSet>,
    sess_db: Option<&SessionStore>,
) -> Builtin {
    let t = text.trim();
    if t == "/quit" {
        return Builtin::Quit;
    }
    if t == "/skills" {
        // 空注册表给提示（与 CLI skills-list 同文案），不推空行进视图
        let block = skills.index_block();
        return Builtin::Done(if block.is_empty() {
            "no skills found. distill one with `skill-distill <name> <desc> <steps...>`".into()
        } else {
            block
        });
    }
    if t == "/commands" {
        let mut block = commands::index_block(command_dirs);
        if let Some(set) = ext_set.as_ref() {
            let extra = set.command_index();
            if !extra.is_empty() {
                block.push('\n');
                block.push_str(&extra);
            }
        }
        return Builtin::Done(block);
    }
    if t == "/tree" {
        return Builtin::Done(session.tree_view());
    }
    if t == "/sessions" {
        // 最近会话列表（与 CLI `sessions` 同列）；库不可用（单测/内嵌）给提示。
        return match sess_db {
            Some(db) => match sessions_block(db, session_id, 20) {
                Ok(block) => Builtin::Done(block),
                Err(e) => Builtin::Done(format!("[sessions] list failed: {e:#}")),
            },
            None => Builtin::Done("[sessions] no session store attached".into()),
        };
    }
    if t == "/resume" || t.starts_with("/resume ") {
        // 会话切换只做解析（返回完整 id），重建树/亲和头由调用方执行（与 /compact 同分工）。
        return match sess_db {
            None => Builtin::Done("[resume] no session store attached".into()),
            Some(db) => {
                let arg = t.strip_prefix("/resume").unwrap().trim();
                match resolve_session_arg(db, arg, session_id) {
                    Ok(id) => Builtin::Resume(id),
                    Err(msg) => Builtin::Done(msg),
                }
            }
        };
    }
    if t == "/goto" {
        // 裸 `/goto` 拦截给用法（与 REPL 同语义）；此前 Pass 会漏进模型白烧一轮。
        return Builtin::Done("[goto] usage: /goto <短id>（/tree 查看节点）".into());
    }
    if let Some(prefix) = t.strip_prefix("/goto ") {
        let prefix = prefix.trim();
        return match session.resolve_short_id(prefix) {
            Some(id) if session.goto_node(&id) => {
                Builtin::Done(format!("[goto {}]", &id[..8.min(id.len())]))
            }
            _ => Builtin::Done(format!("[goto] unknown or ambiguous node prefix: {prefix}")),
        };
    }
    // 以下内建与 REPL 同语义：拦截在先，绝不把命令文本发给模型
    if t == "/rewind" || t.starts_with("/rewind ") {
        let arg = t.strip_prefix("/rewind").unwrap().trim();
        if arg.is_empty() {
            return if session.current_path.len() >= 2 {
                let target = session.current_path[session.current_path.len() - 2].clone();
                session.rewind_to(&target);
                Builtin::Done("[rewound]".into())
            } else {
                Builtin::Done("[rewind] nothing to undo".into())
            };
        }
        return match session.resolve_short_id(arg) {
            Some(id) if session.rewind_to(&id) => {
                Builtin::Done(format!("[rewound {}]", &id[..8.min(id.len())]))
            }
            Some(_) => Builtin::Done("[rewind] node not on current path, use /goto".into()),
            None => Builtin::Done(format!(
                "[rewind] unknown or ambiguous node prefix: {arg}"
            )),
        };
    }
    if t == "/plan" {
        agent.plan_mode = !agent.plan_mode;
        return Builtin::Done(format!(
            "[plan mode {}]",
            if agent.plan_mode { "on" } else { "off" }
        ));
    }
    if t == "/thinking" || t.starts_with("/thinking ") {
        let arg = t.strip_prefix("/thinking").unwrap().trim();
        if arg.is_empty() {
            return match agent.thinking {
                Some(l) => Builtin::Done(format!("[thinking {l:?}]")),
                None => Builtin::Done("[thinking default (provider default)]".into()),
            };
        }
        return match arg.parse::<rupi_llm::ThinkingLevel>() {
            Ok(l) => {
                agent.thinking = Some(l);
                Builtin::Done(format!("[thinking switched to {l:?}]"))
            }
            Err(e) => Builtin::Done(format!("[thinking] {e:#}; staying on current")),
        };
    }
    if t == "/model" || t.starts_with("/model ") {
        let arg = t.strip_prefix("/model").unwrap().trim();
        if arg.is_empty() {
            return Builtin::Done(format!("[model {}]", provider.name()));
        }
        return match rupi_llm::provider_for_model(arg) {
            Ok(mut p) => {
                // 切换后回填亲和头：同会话 id 即同一下游（与 REPL /model 同语义）
                rupi_llm::apply_session_settings(&mut *p, Some(session_id));
                *provider = p.into();
                if let Some(t) = rupi_llm::parse_model_spec(arg).thinking {
                    agent.thinking = Some(t);
                }
                Builtin::Done(format!("[model switched to {arg}]"))
            }
            Err(e) => Builtin::Done(format!("[model] switch failed ({e:#})")),
        };
    }
    if t == "/reload" {
        // 热重载要 ExtensionSet 可变借用 + 可变工具表：内嵌/单测无 set 时回退提示。
        return match ext_set {
            Some(set) => {
                let lines = rupi_ext::refresh_extensions(tools, set);
                Builtin::Done(if lines.is_empty() {
                    "[ext] no changes".into()
                } else {
                    lines.join("\n")
                })
            }
            None => Builtin::Done(
                "[reload] no extension dir attached; restart TUI to pick up changes".into(),
            ),
        };
    }
    if t == "/compact" || t.starts_with("/compact ") {
        // 压实调模型是 async：这里只做标记（带自定义指令），run_loop 内 await 执行
        // （与 REPL /compact 同反馈文案）。
        let prompt = t
            .strip_prefix("/compact")
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_string);
        return Builtin::Compact(prompt);
    }
    Builtin::Pass
}

enum Control {
    Continue,
    Quit,
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    mut ctx: TuiContext<'_>,
) -> anyhow::Result<()> {
    let mut view = ChatView::default();
    view.push_system("rupi TUI — Enter 发送，Esc 中止本轮，运行中输入自动排队跟进，/quit 退出，/sessions 看会话，/resume <短id> 切换会话，/tree 看树，/goto <短id> 跳转，/rewind 回退，/compact 手动压实，/plan 计划模式，/thinking 思考强度，/model 切换模型，/skills 看技能，/commands 看自定义命令，PgUp/PgDn 滚动".into());
    let mut input = InputBuffer::default();
    let mut scroll: u16 = 0;
    let mut reader = EventStream::new();

    loop {
        // 斜杠补全候选：内建 + 自定义命令（小目录扫描，随输入更新；Enter 前 Tab 应用）。
        // 无斜杠候选时回退 @路径补全（root 取 current_dir，失败即无弹窗）。
        let mut custom_names: Vec<String> = commands::list(&ctx.command_dirs)
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        if let Some(set) = ctx.ext_set.as_ref() {
            custom_names.extend(set.list_commands().into_iter().map(|c| c.name));
        }
        let slash_completion = complete::candidates(&input.text(), &custom_names);
        let (completion, completion_prefix) = if slash_completion.is_empty() {
            let at = match std::env::current_dir() {
                Ok(cwd) => complete::at_candidates(&input.text(), input.cursor(), &cwd),
                Err(_) => Vec::new(),
            };
            (at, '@')
        } else {
            (slash_completion, '/')
        };
        draw(
            terminal,
            &view,
            &input,
            scroll,
            false,
            0,
            &completion,
            completion_prefix,
        )?;
        let Some(Ok(Event::Key(key))) = reader.next().await else {
            continue;
        };
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
            KeyCode::PageUp => scroll = scroll.saturating_add(5),
            KeyCode::PageDown => scroll = scroll.saturating_sub(5),
            KeyCode::Tab => {
                if let Some(done) = complete::apply_tab(&input.text(), &custom_names) {
                    input.set_text(&done);
                } else if let Ok(cwd) = std::env::current_dir() {
                    // 斜杠无命中时回退 @路径补全（行中 token，光标留在补全后）。
                    let text = input.text();
                    if let Some((done, cursor)) =
                        complete::apply_at_tab(&text, input.cursor(), &cwd)
                    {
                        input.set_text_and_cursor(&done, cursor);
                    }
                }
            }
            KeyCode::Left => input.move_left(),
            KeyCode::Right => input.move_right(),
            KeyCode::Backspace => input.backspace(),
            KeyCode::Char(c) => {
                scroll = 0;
                input.push_char(c);
            }
            KeyCode::Enter if !input.is_empty() => {
                // follow-up 自动跟进：drive_turn 带回运行中排队的输入，非空则直接作为
                // 下一轮发出（dispatch/展开/落盘全走同一路径；Quit 在内层直接返回）。
                // 内层 Done/Compact 的 continue 因 next 已空而等价于回到外循环读键。
                let mut next: Option<String> = Some(input.take());
                while let Some(text) = next.take() {
                    // MCP 工具热刷新：server 发 notifications/tools/list_changed 即重列差量更新
                    if let (Some(m), Some(rx)) = (ctx.mcp, ctx.mcp_rx.as_mut()) {
                        while let Ok(srv) = rx.try_recv() {
                            match m.refresh_server(&mut *ctx.tools, &srv).await {
                                Ok(added) if !added.is_empty() => view.push_system(format!(
                                    "[mcp] {srv} tools added: {}",
                                    added.join(", ")
                                )),
                                Ok(_) => view.push_system(format!("[mcp] {srv} tools updated")),
                                Err(e) => {
                                    view.push_system(format!("[mcp] refresh {srv} failed: {e:#}"))
                                }
                            }
                        }
                    }
                    // 锁守卫只活在派发语句内：守卫跨 await 会触发 await_holding_lock，
                    // 故先求值出 owned 的 Builtin 再 match（后续臂有 await）。
                    let builtin = {
                        let sess_guard = ctx.sess_db.as_ref().map(|db| db.lock().unwrap());
                        let sid = ctx.session_id.lock().unwrap().clone();
                        dispatch_builtin(
                            &text,
                            ctx.agent,
                            ctx.session,
                            ctx.provider,
                            ctx.skills,
                            &ctx.command_dirs,
                            &sid,
                            &mut *ctx.tools,
                            ctx.ext_set.as_deref_mut(),
                            sess_guard.as_deref(),
                        )
                    };
                    match builtin {
                        Builtin::Quit => return Ok(()),
                        Builtin::Done(msg) => {
                            view.push_system(msg);
                            continue;
                        }
                        Builtin::Resume(id) => {
                            // 会话切换：库重建树 → 换 id（落盘 cell 同写）→ 亲和头回填新 id。
                            let db = match ctx.sess_db.as_ref() {
                                Some(db) => db.clone(),
                                None => {
                                    view.push_system("[resume] no session store attached".into());
                                    continue;
                                }
                            };
                            let db = db.lock().unwrap();
                            match replay_session(&db, &id) {
                                Ok((tree, n)) => {
                                    *ctx.session = tree;
                                    *ctx.session_id.lock().unwrap() = id.clone();
                                    export_session_id(&id);
                                    // 亲和头回填新 id（/model 同语义：重建替换；
                                    // mock 等不可重建只降级提示，切换本身照常生效）。
                                    let model =
                                        ctx.provider.model_id().unwrap_or_default().to_string();
                                    match rupi_llm::provider_for_model(&model) {
                                        Ok(mut p) => {
                                            rupi_llm::apply_session_settings(&mut *p, Some(&id));
                                            *ctx.provider = p.into();
                                        }
                                        Err(e) => view.push_system(format!(
                                            "[resume] provider rebuild skipped ({e:#})"
                                        )),
                                    }
                                    view.push_system(format!(
                                        "[resumed {} ({} msgs)]",
                                        &id[..8.min(id.len())],
                                        n
                                    ));
                                }
                                Err(e) => {
                                    view.push_system(format!("[resume] switch failed: {e:#}"))
                                }
                            }
                            continue;
                        }
                        Builtin::Compact(prompt) => {
                            let before = ctx.session.summary.clone();
                            let buffered =
                                std::sync::Mutex::new(Vec::<rupi_core::AgentEvent>::new());
                            ctx.agent
                                .force_compress_with_prompt(
                                    &**ctx.provider,
                                    ctx.session,
                                    ctx.mem,
                                    &|e| {
                                        buffered.lock().unwrap().push(e);
                                    },
                                    prompt.as_deref(),
                                )
                                .await;
                            for e in buffered.lock().unwrap().drain(..) {
                                view.push_event(&e);
                            }
                            if ctx.session.summary != before && ctx.session.summary.is_some() {
                                view.push_system("[compacted]".into());
                            } else {
                                view.push_system("[compact] nothing to compress".into());
                            }
                            continue;
                        }
                        Builtin::Pass => {}
                    }
                    // 自定义斜杠命令：内建优先（上已 continue），命中则展开为提示词；
                    // 未命中再回退 skill 名（`/skillname args` 即调 skill）。
                    let slash =
                        commands::split(&text).map(|(n, a)| (n.to_owned(), a.to_owned()));
                    let mut send_text = text.clone();
                    if let Some((name, args)) = slash.as_ref() {
                        if let Some(expanded) = commands::expand(&ctx.command_dirs, name, args) {
                            view.push_system(format!("[command /{name}]"));
                            send_text = expanded;
                        } else if let Some(expanded) = ctx.skills.expand_as_command(name, args) {
                            view.push_system(format!("[skill /{name}]"));
                            send_text = expanded;
                        } else if let Some(set) = ctx.ext_set.as_ref() {
                            if let Some(expanded) = set.expand_command(name, args) {
                                view.push_system(format!("[ext /{name}]"));
                                send_text = expanded;
                            }
                        }
                    }
                    // @path 引用展开：斜杠展开之后、发送之前内联文件（图片走 Image 块）。
                    let user = if let Ok(cwd) = std::env::current_dir() {
                        Message::from_blocks(
                            Role::User,
                            commands::expand_at_mentions_blocks(&send_text, &cwd),
                        )
                    } else {
                        Message::text(Role::User, send_text.clone())
                    };
                    view.push_user(user.full_text());
                    scroll = 0;
                    // 发送前刷新 skill 注册表：上一轮蒸馏的新 skill 本轮即对模型可见
                    ctx.skills.refresh(&ctx.skill_dirs);
                    let ext_arcs: Vec<Arc<dyn Extension>> = ctx
                        .ext_set
                        .as_ref()
                        .map(|s| s.extension_arcs())
                        .unwrap_or_default();
                    let turn = drive_turn(
                        terminal,
                        ctx.agent,
                        &**ctx.provider,
                        ctx.session,
                        &*ctx.tools,
                        ctx.mem,
                        ctx.frozen,
                        ctx.skills,
                        &ctx.review_lines,
                        &ctx.on_turn,
                        &mut reader,
                        &mut view,
                        user,
                        &ext_arcs,
                    )
                    .await?;
                    if let Some(set) = ctx.ext_set.as_ref() {
                        for (source, hint) in set.drain_ui_hints() {
                            view.push_system(format!(
                                "[ui {source}/{}] {}",
                                hint.kind, hint.message
                            ));
                        }
                    }
                    match turn {
                        (Control::Continue, followup) => {
                            let q = followup.trim().to_string();
                            if q.is_empty() {
                                break;
                            }
                            view.push_system("[follow-up] queued input auto-sending".into());
                            next = Some(q);
                        }
                        (Control::Quit, _) => return Ok(()),
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// 回合内循环：agent future、键盘、事件 channel 同场 `select!`，delta 到达即画。
/// `on_event` 走 `tokio::sync::mpsc`（不再用 `std::sync::mpsc`）：后者不唤醒 async
/// runtime，内循环只会在按键或整轮结束时重绘，流式名存实亡。
/// 运行中继续打字排队为 follow-up（`collect_followup`），返回 `(Control, followup)`：
/// 调用方在 `Continue` 且排队非空时自动发出下一轮（对标上游 follow-up 全量注入）。
/// `terminal` / `reader` 泛型：生产走 Crossterm + `EventStream`，单测走 `TestBackend`
/// + pending 键流，断言无按键时 delta 也会 `draw`。
#[allow(clippy::too_many_arguments)]
async fn drive_turn<B, S>(
    terminal: &mut Terminal<B>,
    agent: &AgentLoop,
    provider: &dyn LlmProvider,
    session: &mut SessionTree,
    tools: &ToolRegistry,
    mem: &MemoryManager,
    frozen: &FrozenMemory,
    skills: &SkillRegistry,
    review_lines: &Option<Arc<std::sync::Mutex<Vec<String>>>>,
    on_turn: &Option<Arc<dyn Fn(TurnRecord) + Send + Sync>>,
    reader: &mut S,
    view: &mut ChatView,
    user: Message,
    extensions: &[Arc<dyn Extension>],
) -> anyhow::Result<(Control, String)>
where
    B: Backend,
    S: Stream<Item = std::io::Result<Event>> + Unpin,
{
    let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
    let on_event = move |e: AgentEvent| {
        let _ = tx.send(e);
    };
    // 本轮前路径长度：用户节点即 current_path[before]，落盘沿用其 id（resume 短 id 稳定）
    let before = session.current_path.len();
    let user_text = user.full_text();
    // 协作取消：内循环 Esc 置位，主循环在检查点优雅中止（TurnEnd/RunEnd{Aborted} 照常走事件通道）。
    let cancel = rupi_core::CancelFlag::new();
    let fut = agent.run_with_user(
        provider,
        session,
        user,
        tools,
        mem,
        frozen,
        skills,
        extensions,
        &on_event,
        &cancel,
    );
    let mut scroll: u16 = 0;
    // 运行中输入的排队缓冲：输入框实时回显（qb 镜像），结束自动跟进
    let mut followup = String::new();
    let mut qb = InputBuffer::default();
    // 内循环只负责驱动 + 渲染；pin 守卫连同 fut 一起终结于块内，之后才能再读 session
    enum End {
        Finished(anyhow::Result<rupi_core::StopReason>),
        Quit,
    }
    let end: End = {
        tokio::pin!(fut);
        loop {
            flush_turn_view(&mut rx, review_lines, view);
            paint_busy(terminal, view, &qb, scroll, &followup)?;
            tokio::select! {
                biased;
                res = &mut fut => {
                    flush_turn_view(&mut rx, review_lines, view);
                    // 收尾再画一帧：与 fut 竞速的最后几个 delta 也落到 TestBackend / 屏幕上。
                    paint_busy(terminal, view, &qb, scroll, &followup)?;
                    break End::Finished(res);
                }
                maybe_key = reader.next() => {
                    if let Some(Ok(Event::Key(key))) = maybe_key {
                        if collect_followup(&mut followup, &key) {
                            qb.set_text(&followup);
                        } else {
                            match key.code {
                                KeyCode::Char('c')
                                    if key.modifiers.contains(KeyModifiers::CONTROL) =>
                                {
                                    break End::Quit;
                                }
                                // 中止本轮：只置位不 drop future，会话停在一致点，
                                // TurnEnd/RunEnd{Aborted} 照常进视图。排队保留，
                                // 中止后自动跑排队≈转向。
                                KeyCode::Esc => {
                                    cancel.cancel();
                                }
                                KeyCode::PageUp => scroll = scroll.saturating_add(5),
                                KeyCode::PageDown => scroll = scroll.saturating_sub(5),
                                _ => {}
                            }
                        }
                    }
                }
                // delta / 工具事件到达即醒：下一圈 flush + draw，不必等按键或整轮结束。
                // `Some(e) =` 在发送端随 future 关闭后禁用此臂，避免 recv() 空转。
                Some(e) = rx.recv() => {
                    view.push_event(&e);
                    flush_turn_view(&mut rx, review_lines, view);
                }
            }
        }
    };
    match end {
        End::Quit => Ok((Control::Quit, String::new())),
        End::Finished(res) => {
            if matches!(res, Ok(rupi_core::StopReason::Aborted)) {
                view.push_system("[aborted]".into());
            }
            match res {
                Ok(_) => {
                    let (assistant, assistant_node) = session
                        .current_path
                        .iter()
                        .skip(before)
                        .filter_map(|id| session.nodes.get(id))
                        .filter(|n| n.message.role == rupi_core::Role::Assistant)
                        .last()
                        .map(|n| (n.message.full_text(), Some(n.id.clone())))
                        .unwrap_or_default();
                    let user_node = session.current_path.get(before).cloned();
                    let messages: Vec<(String, Message)> = session
                        .current_path
                        .iter()
                        .skip(before)
                        .filter_map(|id| session.nodes.get(id))
                        .map(|n| (n.id.clone(), n.message.clone()))
                        .collect();
                    if let Some(cb) = on_turn {
                        cb(TurnRecord {
                            user: user_text.clone(),
                            assistant,
                            summary: session.summary.clone(),
                            user_node,
                            assistant_node,
                            messages,
                        });
                    }
                }
                Err(e) => {
                    // 回滚本轮已入树节点（与 REPL 同语义）：失败的一轮不留半截历史
                    for id in session.current_path.split_off(before) {
                        session.nodes.remove(&id);
                    }
                    view.push_system(format!("turn failed: {e:#}"));
                }
            }
            Ok((Control::Continue, followup))
        }
    }
}

fn flush_turn_view(
    rx: &mut mpsc::UnboundedReceiver<AgentEvent>,
    review_lines: &Option<Arc<std::sync::Mutex<Vec<String>>>>,
    view: &mut ChatView,
) {
    while let Ok(e) = rx.try_recv() {
        view.push_event(&e);
    }
    if let Some(buf) = review_lines {
        for line in buf.lock().unwrap().drain(..) {
            view.push_system(line);
        }
    }
}

fn paint_busy<B: Backend>(
    terminal: &mut Terminal<B>,
    view: &ChatView,
    input: &InputBuffer,
    scroll: u16,
    followup: &str,
) -> anyhow::Result<()> {
    draw(
        terminal,
        view,
        input,
        scroll,
        true,
        followup.chars().count(),
        &[],
        '/',
    )
}

/// 全屏绘制（Backend 泛型：生产走 Crossterm，单测走 TestBackend 真画一遍断言像素行）。
#[allow(clippy::too_many_arguments)]
fn draw<B: Backend>(
    terminal: &mut Terminal<B>,
    view: &ChatView,
    input: &InputBuffer,
    scroll: u16,
    busy: bool,
    queued: usize,
    completion: &[String],
    completion_prefix: char,
) -> anyhow::Result<()> {
    terminal
        .draw(|f| {
            let area = f.area();
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(1),
                    Constraint::Length(3),
                    Constraint::Length(1),
                ])
                .split(area);
            let lines: Vec<RLine> = view.lines.iter().map(render_line).collect();
            let total = lines.len() as u16;
            let visible = chunks[0].height as usize;
            let start = total.saturating_sub(visible as u16 + scroll) as usize;
            let msgs = Paragraph::new(lines[start..].to_vec())
                .block(Block::default().borders(Borders::ALL).title("rupi"))
                .wrap(Wrap { trim: false });
            f.render_widget(msgs, chunks[0]);
            let (pre, post) = input.split_for_render();
            let prompt = Paragraph::new(RLine::from(vec![
                Span::styled("> ", Style::default().fg(Color::Green)),
                Span::raw(pre.clone()),
                Span::raw(post),
            ]))
            .block(Block::default().borders(Borders::ALL));
            f.render_widget(prompt, chunks[1]);
            // 补全弹窗：输入框上方浮层，多候选时展示（Tab 补全/公共前缀）
            if !completion.is_empty() {
                let shown: Vec<RLine> = completion
                    .iter()
                    .take(8)
                    .map(|c| {
                        RLine::from(vec![Span::styled(
                            format!("{completion_prefix}{c}"),
                            Style::default().fg(Color::Yellow),
                        )])
                    })
                    .collect();
                let extra = completion.len().saturating_sub(8);
                let mut items = shown;
                if extra > 0 {
                    items.push(RLine::from(format!("… +{extra} more")));
                }
                let height = (items.len() as u16 + 2).min(chunks[0].height.max(1));
                let area = Rect {
                    x: chunks[1].x,
                    y: chunks[1].y.saturating_sub(height),
                    width: chunks[1].width,
                    height,
                };
                f.render_widget(Clear, area);
                f.render_widget(
                    Paragraph::new(items)
                        .block(Block::default().borders(Borders::ALL).title("Tab 补全")),
                    area,
                );
            }
            f.render_widget(
                Paragraph::new(if busy {
                    if queued > 0 {
                        format!("… thinking ({queued} queued · Esc 中断并转向排队 · Ctrl-C 退出)")
                    } else {
                        "… thinking (Esc 中断本轮 · Ctrl-C 退出)".to_string()
                    }
                } else {
                    "ready".to_string()
                }),
                chunks[2],
            );
            f.set_cursor_position((
                chunks[1].x + 3 + pre.chars().count() as u16,
                chunks[1].y + 1,
            ));
        })
        .context("draw TUI")?;
    Ok(())
}

fn render_line(l: &Line) -> RLine<'static> {
    match l {
        Line::User(t) => RLine::from(vec![
            Span::styled("you: ", Style::default().fg(Color::Cyan)),
            Span::raw(t.clone()),
        ]),
        Line::AssistantText(t) => RLine::from(Span::raw(t.clone())),
        Line::Tool(t) => RLine::from(vec![Span::styled(
            t.clone(),
            Style::default().fg(Color::Yellow),
        )]),
        Line::System(t) => RLine::from(vec![Span::styled(
            t.clone(),
            Style::default().fg(Color::DarkGray),
        )]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rupi_core::{Message, Role};

    fn harness() -> (AgentLoop, SessionTree, Arc<dyn LlmProvider>, SkillRegistry) {
        (
            AgentLoop::new(3),
            SessionTree::new(),
            Arc::new(rupi_llm::MockProvider::new(vec![])),
            SkillRegistry::default(),
        )
    }

    fn done_text(b: Builtin) -> String {
        match b {
            Builtin::Done(s) => s,
            _ => panic!("expected Done, got other"),
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    #[test]
    fn followup_collects_text_enter_backspace() {
        // 运行中打字排队：字符追加、Enter 换行、Backspace 删除
        let mut buf = String::new();
        assert!(collect_followup(&mut buf, &key(KeyCode::Char('h'))));
        assert!(collect_followup(&mut buf, &key(KeyCode::Char('i'))));
        assert!(collect_followup(&mut buf, &key(KeyCode::Enter)));
        assert_eq!(buf, "hi\n");
        assert!(collect_followup(&mut buf, &key(KeyCode::Backspace)));
        assert!(collect_followup(&mut buf, &key(KeyCode::Backspace)));
        assert_eq!(buf, "h");
    }

    #[test]
    fn followup_leaves_control_keys_to_caller() {
        // Esc/Ctrl-C/功能键不消费，留给调用方的取消/退出/滚动分支
        let mut buf = String::new();
        assert!(!collect_followup(&mut buf, &key(KeyCode::Esc)));
        assert!(!collect_followup(
            &mut buf,
            &KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
        ));
        assert!(!collect_followup(&mut buf, &key(KeyCode::PageUp)));
        assert!(!collect_followup(&mut buf, &key(KeyCode::Tab)));
        assert!(buf.is_empty());
        // 空缓冲退格不 panic
        assert!(collect_followup(&mut buf, &key(KeyCode::Backspace)));
        assert!(buf.is_empty());
    }

    #[test]
    fn draw_renders_messages_input_completion_and_status() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut view = ChatView::default();
        view.push_user("hello".into());
        view.push_event(&rupi_core::AgentEvent::TextDelta {
            delta: "world".into(),
        });
        let mut input = InputBuffer::default();
        for c in "he".chars() {
            input.push_char(c);
        }
        let completion = vec!["help".to_string(), "history".to_string()];
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        draw(&mut terminal, &view, &input, 0, false, 0, &completion, '/').unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        for want in [
            "rupi",
            "you: hello",
            "world",
            "> he",
            "/help",
            "/history",
            "Tab",
            "ready",
        ] {
            assert!(screen.contains(want), "缺 `{want}`:\n{screen}");
        }
        // 光标：输入框行首 x+3（"> " 后）+ 字符数，"he" → x=5；输入框 y=8 → 光标 y=9
        let pos = terminal.backend_mut().get_cursor_position().unwrap();
        assert_eq!((pos.x, pos.y), (5, 9), "光标位置不对");
    }

    #[test]
    fn draw_at_completion_uses_at_prefix() {
        // @路径候选弹窗以前缀 @ 展示（与斜杠 / 区分）。
        use ratatui::{backend::TestBackend, Terminal};
        let view = ChatView::default();
        let input = InputBuffer::default();
        let completion = vec!["src/main.rs".to_string()];
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        draw(&mut terminal, &view, &input, 0, false, 0, &completion, '@').unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(screen.contains("@src/main.rs"), "缺 @ 候选:\n{screen}");
    }

    #[test]
    fn draw_busy_status_hints_steer_and_quit() {
        // 运行中状态栏即 steering 说明书：排队时提示 Esc 中断并转向，无排队只提示中断。
        use ratatui::{backend::TestBackend, Terminal};
        let view = ChatView::default();
        let input = InputBuffer::default();
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        draw(&mut terminal, &view, &input, 0, true, 2, &[], '/').unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(screen.contains("2 queued"), "缺排队数:\n{screen}");
        assert!(screen.contains("Esc"), "缺转向提示:\n{screen}");
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        draw(&mut terminal, &view, &input, 0, true, 0, &[], '/').unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(screen.contains("Esc"), "缺中断提示:\n{screen}");
        assert!(!screen.contains("queued"), "无排队不应提 queued:\n{screen}");
    }

    /// 录屏后端：每次 `draw` 记下像素行；若已含 marker 则叫醒 provider，
    /// 证明重绘发生在 agent future 结束之前、且没有按键。
    struct StreamingTestBackend {
        inner: ratatui::backend::TestBackend,
        marker: String,
        painted: Arc<tokio::sync::Notify>,
        frames: Arc<Mutex<Vec<String>>>,
    }

    impl ratatui::backend::Backend for StreamingTestBackend {
        fn draw<'a, I>(&mut self, content: I) -> std::io::Result<()>
        where
            I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
        {
            self.inner.draw(content)?;
            let screen: String = self
                .inner
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect();
            if screen.contains(&self.marker) {
                self.painted.notify_waiters();
            }
            self.frames.lock().unwrap().push(screen);
            Ok(())
        }
        fn hide_cursor(&mut self) -> std::io::Result<()> {
            self.inner.hide_cursor()
        }
        fn show_cursor(&mut self) -> std::io::Result<()> {
            self.inner.show_cursor()
        }
        fn get_cursor_position(&mut self) -> std::io::Result<ratatui::layout::Position> {
            self.inner.get_cursor_position()
        }
        fn set_cursor_position<P: Into<ratatui::layout::Position>>(
            &mut self,
            position: P,
        ) -> std::io::Result<()> {
            self.inner.set_cursor_position(position)
        }
        fn clear(&mut self) -> std::io::Result<()> {
            self.inner.clear()
        }
        fn size(&self) -> std::io::Result<ratatui::layout::Size> {
            self.inner.size()
        }
        fn window_size(&mut self) -> std::io::Result<ratatui::backend::WindowSize> {
            self.inner.window_size()
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.inner.flush()
        }
    }

    /// 先推一个独特 delta，再挂起等 TUI 画到该 marker；若 `select!` 没有
    /// `rx.recv()` 臂，这里会超时，整轮失败。
    struct SlowDeltaProvider {
        marker: String,
        painted: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for SlowDeltaProvider {
        fn name(&self) -> &str {
            "slow-delta"
        }
        async fn complete(
            &self,
            _req: rupi_llm::ChatRequest,
        ) -> anyhow::Result<rupi_llm::ChatResponse> {
            Ok(rupi_llm::MockProvider::text_response(&self.marker))
        }
        async fn complete_streaming(
            &self,
            req: rupi_llm::ChatRequest,
            tx: mpsc::Sender<rupi_llm::StreamEvent>,
        ) -> anyhow::Result<rupi_llm::ChatResponse> {
            let resp = self.complete(req).await?;
            let _ = tx
                .send(rupi_llm::StreamEvent::TextDelta(self.marker.clone()))
                .await;
            tokio::time::timeout(std::time::Duration::from_secs(1), self.painted.notified())
                .await
                .map_err(|_| {
                    anyhow::anyhow!("TUI did not redraw after TextDelta without a keypress")
                })?;
            Ok(resp)
        }
    }

    #[tokio::test]
    async fn drive_turn_redraws_delta_without_keypress() {
        // 键流永远 pending：若重绘只靠按键，marker 不会在 future 结束前出现。
        let marker = "STREAM_DELTA_OK";
        let painted = Arc::new(tokio::sync::Notify::new());
        let frames = Arc::new(Mutex::new(Vec::<String>::new()));
        let backend = StreamingTestBackend {
            inner: ratatui::backend::TestBackend::new(40, 12),
            marker: marker.to_string(),
            painted: painted.clone(),
            frames: frames.clone(),
        };
        let mut terminal = Terminal::new(backend).unwrap();
        let mut reader = futures::stream::pending::<std::io::Result<Event>>();
        let provider = SlowDeltaProvider {
            marker: marker.to_string(),
            painted,
        };
        let agent = AgentLoop::new(3);
        let mut session = SessionTree::new();
        let tools = ToolRegistry::default();
        let home = std::env::temp_dir().join(format!(
            "rupi-tui-stream-redraw-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mem = MemoryManager::new(rupi_memory::MemoryStore::new(home.clone()));
        let frozen = FrozenMemory::default();
        let skills = SkillRegistry::default();
        let mut view = ChatView::default();
        view.push_user("ping".into());
        let (ctrl, followup) = drive_turn(
            &mut terminal,
            &agent,
            &provider,
            &mut session,
            &tools,
            &mem,
            &frozen,
            &skills,
            &None,
            &None,
            &mut reader,
            &mut view,
            Message::text(Role::User, "ping"),
            &[],
        )
        .await
        .unwrap();
        let _ = std::fs::remove_dir_all(&home);
        assert!(matches!(ctrl, Control::Continue), "回合应正常结束");
        assert!(followup.is_empty());
        assert!(
            !view
                .lines
                .iter()
                .any(|l| matches!(l, Line::System(s) if s.contains("turn failed"))),
            "回合失败: {:?}",
            view.lines
        );
        let frames = frames.lock().unwrap();
        assert!(
            frames.iter().any(|s| s.contains(marker)),
            "TestBackend 应在无按键时画出 delta:\n{}",
            frames.last().cloned().unwrap_or_default()
        );
        // provider 在 complete_streaming 返回前就等到了含 marker 的帧，
        // 因此至少有一帧发生在整轮结束的收尾 draw 之前。
        let first_hit = frames.iter().position(|s| s.contains(marker)).unwrap();
        assert!(
            first_hit + 1 < frames.len(),
            "delta 应在流式过程中入画，而不是只靠收尾那一帧；first_hit={first_hit} frames={}",
            frames.len()
        );
    }

    #[test]
    fn quit_pass_and_plain_text() {
        let (mut agent, mut session, mut provider, skills) = harness();
        assert!(matches!(
            dispatch_builtin(
                "/quit",
                &mut agent,
                &mut session,
                &mut provider,
                &skills,
                &[],
                "t-sess",
                &mut ToolRegistry::default(),
                None,
                None,
            ),
            Builtin::Quit
        ));
        assert!(matches!(
            dispatch_builtin(
                "hello",
                &mut agent,
                &mut session,
                &mut provider,
                &skills,
                &[],
                "t-sess",
                &mut ToolRegistry::default(),
                None,
                None,
            ),
            Builtin::Pass
        ));
        // 裸 `/goto` 拦截给用法（与 REPL 同语义，不再 Pass 漏进模型）
        let msg = done_text(dispatch_builtin(
            "/goto",
            &mut agent,
            &mut session,
            &mut provider,
            &skills,
            &[],
            "t-sess",
            &mut ToolRegistry::default(),
            None,
            None,
        ));
        assert!(msg.contains("usage:"), "{msg}");
    }

    #[test]
    fn plan_toggles_and_persists_on_agent() {
        let (mut agent, mut session, mut provider, skills) = harness();
        assert_eq!(
            done_text(dispatch_builtin(
                "/plan",
                &mut agent,
                &mut session,
                &mut provider,
                &skills,
                &[],
                "t-sess",
                &mut ToolRegistry::default(),
                None,
                None,
            )),
            "[plan mode on]"
        );
        assert!(agent.plan_mode);
        assert_eq!(
            done_text(dispatch_builtin(
                "/plan",
                &mut agent,
                &mut session,
                &mut provider,
                &skills,
                &[],
                "t-sess",
                &mut ToolRegistry::default(),
                None,
                None,
            )),
            "[plan mode off]"
        );
        assert!(!agent.plan_mode);
    }

    #[test]
    fn thinking_show_set_and_reject() {
        let (mut agent, mut session, mut provider, skills) = harness();
        assert_eq!(
            done_text(dispatch_builtin(
                "/thinking",
                &mut agent,
                &mut session,
                &mut provider,
                &skills,
                &[],
                "t-sess",
                &mut ToolRegistry::default(),
                None,
                None,
            )),
            "[thinking default (provider default)]"
        );
        assert_eq!(
            done_text(dispatch_builtin(
                "/thinking high",
                &mut agent,
                &mut session,
                &mut provider,
                &skills,
                &[],
                "t-sess",
                &mut ToolRegistry::default(),
                None,
                None,
            )),
            "[thinking switched to High]"
        );
        assert_eq!(agent.thinking, Some(rupi_llm::ThinkingLevel::High));
        assert_eq!(
            done_text(dispatch_builtin(
                "/thinking",
                &mut agent,
                &mut session,
                &mut provider,
                &skills,
                &[],
                "t-sess",
                &mut ToolRegistry::default(),
                None,
                None,
            )),
            "[thinking High]"
        );
        let msg = done_text(dispatch_builtin(
            "/thinking ultra",
            &mut agent,
            &mut session,
            &mut provider,
            &skills,
            &[],
            "t-sess",
            &mut ToolRegistry::default(),
            None,
            None,
        ));
        assert!(msg.contains("staying on current"), "{msg}");
        assert_eq!(agent.thinking, Some(rupi_llm::ThinkingLevel::High));
    }

    #[test]
    fn model_show_and_failed_switch_stays() {
        let (mut agent, mut session, mut provider, skills) = harness();
        assert_eq!(
            done_text(dispatch_builtin(
                "/model",
                &mut agent,
                &mut session,
                &mut provider,
                &skills,
                &[],
                "t-sess",
                &mut ToolRegistry::default(),
                None,
                None,
            )),
            "[model mock]"
        );
        // hermetic：清空 key，保证 gpt 路由走缺 key 失败分支
        let saved: Vec<(&str, Option<String>)> = ["RUPI_API_KEY", "OPENAI_API_KEY"]
            .iter()
            .map(|k| (*k, std::env::var(k).ok()))
            .collect();
        for (k, _) in &saved {
            unsafe { std::env::remove_var(k) };
        }
        let msg = done_text(dispatch_builtin(
            "/model gpt-4o-mini",
            &mut agent,
            &mut session,
            &mut provider,
            &skills,
            &[],
            "t-sess",
            &mut ToolRegistry::default(),
            None,
            None,
        ));
        for (k, v) in saved {
            if let Some(val) = v {
                unsafe { std::env::set_var(k, val) };
            }
        }
        assert!(msg.contains("switch failed"), "{msg}");
        assert_eq!(provider.name(), "mock");
    }

    #[test]
    fn rewind_needs_two_nodes() {
        let (mut agent, mut session, mut provider, skills) = harness();
        assert_eq!(
            done_text(dispatch_builtin(
                "/rewind",
                &mut agent,
                &mut session,
                &mut provider,
                &skills,
                &[],
                "t-sess",
                &mut ToolRegistry::default(),
                None,
                None,
            )),
            "[rewind] nothing to undo"
        );
        session.push(Message::text(Role::User, "one"));
        session.push(Message::text(Role::Assistant, "two"));
        assert_eq!(
            done_text(dispatch_builtin(
                "/rewind",
                &mut agent,
                &mut session,
                &mut provider,
                &skills,
                &[],
                "t-sess",
                &mut ToolRegistry::default(),
                None,
                None,
            )),
            "[rewound]"
        );
        assert!(session.current_path.len() < 3);
        // 带参回退到指定节点：短 id 命中即截断；未知/歧义/跨分支各有反馈。
        let target = session.current_path[0].clone();
        let short = &target[..8.min(target.len())];
        assert_eq!(
            done_text(dispatch_builtin(
                &format!("/rewind {short}"),
                &mut agent,
                &mut session,
                &mut provider,
                &skills,
                &[],
                "t-sess",
                &mut ToolRegistry::default(),
                None,
                None,
            )),
            format!("[rewound {short}]")
        );
        assert_eq!(session.current_path.len(), 1);
        let msg = done_text(dispatch_builtin(
            "/rewind zzz",
            &mut agent,
            &mut session,
            &mut provider,
            &skills,
            &[],
            "t-sess",
            &mut ToolRegistry::default(),
            None,
            None,
        ));
        assert!(msg.contains("unknown or ambiguous"), "{msg}");
    }

    #[test]
    fn compact_maps_to_marker_not_model() {
        // /compact 只做标记（async 压实由 run_loop 执行），绝不漏进模型；
        // 带参版本透传自定义指令。
        let (mut agent, mut session, mut provider, skills) = harness();
        assert!(matches!(
            dispatch_builtin(
                "/compact",
                &mut agent,
                &mut session,
                &mut provider,
                &skills,
                &[],
                "t-sess",
                &mut ToolRegistry::default(),
                None,
                None,
            ),
            Builtin::Compact(None)
        ));
        assert!(matches!(
            dispatch_builtin(
                "/compact focus on file list",
                &mut agent,
                &mut session,
                &mut provider,
                &skills,
                &[],
                "t-sess",
                &mut ToolRegistry::default(),
                None,
                None,
            ),
            Builtin::Compact(Some(_))
        ));
    }

    #[test]
    fn reload_hot_reloads_extensions_and_goto_unknown() {
        // /reload 有 set 时真热重载（新增 manifest 即注册），无 set 回退提示。
        let (mut agent, mut session, mut provider, skills) = harness();
        let dir = std::env::temp_dir().join(format!("rupi-tui-reload-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut tools = ToolRegistry::default();
        let mut set = rupi_ext::ExtensionSet::new(dir.clone());
        // 空目录：无变化反馈
        let msg = done_text(dispatch_builtin(
            "/reload",
            &mut agent,
            &mut session,
            &mut provider,
            &skills,
            &[],
            "t-sess",
            &mut tools,
            Some(&mut set),
            None,
        ));
        assert!(msg.contains("no changes"), "{msg}");
        // 新增 manifest：重载注册为工具
        std::fs::write(
            dir.join("echo.json"),
            r#"{"name": "tui-echo", "description": "x", "input_schema": {}, "command": "sh"}"#,
        )
        .unwrap();
        let msg = done_text(dispatch_builtin(
            "/reload",
            &mut agent,
            &mut session,
            &mut provider,
            &skills,
            &[],
            "t-sess",
            &mut tools,
            Some(&mut set),
            None,
        ));
        assert!(msg.contains("tui-echo"), "{msg}");
        assert!(tools.definitions().iter().any(|d| d.name == "tui-echo"));
        let _ = std::fs::remove_dir_all(&dir);
        // 无 set 回退提示
        let msg = done_text(dispatch_builtin(
            "/reload",
            &mut agent,
            &mut session,
            &mut provider,
            &skills,
            &[],
            "t-sess",
            &mut ToolRegistry::default(),
            None,
            None,
        ));
        assert!(msg.contains("no extension dir"), "{msg}");
        let msg = done_text(dispatch_builtin(
            "/goto zzz",
            &mut agent,
            &mut session,
            &mut provider,
            &skills,
            &[],
            "t-sess",
            &mut ToolRegistry::default(),
            None,
            None,
        ));
        assert!(msg.contains("unknown or ambiguous"), "{msg}");
    }

    #[test]
    fn read_only_views_never_fall_through_to_model() {
        use rupi_core::Message;
        let (mut agent, mut session, mut provider, skills) = harness();
        // 空技能注册表给提示而非空行（与 CLI 同文案）
        let msg = done_text(dispatch_builtin(
            "/skills",
            &mut agent,
            &mut session,
            &mut provider,
            &skills,
            &[],
            "t-sess",
            &mut ToolRegistry::default(),
            None,
            None,
        ));
        assert!(msg.contains("no skills found"), "{msg}");
        // 空命令目录给指引
        let msg = done_text(dispatch_builtin(
            "/commands",
            &mut agent,
            &mut session,
            &mut provider,
            &skills,
            &[],
            "t-sess",
            &mut ToolRegistry::default(),
            None,
            None,
        ));
        assert!(msg.contains("no custom commands"), "{msg}");
        // 空树也有视图（不空返回、不漏进模型）
        let msg = done_text(dispatch_builtin(
            "/tree",
            &mut agent,
            &mut session,
            &mut provider,
            &skills,
            &[],
            "t-sess",
            &mut ToolRegistry::default(),
            None,
            None,
        ));
        assert!(!msg.is_empty(), "空树视图不应为空");
        session.push(Message::text(rupi_core::Role::User, "hi"));
        let msg2 = done_text(dispatch_builtin(
            "/tree",
            &mut agent,
            &mut session,
            &mut provider,
            &skills,
            &[],
            "t-sess",
            &mut ToolRegistry::default(),
            None,
            None,
        ));
        assert_ne!(msg, msg2, "有节点后视图应变化");
    }

    fn sess_home(tag: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!("rupi-tui-sess-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        base
    }

    fn seed_session(store: &SessionStore, profile: &str) -> String {
        let id = store.create_session(profile).unwrap();
        store.add_message(&id, "user", "hello").unwrap();
        store
            .add_message_with_id("node-assistant-1", &id, "assistant", "hi there")
            .unwrap();
        id
    }

    #[test]
    fn sessions_block_lists_and_marks_current() {
        let home = sess_home("list");
        let store = SessionStore::open(&home).unwrap();
        assert!(sessions_block(&store, "none", 20)
            .unwrap()
            .contains("no sessions yet"));
        let id = seed_session(&store, "work");
        let block = sessions_block(&store, &id, 20).unwrap();
        assert!(block.contains(&id[..8]), "{block}");
        assert!(block.contains("*[work]"), "{block}");
        assert!(block.contains("/resume"), "{block}");
        // 非当前会话无星标
        let other = sessions_block(&store, "other", 20).unwrap();
        assert!(!other.contains('*'), "{other}");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn resolve_session_arg_accepts_full_and_prefix() {
        let home = sess_home("resolve");
        let store = SessionStore::open(&home).unwrap();
        let id = seed_session(&store, "work");
        assert_eq!(resolve_session_arg(&store, &id, "other").unwrap(), id);
        assert_eq!(resolve_session_arg(&store, &id[..12], "other").unwrap(), id);
        // 裸命令给用法，未知 id 指引 /sessions，指回当前拒绝
        assert!(resolve_session_arg(&store, "", "other")
            .unwrap_err()
            .contains("usage"));
        assert!(resolve_session_arg(&store, "no-such", "other")
            .unwrap_err()
            .contains("unknown session"));
        assert!(resolve_session_arg(&store, &id, &id)
            .unwrap_err()
            .contains("already"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn replay_session_restores_transcript_and_summary() {
        let home = sess_home("replay");
        let store = SessionStore::open(&home).unwrap();
        let id = seed_session(&store, "work");
        store.set_summary(&id, "talked about tea").unwrap();
        let (tree, n) = replay_session(&store, &id).unwrap();
        assert_eq!(n, 2);
        assert_eq!(tree.history().len(), 2);
        assert_eq!(tree.summary.as_deref(), Some("talked about tea"));
        assert!(tree.summary_through.is_some());
        // 空会话返回空树（存在但零消息可直接续聊）
        let empty = store.create_session("empty").unwrap();
        let (tree, n) = replay_session(&store, &empty).unwrap();
        assert_eq!(n, 0);
        assert!(tree.history().is_empty());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn sessions_and_resume_dispatch_without_store_hint() {
        let (mut agent, mut session, mut provider, skills) = harness();
        let msg = done_text(dispatch_builtin(
            "/sessions",
            &mut agent,
            &mut session,
            &mut provider,
            &skills,
            &[],
            "t-sess",
            &mut ToolRegistry::default(),
            None,
            None,
        ));
        assert!(msg.contains("no session store"), "{msg}");
        let msg = done_text(dispatch_builtin(
            "/resume abc",
            &mut agent,
            &mut session,
            &mut provider,
            &skills,
            &[],
            "t-sess",
            &mut ToolRegistry::default(),
            None,
            None,
        ));
        assert!(msg.contains("no session store"), "{msg}");
    }

    #[test]
    fn resume_dispatch_resolves_to_resume_variant() {
        let home = sess_home("dispatch");
        let store = SessionStore::open(&home).unwrap();
        let id = seed_session(&store, "work");
        let (mut agent, mut session, mut provider, skills) = harness();
        // 完整 id → Resume 变体（调用方重建，不经模型）
        assert!(matches!(
            dispatch_builtin(
                &format!("/resume {id}"),
                &mut agent,
                &mut session,
                &mut provider,
                &skills,
                &[],
                "other",
                &mut ToolRegistry::default(),
                None,
                Some(&store),
            ),
            Builtin::Resume(got) if got == id
        ));
        // 裸 /resume 给用法，未知 id 指引 /sessions
        let msg = done_text(dispatch_builtin(
            "/resume",
            &mut agent,
            &mut session,
            &mut provider,
            &skills,
            &[],
            "other",
            &mut ToolRegistry::default(),
            None,
            Some(&store),
        ));
        assert!(msg.contains("usage"), "{msg}");
        let msg = done_text(dispatch_builtin(
            "/resume no-such",
            &mut agent,
            &mut session,
            &mut provider,
            &skills,
            &[],
            "other",
            &mut ToolRegistry::default(),
            None,
            Some(&store),
        ));
        assert!(msg.contains("unknown session"), "{msg}");
        let _ = std::fs::remove_dir_all(&home);
    }
}
