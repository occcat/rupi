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
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line as RLine, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Terminal,
};
use rupi_agent::AgentLoop;
use rupi_core::{commands, AgentEvent, SessionTree};
use rupi_llm::LlmProvider;
use rupi_memory::{FrozenMemory, MemoryManager};
use rupi_skills::SkillRegistry;
use rupi_tools::ToolRegistry;
use std::io::Stdout;
use std::sync::Arc;

/// TUI 运行上下文：与 REPL 版 `run_chat` 同构的装配。
pub struct TuiContext<'a> {
    pub provider: &'a dyn LlmProvider,
    pub agent: &'a AgentLoop,
    pub session: &'a mut SessionTree,
    pub tools: &'a ToolRegistry,
    pub mem: &'a MemoryManager,
    pub frozen: &'a FrozenMemory,
    pub skills: &'a SkillRegistry,
    /// skill 发现目录：每轮发送前 `refresh`，会话内新蒸馏 skill 即时可见（与 REPL 同闭环）。
    pub skill_dirs: Vec<std::path::PathBuf>,
    /// 自定义斜杠命令目录：发送前展开（与 REPL 同语义）。
    pub command_dirs: Vec<std::path::PathBuf>,
    /// review 建议行缓冲（agent 回调写入，UI 每帧排空为 System 行）。`--review` 时装配。
    pub review_lines: Option<Arc<std::sync::Mutex<Vec<String>>>>,
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

enum Control {
    Continue,
    Quit,
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ctx: TuiContext<'_>,
) -> anyhow::Result<()> {
    let mut view = ChatView::default();
    view.push_system("rupi TUI — Enter 发送，/quit 退出，/tree 看树，/goto <短id> 跳转，/skills 看技能，/commands 看自定义命令，PgUp/PgDn 滚动".into());
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
                if text.trim() == "/quit" {
                    break;
                }
                if text.trim() == "/skills" {
                    view.push_system(ctx.skills.index_block());
                    continue;
                }
                if text.trim() == "/commands" {
                    view.push_system(commands::index_block(&ctx.command_dirs));
                    continue;
                }
                if text.trim() == "/tree" {
                    view.push_system(ctx.session.tree_view());
                    continue;
                }
                if let Some(prefix) = text.trim().strip_prefix("/goto ") {
                    let prefix = prefix.trim();
                    match ctx.session.resolve_short_id(prefix) {
                        Some(id) if ctx.session.goto_node(&id) => {
                            view.push_system(format!("[goto {}]", &id[..8.min(id.len())]));
                        }
                        _ => view.push_system(format!(
                            "[goto] unknown or ambiguous node prefix: {prefix}"
                        )),
                    }
                    continue;
                }
                // 自定义斜杠命令：内建优先（上已 continue），命中则展开为提示词
                let slash =
                    commands::split(&text).map(|(n, a)| (n.to_owned(), a.to_owned()));
                let mut send_text = text.clone();
                if let Some((name, args)) = slash.as_ref() {
                    if let Some(expanded) = commands::expand(&ctx.command_dirs, name, args)
                    {
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
                    ctx.provider,
                    ctx.session,
                    ctx.tools,
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

fn draw(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
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
                    .map(|c| RLine::from(vec![Span::styled(format!("/{c}"), Style::default().fg(Color::Yellow))]))
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
                    Paragraph::new(items).block(Block::default().borders(Borders::ALL).title("Tab 补全")),
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
