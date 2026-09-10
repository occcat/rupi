//! ratatui 终端主循环：消息区 + 输入框 + 状态栏。
//! 并发模型：空闲外循环收键；提交后进内循环，用 `select!` 同时驱动 agent future、
//! 排空事件 channel、响应滚动/退出——流式 delta 到达即渲染。

use crate::complete;
use crate::view::{ChatView, InputBuffer, Line};
use anyhow::Context;
use crossterm::{
    event::{Event, EventStream, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt as _;
use ratatui::{
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line as RLine, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Terminal,
};
use rupi_agent::AgentLoop;
use rupi_core::{commands, AgentEvent, SessionTree};
use rupi_llm::LlmProvider;
use rupi_mcp::McpManager;
use rupi_memory::{FrozenMemory, MemoryManager};
use rupi_skills::SkillRegistry;
use rupi_tools::ToolRegistry;
use std::io::Stdout;
use std::sync::Arc;
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
    /// 自定义斜杠命令目录：发送前展开（与 REPL 同语义）。
    pub command_dirs: Vec<std::path::PathBuf>,
    /// review 建议行缓冲（agent 回调写入，UI 每帧排空为 System 行）。`--review` 时装配。
    pub review_lines: Option<Arc<std::sync::Mutex<Vec<String>>>>,
    /// sessions.db 会话 id：`/model` 切换后重建 provider 时回填亲和头（与 REPL 同语义）。
    pub session_id: String,
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

/// 内建斜杠派发结果：Quit 退出主循环；Done 顯示一行系统消息并等下一输入；
/// Compact 手动压实（async，调用方执行后推行反馈）；Pass 非内建，调用方走自定义展开/发送。
/// 纯逻辑（无终端依赖），可单测。
enum Builtin {
    Quit,
    Done(String),
    Compact,
    Pass,
}

fn dispatch_builtin(
    text: &str,
    agent: &mut AgentLoop,
    session: &mut SessionTree,
    provider: &mut Arc<dyn LlmProvider>,
    skills: &SkillRegistry,
    command_dirs: &[std::path::PathBuf],
    session_id: &str,
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
        return Builtin::Done(commands::index_block(command_dirs));
    }
    if t == "/tree" {
        return Builtin::Done(session.tree_view());
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
    if t == "/rewind" {
        return if session.current_path.len() >= 2 {
            let target = session.current_path[session.current_path.len() - 2].clone();
            session.rewind_to(&target);
            Builtin::Done("[rewound]".into())
        } else {
            Builtin::Done("[rewind] nothing to undo".into())
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
                Builtin::Done(format!("[model switched to {arg}]"))
            }
            Err(e) => Builtin::Done(format!("[model] switch failed ({e:#})")),
        };
    }
    if t == "/reload" {
        // 热重载要 ExtensionSet 所有权 + 可变工具表（REPL 专属路径），TUI 只读装配
        return Builtin::Done(
            "[reload] hot reload is REPL-only; restart TUI to pick up extension changes".into(),
        );
    }
    if t == "/compact" {
        // 压实调模型是 async：这里只做标记，run_loop 内 await 执行（与 REPL /compact 同反馈文案）
        return Builtin::Compact;
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
    view.push_system("rupi TUI — Enter 发送，Esc 中止本轮，/quit 退出，/tree 看树，/goto <短id> 跳转，/rewind 回退，/compact 手动压实，/plan 计划模式，/thinking 思考强度，/model 切换模型，/skills 看技能，/commands 看自定义命令，PgUp/PgDn 滚动".into());
    let mut input = InputBuffer::default();
    let mut scroll: u16 = 0;
    let mut reader = EventStream::new();

    loop {
        // 斜杠补全候选：内建 + 自定义命令（小目录扫描，随输入更新；Enter 前 Tab 应用）
        let custom_names: Vec<String> = commands::list(&ctx.command_dirs)
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        let completion = complete::candidates(&input.text(), &custom_names);
        draw(terminal, &view, &input, scroll, false, &completion)?;
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
                let text = input.take();
                // MCP 工具热刷新：server 发 notifications/tools/list_changed 即重列差量更新
                if let (Some(m), Some(rx)) = (ctx.mcp, ctx.mcp_rx.as_mut()) {
                    while let Ok(srv) = rx.try_recv() {
                        match m.refresh_server(&mut *ctx.tools, &srv).await {
                            Ok(added) if !added.is_empty() => view.push_system(format!(
                                "[mcp] {srv} tools added: {}",
                                added.join(", ")
                            )),
                            Ok(_) => view.push_system(format!("[mcp] {srv} tools updated")),
                            Err(e) => view.push_system(format!("[mcp] refresh {srv} failed: {e:#}")),
                        }
                    }
                }
                match dispatch_builtin(
                    &text,
                    ctx.agent,
                    ctx.session,
                    ctx.provider,
                    ctx.skills,
                    &ctx.command_dirs,
                    &ctx.session_id,
                ) {
                    Builtin::Quit => break,
                    Builtin::Done(msg) => {
                        view.push_system(msg);
                        continue;
                    }
                    Builtin::Compact => {
                        let before = ctx.session.summary.clone();
                        let buffered =
                            std::sync::Mutex::new(Vec::<rupi_core::AgentEvent>::new());
                        ctx.agent
                            .force_compress_with_event(
                                &**ctx.provider,
                                ctx.session,
                                ctx.mem,
                                &|e| {
                                    buffered.lock().unwrap().push(e);
                                },
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
                // 自定义斜杠命令：内建优先（上已 continue），命中则展开为提示词
                let slash = commands::split(&text).map(|(n, a)| (n.to_owned(), a.to_owned()));
                let mut send_text = text.clone();
                if let Some((name, args)) = slash.as_ref() {
                    if let Some(expanded) = commands::expand(&ctx.command_dirs, name, args) {
                        view.push_system(format!("[command /{name}]"));
                        send_text = expanded;
                    }
                }
                view.push_user(send_text.clone());
                scroll = 0;
                // 发送前刷新 skill 注册表：上一轮蒸馏的新 skill 本轮即对模型可见
                ctx.skills.refresh(&ctx.skill_dirs);
                match drive_turn(
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
                    send_text,
                )
                .await?
                {
                    Control::Continue => {}
                    Control::Quit => break,
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// 回合内循环：agent future 与键盘事件同场 `select!`，delta 到达即画。
#[allow(clippy::too_many_arguments)]
async fn drive_turn(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    agent: &AgentLoop,
    provider: &dyn LlmProvider,
    session: &mut SessionTree,
    tools: &ToolRegistry,
    mem: &MemoryManager,
    frozen: &FrozenMemory,
    skills: &SkillRegistry,
    review_lines: &Option<Arc<std::sync::Mutex<Vec<String>>>>,
    on_turn: &Option<Arc<dyn Fn(TurnRecord) + Send + Sync>>,
    reader: &mut EventStream,
    view: &mut ChatView,
    text: String,
) -> anyhow::Result<Control> {
    let (tx, rx) = std::sync::mpsc::channel::<AgentEvent>();
    let on_event = |e: AgentEvent| {
        let _ = tx.send(e);
    };
    // 本轮前路径长度：用户节点即 current_path[before]，落盘沿用其 id（resume 短 id 稳定）
    let before = session.current_path.len();
    // 协作取消：内循环 Esc 置位，主循环在检查点优雅中止（TurnEnd/RunEnd{Aborted} 照常走事件通道）。
    let cancel = rupi_core::CancelFlag::new();
    let fut = agent.run(
        provider,
        session,
        &text,
        tools,
        mem,
        frozen,
        skills,
        &[],
        &on_event,
        &cancel,
    );
    let mut scroll: u16 = 0;
    // 内循环只负责驱动 + 渲染；pin 守卫连同 fut 一起终结于块内，之后才能再读 session
    enum End {
        Finished(anyhow::Result<rupi_core::StopReason>),
        Quit,
    }
    let end: End = {
        tokio::pin!(fut);
        loop {
            while let Ok(e) = rx.try_recv() {
                view.push_event(&e);
            }
            if let Some(buf) = review_lines {
                for line in buf.lock().unwrap().drain(..) {
                    view.push_system(line);
                }
            }
            draw(terminal, view, &InputBuffer::default(), scroll, true, &[])?;
            tokio::select! {
                res = &mut fut => {
                    while let Ok(e) = rx.try_recv() {
                        view.push_event(&e);
                    }
                    if let Some(buf) = review_lines {
                        for line in buf.lock().unwrap().drain(..) {
                            view.push_system(line);
                        }
                    }
                    break End::Finished(res);
                }
                maybe_key = reader.next() => {
                    match maybe_key {
                        Some(Ok(Event::Key(key))) => match key.code {
                            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                break End::Quit;
                            }
                            // 中止本轮：只置位不 drop future，会话停在一致点，
                            // TurnEnd/RunEnd{Aborted} 照常进视图。
                            KeyCode::Esc => {
                                cancel.cancel();
                            }
                            KeyCode::PageUp => scroll = scroll.saturating_add(5),
                            KeyCode::PageDown => scroll = scroll.saturating_sub(5),
                            _ => {}
                        },
                        _ => {}
                    }
                }
            }
        }
    };
    match end {
        End::Quit => Ok(Control::Quit),
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
                    if let Some(cb) = on_turn {
                        cb(TurnRecord {
                            user: text.clone(),
                            assistant,
                            summary: session.summary.clone(),
                            user_node,
                            assistant_node,
                        });
                    }
                }
                Err(e) => {
                    view.push_system(format!("turn failed: {e:#}"));
                }
            }
            Ok(Control::Continue)
        }
    }
}

/// 全屏绘制（Backend 泛型：生产走 Crossterm，单测走 TestBackend 真画一遍断言像素行）。
fn draw<B: Backend>(
    terminal: &mut Terminal<B>,
    view: &ChatView,
    input: &InputBuffer,
    scroll: u16,
    busy: bool,
    completion: &[String],
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
                            format!("/{c}"),
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
                    "… thinking (Ctrl-C 退出)"
                } else {
                    "ready"
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
        draw(&mut terminal, &view, &input, 0, false, &completion).unwrap();
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
                "t-sess"
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
                "t-sess"
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
                 "t-sess"
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
                 "t-sess"
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
                 "t-sess"
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
                 "t-sess"
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
                 "t-sess"
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
                 "t-sess"
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
                 "t-sess"
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
                 "t-sess"
            )),
            "[rewound]"
        );
        assert!(session.current_path.len() < 3);
    }

    #[test]
    fn compact_maps_to_marker_not_model() {
        // /compact 只做标记（async 压实由 run_loop 执行），绝不漏进模型
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
            ),
            Builtin::Compact
        ));
    }

    #[test]
    fn reload_is_explicitly_unsupported_and_goto_unknown() {
        let (mut agent, mut session, mut provider, skills) = harness();
        let msg = done_text(dispatch_builtin(
            "/reload",
            &mut agent,
            &mut session,
                 &mut provider,
                 &skills,
                 &[],
                "t-sess",
        ));
        assert!(msg.contains("REPL-only"), "{msg}");
        let msg = done_text(dispatch_builtin(
            "/goto zzz",
            &mut agent,
            &mut session,
                 &mut provider,
                 &skills,
                 &[],
                "t-sess",
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
        ));
        assert_ne!(msg, msg2, "有节点后视图应变化");
    }
}
