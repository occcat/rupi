//! 轻量 Markdown → ratatui 行（标题、围栏代码、列表、粗体、行内代码）。

use crate::theme::Theme;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line as RLine, Span};

pub fn render_markdown(text: &str, theme: &Theme) -> Vec<RLine<'static>> {
    let mut out = Vec::new();
    let mut in_code = false;
    let mut code_buf: Vec<String> = Vec::new();
    for raw in text.lines() {
        let t = raw.trim_end();
        if t.starts_with("```") {
            if in_code {
                for row in code_buf.drain(..) {
                    out.push(RLine::from(Span::styled(
                        row,
                        Style::default().fg(theme.code),
                    )));
                }
                in_code = false;
            } else {
                in_code = true;
            }
            continue;
        }
        if in_code {
            code_buf.push(t.to_string());
            continue;
        }
        if let Some(rest) = t.strip_prefix("### ") {
            out.push(RLine::from(Span::styled(
                rest.to_string(),
                Style::default()
                    .fg(theme.heading)
                    .add_modifier(Modifier::BOLD),
            )));
        } else if let Some(rest) = t.strip_prefix("## ") {
            out.push(RLine::from(Span::styled(
                rest.to_string(),
                Style::default()
                    .fg(theme.heading)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            )));
        } else if let Some(rest) = t.strip_prefix("# ") {
            out.push(RLine::from(Span::styled(
                rest.to_string(),
                Style::default()
                    .fg(theme.heading)
                    .add_modifier(Modifier::BOLD),
            )));
        } else if let Some(rest) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) {
            let mut spans = vec![Span::styled(
                "• ".to_string(),
                Style::default().fg(theme.accent),
            )];
            spans.extend(inline(rest, theme));
            out.push(RLine::from(spans));
        } else if t.is_empty() {
            out.push(RLine::from(""));
        } else {
            out.push(RLine::from(inline(t, theme)));
        }
    }
    if in_code {
        for row in code_buf {
            out.push(RLine::from(Span::styled(
                row,
                Style::default().fg(theme.code),
            )));
        }
    }
    if out.is_empty() {
        out.push(RLine::from(""));
    }
    out
}

fn inline(text: &str, theme: &Theme) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    let mut buf = String::new();
    while i < chars.len() {
        if chars[i] == '`' {
            if !buf.is_empty() {
                spans.push(Span::styled(
                    std::mem::take(&mut buf),
                    Style::default().fg(theme.assistant),
                ));
            }
            i += 1;
            let mut code = String::new();
            while i < chars.len() && chars[i] != '`' {
                code.push(chars[i]);
                i += 1;
            }
            if i < chars.len() {
                i += 1;
            }
            spans.push(Span::styled(code, Style::default().fg(theme.code)));
        } else if chars[i] == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
            if !buf.is_empty() {
                spans.push(Span::styled(
                    std::mem::take(&mut buf),
                    Style::default().fg(theme.assistant),
                ));
            }
            i += 2;
            let mut bold = String::new();
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '*') {
                bold.push(chars[i]);
                i += 1;
            }
            if i + 1 < chars.len() {
                i += 2;
            }
            spans.push(Span::styled(
                bold,
                Style::default()
                    .fg(theme.assistant)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            buf.push(chars[i]);
            i += 1;
        }
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, Style::default().fg(theme.assistant)));
    }
    if spans.is_empty() {
        spans.push(Span::raw(String::new()));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headings_code_and_bold() {
        let theme = Theme::dark();
        let lines = render_markdown(
            "# Hi\n\nuse `x` and **bold**\n```\nfn a(){}\n```\n- item",
            &theme,
        );
        assert!(lines.len() >= 4);
        let flat: String = lines.iter().map(|l| l.width().to_string()).collect();
        let _ = flat;
        let text: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Hi"), "{text}");
        assert!(text.contains("fn a(){}"), "{text}");
        assert!(text.contains("item"), "{text}");
        assert!(text.contains("bold"), "{text}");
    }
}
