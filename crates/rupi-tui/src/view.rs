//! 纯视图逻辑（可单测）：输入缓冲 + 事件折叠成渲染行 + 增量视觉缓存。

use crate::kitty::InlineImage;
use crate::markdown::{render_markdown_rows, MdRow};
use crate::theme::Theme;
use ratatui::style::Style;
use ratatui::text::{Line as RLine, Span};
use rupi_core::AgentEvent;

/// 单行渲染内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    User(String),
    AssistantText(String),
    Tool {
        name: String,
        summary: String,
        full: String,
        is_error: bool,
    },
    Thinking(String),
    System(String),
    Image {
        alt: String,
        media_type: String,
        data: String,
    },
}

/// 输入框：字符缓冲 + 光标（字符级，支持中文）。
#[derive(Debug, Default)]
pub struct InputBuffer {
    chars: Vec<char>,
    cursor: usize,
}

impl InputBuffer {
    pub fn push_char(&mut self, c: char) {
        self.chars.insert(self.cursor, c);
        self.cursor += 1;
    }

    /// 括号粘贴：整段插入（含换行），不把换行当提交。
    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars() {
            if c == '\r' {
                continue;
            }
            self.push_char(c);
        }
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }

    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_right(&mut self) {
        if self.cursor < self.chars.len() {
            self.cursor += 1;
        }
    }

    pub fn take(&mut self) -> String {
        let s: String = self.chars.iter().collect();
        self.chars.clear();
        self.cursor = 0;
        s
    }

    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn set_text(&mut self, s: &str) {
        self.chars = s.chars().collect();
        self.cursor = self.chars.len();
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn set_text_and_cursor(&mut self, s: &str, cursor: usize) {
        self.chars = s.chars().collect();
        self.cursor = cursor.min(self.chars.len());
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    pub fn split_for_render(&self) -> (String, String) {
        (
            self.chars[..self.cursor].iter().collect(),
            self.chars[self.cursor..].iter().collect(),
        )
    }
}

/// 对标 Pi TUI：超过 10 行的粘贴在预览里收成一行。
pub const PASTE_COLLAPSE_LINES: usize = 10;

pub fn collapse_paste_preview(text: &str) -> Option<String> {
    let lines = text.split('\n').count();
    if lines <= PASTE_COLLAPSE_LINES {
        None
    } else {
        Some(format!(
            "[pasted {lines} lines, {} chars]",
            text.chars().count()
        ))
    }
}

#[derive(Debug, Default)]
struct VisualCache {
    key: u64,
    rows: Vec<RLine<'static>>,
    images: Vec<InlineImage>,
    /// 每个逻辑行对应视觉行的起始下标。
    starts: Vec<usize>,
    items: usize,
    last_fp: u64,
}

fn visual_key(
    theme: &Theme,
    tools_folded: bool,
    thinking_folded: bool,
    inline_images: bool,
) -> u64 {
    theme.id
        ^ ((u64::from(tools_folded)) << 60)
        ^ ((u64::from(thinking_folded)) << 61)
        ^ ((u64::from(inline_images)) << 62)
}

/// 聊天视图：把 `AgentEvent` 流折叠为行；连续 `TextDelta` 合并进同一行。
#[derive(Debug, Default)]
pub struct ChatView {
    pub lines: Vec<Line>,
    cache: VisualCache,
}

impl ChatView {
    pub fn push_event(&mut self, e: &AgentEvent) {
        match e {
            AgentEvent::TextDelta { delta } => {
                if let Some(Line::AssistantText(last)) = self.lines.last_mut() {
                    last.push_str(delta);
                } else {
                    self.lines.push(Line::AssistantText(delta.clone()));
                }
            }
            AgentEvent::ToolStart { name, .. } => {
                self.lines.push(Line::Tool {
                    name: name.clone(),
                    summary: format!("◌ {name} …"),
                    full: String::new(),
                    is_error: false,
                });
            }
            AgentEvent::ToolEnd {
                name,
                content,
                is_error,
                ..
            } => {
                let first = content
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(80)
                    .collect::<String>();
                let mark = if *is_error { "✗" } else { "✓" };
                self.lines.push(Line::Tool {
                    name: name.clone(),
                    summary: format!("{mark} {name}: {first}"),
                    full: content.clone(),
                    is_error: *is_error,
                });
            }
            AgentEvent::TurnStart { .. }
            | AgentEvent::TurnEnd { .. }
            | AgentEvent::RunEnd { .. }
            | AgentEvent::UiPromptStart { .. }
            | AgentEvent::UiPromptEnd { .. } => {}
            AgentEvent::MemoryRecall { detail } => {
                self.lines.push(Line::System(detail.clone()));
            }
            AgentEvent::CompactionStart => {
                self.lines.push(Line::Tool {
                    name: "compact".into(),
                    summary: "◌ compacting …".into(),
                    full: String::new(),
                    is_error: false,
                });
            }
            AgentEvent::CompactionEnd { summarized, kept } => {
                self.lines.push(Line::Tool {
                    name: "compact".into(),
                    summary: format!("✓ compacted: summarized {summarized}, kept {kept}"),
                    full: String::new(),
                    is_error: false,
                });
            }
            AgentEvent::Usage {
                input_tokens,
                output_tokens,
                cache_read,
                cache_write,
            } => {
                self.lines.push(Line::System(format!(
                    "· usage in={input_tokens} out={output_tokens} R={cache_read} W={cache_write}"
                )));
            }
            AgentEvent::Error { message } => {
                self.lines.push(Line::System(format!("error: {message}")));
            }
            AgentEvent::UiHint {
                source,
                kind,
                message,
            } => {
                self.lines
                    .push(Line::System(format!("[ui {source}/{kind}] {message}")));
            }
            AgentEvent::Thinking { text } => {
                self.lines.push(Line::Thinking(text.clone()));
            }
            AgentEvent::SteeringInjected { messages, .. } => {
                for m in messages {
                    self.lines.push(Line::User(format!("[steer] {m}")));
                }
            }
            AgentEvent::ModelChange { provider, model } => {
                self.lines
                    .push(Line::System(format!("[model {provider}/{model}]")));
            }
            AgentEvent::ExtensionUiRequest { method, title, .. } => {
                self.lines.push(Line::System(format!(
                    "[ui {method}] {}",
                    title.as_deref().unwrap_or("")
                )));
            }
            AgentEvent::AutoRetryStart { attempt, .. } => {
                self.lines.push(Line::System(format!("[retry {attempt}]")));
            }
            AgentEvent::AutoRetryEnd {
                success, attempt, ..
            } => {
                self.lines.push(Line::System(format!(
                    "[retry {attempt} {}]",
                    if *success { "ok" } else { "fail" }
                )));
            }
        }
    }

    pub fn push_user(&mut self, text: String) {
        self.lines.push(Line::User(text));
    }

    pub fn push_system(&mut self, text: String) {
        self.lines.push(Line::System(text));
    }

    pub fn push_image(
        &mut self,
        alt: impl Into<String>,
        media_type: impl Into<String>,
        data: impl Into<String>,
    ) {
        self.lines.push(Line::Image {
            alt: alt.into(),
            media_type: media_type.into(),
            data: data.into(),
        });
    }

    pub fn inline_images(&self) -> &[InlineImage] {
        &self.cache.images
    }

    /// 只重绘脏逻辑行；调用方再切片可见窗口，避免每帧 clone 全部 `view.lines`。
    pub fn visual_lines(
        &mut self,
        theme: &Theme,
        tools_folded: bool,
        thinking_folded: bool,
        inline_images: bool,
    ) -> &[RLine<'static>] {
        let key = visual_key(theme, tools_folded, thinking_folded, inline_images);
        if self.cache.key != key {
            self.rebuild(theme, tools_folded, thinking_folded, inline_images);
            return &self.cache.rows;
        }
        while self.cache.items < self.lines.len() {
            self.append_item(
                self.cache.items,
                theme,
                tools_folded,
                thinking_folded,
                inline_images,
            );
        }
        if let Some(last) = self.lines.last() {
            let fp = line_fp(last);
            if fp != self.cache.last_fp && !self.lines.is_empty() {
                self.rebuild_from(
                    self.lines.len() - 1,
                    theme,
                    tools_folded,
                    thinking_folded,
                    inline_images,
                );
            }
        }
        &self.cache.rows
    }

    fn rebuild(
        &mut self,
        theme: &Theme,
        tools_folded: bool,
        thinking_folded: bool,
        inline_images: bool,
    ) {
        self.cache.rows.clear();
        self.cache.images.clear();
        self.cache.starts.clear();
        self.cache.items = 0;
        self.cache.key = visual_key(theme, tools_folded, thinking_folded, inline_images);
        for i in 0..self.lines.len() {
            self.append_item(i, theme, tools_folded, thinking_folded, inline_images);
        }
    }

    fn rebuild_from(
        &mut self,
        from: usize,
        theme: &Theme,
        tools_folded: bool,
        thinking_folded: bool,
        inline_images: bool,
    ) {
        let cut = *self
            .cache
            .starts
            .get(from)
            .unwrap_or(&self.cache.rows.len());
        self.cache.rows.truncate(cut);
        self.cache.images.retain(|img| img.row < cut);
        self.cache.starts.truncate(from);
        self.cache.items = from;
        for i in from..self.lines.len() {
            self.append_item(i, theme, tools_folded, thinking_folded, inline_images);
        }
    }

    fn append_item(
        &mut self,
        idx: usize,
        theme: &Theme,
        tools_folded: bool,
        thinking_folded: bool,
        inline_images: bool,
    ) {
        self.cache.starts.push(self.cache.rows.len());
        let (rows, images) = render_item(
            &self.lines[idx],
            theme,
            tools_folded,
            thinking_folded,
            inline_images,
        );
        let base = self.cache.rows.len();
        for mut img in images {
            img.row += base;
            self.cache.images.push(img);
        }
        self.cache.rows.extend(rows);
        self.cache.items = idx + 1;
        self.cache.last_fp = line_fp(&self.lines[idx]);
    }
}

fn line_fp(l: &Line) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    match l {
        Line::User(s) | Line::AssistantText(s) | Line::Thinking(s) | Line::System(s) => {
            s.hash(&mut h)
        }
        Line::Image {
            alt,
            media_type,
            data,
        } => {
            alt.hash(&mut h);
            media_type.hash(&mut h);
            data.hash(&mut h);
        }
        Line::Tool {
            name,
            summary,
            full,
            is_error,
        } => {
            name.hash(&mut h);
            summary.hash(&mut h);
            full.hash(&mut h);
            is_error.hash(&mut h);
        }
    }
    h.finish()
}

