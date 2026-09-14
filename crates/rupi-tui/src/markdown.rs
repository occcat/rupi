//! 轻量 Markdown → ratatui 行（标题、围栏高亮、Mermaid、粗体、行内代码、standalone 图）。

use crate::highlight::highlight;
use crate::kitty::{self, InlineImage};
use crate::mermaid::{self, is_mermaid_lang};
use crate::theme::Theme;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line as RLine, Span};

pub enum MdRow {
    Line(RLine<'static>),
    Image(InlineImage),
}

pub fn render_markdown(text: &str, theme: &Theme) -> Vec<RLine<'static>> {
    flatten_rows(render_markdown_rows(text, theme, false))
}

pub fn render_markdown_rows(text: &str, theme: &Theme, inline_images: bool) -> Vec<MdRow> {
    let mut out = Vec::new();
    let mut in_code = false;
    let mut code_lang = String::new();
    let mut code_buf: Vec<String> = Vec::new();
    for raw in text.lines() {
        let t = raw.trim_end();
        if t.starts_with("```") {
            if in_code {
                out.extend(flush_fence(&code_lang, &code_buf, theme));
                code_buf.clear();
                code_lang.clear();
                in_code = false;
            } else {
                in_code = true;
                code_lang = fence_lang(t);
            }
            continue;
        }
        if in_code {
            code_buf.push(t.to_string());
            continue;
        }
        if let Some(img) = standalone_image(t) {
            out.extend(image_rows(img, theme, inline_images));
            continue;
        }
        if let Some(rest) = t.strip_prefix("### ") {
            out.push(MdRow::Line(RLine::from(Span::styled(
                rest.to_string(),
                Style::default()
                    .fg(theme.heading)
                    .add_modifier(Modifier::BOLD),
            ))));
        } else if let Some(rest) = t.strip_prefix("## ") {
            out.push(MdRow::Line(RLine::from(Span::styled(
                rest.to_string(),
                Style::default()
                    .fg(theme.heading)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            ))));
        } else if let Some(rest) = t.strip_prefix("# ") {
            out.push(MdRow::Line(RLine::from(Span::styled(
                rest.to_string(),
                Style::default()
                    .fg(theme.heading)
                    .add_modifier(Modifier::BOLD),
            ))));
        } else if let Some(rest) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) {
            let mut spans = vec![Span::styled(
                "• ".to_string(),
                Style::default().fg(theme.accent),
            )];
            spans.extend(inline(rest, theme));
            out.push(MdRow::Line(RLine::from(spans)));
        } else if t.is_empty() {
            out.push(MdRow::Line(RLine::from("")));
        } else {
            out.push(MdRow::Line(RLine::from(inline(t, theme))));
        }
    }
    if in_code {
        out.extend(flush_fence(&code_lang, &code_buf, theme));
    }
    if out.is_empty() {
        out.push(MdRow::Line(RLine::from("")));
    }
    out
}

fn flatten_rows(rows: Vec<MdRow>) -> Vec<RLine<'static>> {
    rows.into_iter()
        .map(|r| match r {
            MdRow::Line(l) => l,
            MdRow::Image(img) => {
                let px = kitty::dimensions(&img.media_type, &img.data);
                RLine::from(Span::styled(
                    kitty::caption(&img.alt, &img.media_type, px),
                    Style::default().fg(theme_system_fallback()),
                ))
            }
        })
        .collect()
}

fn theme_system_fallback() -> ratatui::style::Color {
    Theme::dark().system
}

fn fence_lang(opener: &str) -> String {
    opener
        .trim_start_matches('`')
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_string()
}

fn flush_fence(lang: &str, buf: &[String], theme: &Theme) -> Vec<MdRow> {
    let src = buf.join("\n");
    if is_mermaid_lang(lang) {
        return mermaid::render_mermaid(&src)
            .into_iter()
            .map(|row| {
                MdRow::Line(RLine::from(Span::styled(
                    row,
                    Style::default().fg(theme.code),
                )))
            })
            .collect();
    }
    highlight(&src, lang, theme)
        .into_iter()
        .map(MdRow::Line)
        .collect()
}

