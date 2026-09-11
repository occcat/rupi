//! 纯视图逻辑（可单测）：输入缓冲 + 事件折叠成渲染行。

use rupi_core::AgentEvent;

/// 单行渲染内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    User(String),
    AssistantText(String),
    Tool(String),
    System(String),
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

    /// 当前全文（补全候选计算用，不消费）。
    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    /// 写回补全结果，光标置末尾。
    pub fn set_text(&mut self, s: &str) {
        self.chars = s.chars().collect();
        self.cursor = self.chars.len();
    }

    /// 当前光标（字符级，@路径补全定位 token 用）。
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// 写回补全结果并指定光标（@行中补全用，越界钳制到末尾）。
    pub fn set_text_and_cursor(&mut self, s: &str, cursor: usize) {
        self.chars = s.chars().collect();
        self.cursor = cursor.min(self.chars.len());
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    /// 渲染为 (光标前文本, 光标后文本)，供精确放光标。
    pub fn split_for_render(&self) -> (String, String) {
        (
            self.chars[..self.cursor].iter().collect(),
            self.chars[self.cursor..].iter().collect(),
        )
    }
}

/// 聊天视图：把 `AgentEvent` 流折叠为行；连续 `TextDelta` 合并进同一行。
#[derive(Debug, Default)]
pub struct ChatView {
    pub lines: Vec<Line>,
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
                self.lines.push(Line::Tool(format!("◌ {name} …")));
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
                self.lines
                    .push(Line::Tool(format!("{mark} {name}: {first}")));
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
                self.lines.push(Line::Tool("◌ compacting …".into()));
            }
            AgentEvent::CompactionEnd { summarized, kept } => {
                self.lines.push(Line::Tool(format!(
                    "✓ compacted: summarized {summarized}, kept {kept}"
                )));
            }
            AgentEvent::Usage {
                input_tokens,
                output_tokens,
            } => {
                self.lines.push(Line::Tool(format!(
                    "· usage in={input_tokens} out={output_tokens}"
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
        }
    }

    pub fn push_user(&mut self, text: String) {
        self.lines.push(Line::User(text));
    }

    pub fn push_system(&mut self, text: String) {
        self.lines.push(Line::System(text));
    }
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
        assert!(matches!(v.lines[3], Line::Tool(_)));
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
        // 压实操作框定：start 转圈行，end 落盘行（含摘要/保留计数）
        let mut v = ChatView::default();
        v.push_event(&AgentEvent::CompactionStart);
        v.push_event(&AgentEvent::CompactionEnd {
            summarized: 4,
            kept: 2,
        });
        assert_eq!(v.lines.len(), 2);
        assert!(matches!(&v.lines[0], Line::Tool(s) if s.contains("compacting")));
        assert!(matches!(&v.lines[1], Line::Tool(s) if s.contains("summarized 4") && s.contains("kept 2")));
    }
}