fn render_item(
    l: &Line,
    theme: &Theme,
    tools_folded: bool,
    thinking_folded: bool,
    inline_images: bool,
) -> (Vec<RLine<'static>>, Vec<InlineImage>) {
    match l {
        Line::User(t) => (
            vec![RLine::from(vec![
                Span::styled("you: ", Style::default().fg(theme.user)),
                Span::raw(t.clone()),
            ])],
            Vec::new(),
        ),
        Line::AssistantText(t) => collect_md(render_markdown_rows(t, theme, inline_images), theme),
        Line::Tool { summary, full, .. } => {
            if tools_folded || full.is_empty() || full == summary {
                (
                    vec![RLine::from(Span::styled(
                        summary.clone(),
                        Style::default().fg(theme.tool),
                    ))],
                    Vec::new(),
                )
            } else {
                let mut rows = vec![RLine::from(Span::styled(
                    format!("{summary}  [Ctrl+O 折叠]"),
                    Style::default().fg(theme.tool),
                ))];
                for line in full.lines() {
                    rows.push(RLine::from(Span::styled(
                        line.to_string(),
                        Style::default().fg(theme.tool),
                    )));
                }
                (rows, Vec::new())
            }
        }
        Line::Thinking(t) => {
            if thinking_folded {
                let n = t.chars().count();
                (
                    vec![RLine::from(Span::styled(
                        format!("▸ thinking ({n} chars, Ctrl+T)"),
                        Style::default().fg(theme.thinking),
                    ))],
                    Vec::new(),
                )
            } else {
                let mut rows = vec![RLine::from(Span::styled(
                    "▾ thinking",
                    Style::default().fg(theme.thinking),
                ))];
                for line in t.lines() {
                    rows.push(RLine::from(Span::styled(
                        line.to_string(),
                        Style::default().fg(theme.thinking),
                    )));
                }
                (rows, Vec::new())
            }
        }
        Line::System(t) => (
            vec![RLine::from(vec![Span::styled(
                t.clone(),
                Style::default().fg(theme.system),
            )])],
            Vec::new(),
        ),
        Line::Image {
            alt,
            media_type,
            data,
        } => collect_md(
            crate::markdown::image_preview(alt, media_type, data, theme, inline_images),
            theme,
        ),
    }
}