fn standalone_image(line: &str) -> Option<InlineImage> {
    let t = line.trim();
    let rest = t.strip_prefix("![")?;
    let (alt, rest) = rest.split_once("](")?;
    let src = rest.strip_suffix(')')?.trim();
    let src = src
        .split_whitespace()
        .next()
        .unwrap_or(src)
        .trim_matches(['<', '>']);
    if let Some((media, data)) = kitty::parse_data_url(src) {
        return Some(kitty::spec(alt, &media, &data));
    }
    if let Some((media, data)) = kitty::load_local_image(src) {
        return Some(kitty::spec(alt, &media, &data));
    }
    Some(kitty::spec(alt, "image", ""))
}

pub fn image_preview(
    alt: &str,
    media_type: &str,
    data: &str,
    theme: &Theme,
    inline_images: bool,
) -> Vec<MdRow> {
    image_rows(kitty::spec(alt, media_type, data), theme, inline_images)
}

fn image_rows(mut img: InlineImage, theme: &Theme, inline_images: bool) -> Vec<MdRow> {
    let px = kitty::dimensions(&img.media_type, &img.data);
    let cap = kitty::caption(&img.alt, &img.media_type, px);
    let cap_line = MdRow::Line(RLine::from(Span::styled(
        cap,
        Style::default().fg(theme.system),
    )));
    if !inline_images || img.data.is_empty() {
        return vec![cap_line];
    }
    let extra = img.rows.max(1).saturating_sub(1);
    img.row = 0;
    let mut rows = vec![MdRow::Image(img)];
    for _ in 0..extra {
        rows.push(MdRow::Line(RLine::from("")));
    }
    rows
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

    fn flat(lines: &[RLine]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn headings_code_and_bold() {
        let theme = Theme::dark();
        let lines = render_markdown(
            "# Hi\n\nuse `x` and **bold**\n```\nfn a(){}\n```\n- item",
            &theme,
        );
        assert!(lines.len() >= 4);
        let text = flat(&lines);
        assert!(text.contains("Hi"), "{text}");
        assert!(text.contains("fn a(){}"), "{text}");
        assert!(text.contains("item"), "{text}");
        assert!(text.contains("bold"), "{text}");
    }

    #[test]
    fn rust_fence_highlights_keyword() {
        let theme = Theme::dark();
        let lines = render_markdown("```rust\nfn main() {}\n```", &theme);
        let mut saw = false;
        for l in &lines {
            for s in &l.spans {
                if s.content.as_ref() == "fn" {
                    assert_eq!(s.style.fg, Some(theme.syntax_keyword));
                    saw = true;
                }
            }
        }
        assert!(saw, "{}", flat(&lines));
    }

    #[test]
    fn mermaid_fence_renders_boxes() {
        let theme = Theme::dark();
        let lines = render_markdown("```mermaid\ngraph TD\n  A[Hello] --> B[World]\n```", &theme);
        let text = flat(&lines);
        assert!(text.contains("Hello"), "{text}");
        assert!(text.contains("World"), "{text}");
        assert!(text.contains("┌"), "{text}");
        assert!(!text.contains("```"), "{text}");
    }

    #[test]
    fn standalone_data_image_caption() {
        let theme = Theme::dark();
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        let rows = render_markdown_rows(
            &format!("![dot](data:image/png;base64,{png})"),
            &theme,
            true,
        );
        assert!(
            rows.iter()
                .any(|r| matches!(r, MdRow::Image(img) if img.media_type == "image/png")),
            "missing image row"
        );
        let text = flatten_rows(render_markdown_rows(
            &format!("![dot](data:image/png;base64,{png})"),
            &theme,
            false,
        ));
        let s = flat(&text);
        assert!(s.contains("dot"), "{s}");
        assert!(s.contains("image"), "{s}");
    }
}