fn collect_md(rows: Vec<MdRow>, theme: &Theme) -> (Vec<RLine<'static>>, Vec<InlineImage>) {
    let mut lines = Vec::new();
    let mut images = Vec::new();
    for row in rows {
        match row {
            MdRow::Line(l) => lines.push(l),
            MdRow::Image(img) => {
                let px = crate::kitty::dimensions(&img.media_type, &img.data);
                lines.push(RLine::from(Span::styled(
                    crate::kitty::caption(&img.alt, &img.media_type, px),
                    Style::default().fg(theme.system),
                )));
                images.push(img);
            }
        }
    }
    (lines, images)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_buffer_edits_around_cursor() {
        let mut b = InputBuffer::default();
        for c in "你好ab".chars() {
            b.push_char(c);
        }
        b.move_left();
        b.move_left();
        b.backspace();
        let (pre, post) = b.split_for_render();
        assert_eq!(pre, "你");
        assert_eq!(post, "ab");
        assert_eq!(b.take(), "你ab");
        assert!(b.is_empty());
    }

    #[test]
    fn bracketed_paste_keeps_newlines_and_collapses_preview() {
        let mut b = InputBuffer::default();
        b.insert_str("one\r\ntwo\nthree");
        assert_eq!(b.text(), "one\ntwo\nthree");
        assert!(collapse_paste_preview(&b.text()).is_none());
        let big = (0..12)
            .map(|i| format!("L{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let preview = collapse_paste_preview(&big).expect("should collapse");
        assert!(preview.contains("12 lines"), "{preview}");
        assert!(preview.contains("chars"), "{preview}");
    }

    #[test]
    fn text_deltas_coalesce_and_tools_render() {
        let mut v = ChatView::default();
        v.push_user("hi".into());
        v.push_event(&AgentEvent::TextDelta {
            delta: "hel".into(),
        });
        v.push_event(&AgentEvent::TextDelta { delta: "lo".into() });
        v.push_event(&AgentEvent::ToolStart {
            tool_call_id: "1".into(),
            name: "read".into(),
            arguments: serde_json::json!({}),
        });
        v.push_event(&AgentEvent::ToolEnd {
            tool_call_id: "1".into(),
            name: "read".into(),
            content: "file content here".into(),
            is_error: false,
        });
        v.push_event(&AgentEvent::TextDelta {
            delta: "done".into(),
        });
        assert_eq!(v.lines.len(), 5);
        assert_eq!(v.lines[1], Line::AssistantText("hello".into()));
        assert!(matches!(v.lines[3], Line::Tool { .. }));
        assert_eq!(v.lines[4], Line::AssistantText("done".into()));
    }

    #[test]
    fn memory_recall_renders_as_system_line() {
        let mut v = ChatView::default();
        v.push_event(&AgentEvent::MemoryRecall {
            detail: "🧠 jsonl — recalled 2 memories".into(),
        });
        assert_eq!(
            v.lines,
            vec![Line::System("🧠 jsonl — recalled 2 memories".into())]
        );
    }

    #[test]
    fn compaction_events_render_as_tool_status_lines() {
        let mut v = ChatView::default();
        v.push_event(&AgentEvent::CompactionStart);
        v.push_event(&AgentEvent::CompactionEnd {
            summarized: 4,
            kept: 2,
        });
        assert_eq!(v.lines.len(), 2);
        assert!(
            matches!(&v.lines[0], Line::Tool { summary, .. } if summary.contains("compacting"))
        );
        assert!(
            matches!(&v.lines[1], Line::Tool { summary, .. } if summary.contains("summarized 4") && summary.contains("kept 2"))
        );
    }

    #[test]
    fn incremental_visual_does_not_rebuild_prefix() {
        let theme = Theme::dark();
        let mut v = ChatView::default();
        v.push_user("a".into());
        let n1 = v.visual_lines(&theme, true, true, false).len();
        v.push_event(&AgentEvent::TextDelta { delta: "x".into() });
        let n2 = v.visual_lines(&theme, true, true, false).len();
        assert!(n2 >= n1);
        v.push_event(&AgentEvent::TextDelta { delta: "y".into() });
        let rows = v.visual_lines(&theme, true, true, false);
        let text: String = rows
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(text.contains("xy"), "{text}");
        assert_eq!(v.cache.items, v.lines.len());
    }

    #[test]
    fn thinking_and_tool_fold() {
        let theme = Theme::dark();
        let mut v = ChatView::default();
        v.push_event(&AgentEvent::Thinking {
            text: "secret plan".into(),
        });
        v.push_event(&AgentEvent::ToolEnd {
            tool_call_id: "1".into(),
            name: "bash".into(),
            content: "line1\nline2\nline3".into(),
            is_error: false,
        });
        let folded = v.visual_lines(&theme, true, true, false);
        let ft: String = folded
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(ft.contains("thinking"), "{ft}");
        assert!(!ft.contains("line2"), "{ft}");
        let open = v.visual_lines(&theme, false, false, false);
        let ot: String = open
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(ot.contains("line2"), "{ot}");
        assert!(ot.contains("secret plan"), "{ot}");
    }

    #[test]
    fn mermaid_and_image_rows_in_visual_cache() {
        let theme = Theme::dark();
        let mut v = ChatView::default();
        v.push_event(&AgentEvent::TextDelta {
            delta: "```mermaid\ngraph TD\n  A[Hello] --> B[World]\n```\n".into(),
        });
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        v.push_image("dot", "image/png", png);
        let rows = v.visual_lines(&theme, true, true, true);
        let text: String = rows
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(text.contains("Hello"), "{text}");
        assert!(text.contains("┌"), "{text}");
        assert!(text.contains("image"), "{text}");
        assert_eq!(v.inline_images().len(), 1);
        assert_eq!(v.inline_images()[0].media_type, "image/png");
    }
}
