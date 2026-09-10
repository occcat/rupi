//! Shared truncation utilities matching Pi's coding-agent defaults:
//! 2000 lines or 50KB, whichever is hit first. Never returns partial lines
//! for head truncation.

pub const DEFAULT_MAX_LINES: usize = 2000;
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;
pub const GREP_MAX_LINE_LENGTH: usize = 500;

#[derive(Debug, Clone)]
pub struct TruncationResult {
    pub content: String,
    pub truncated: bool,
    pub truncated_by: Option<&'static str>,
    pub total_lines: usize,
    pub total_bytes: usize,
    pub output_lines: usize,
    pub output_bytes: usize,
}

pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn split_lines(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    lines
}

pub fn truncate_head(content: &str, max_lines: usize, max_bytes: usize) -> TruncationResult {
    let lines = split_lines(content);
    let total_lines = lines.len();
    let total_bytes = content.len();
    let mut out = String::new();
    let mut truncated_by = None;
    for (i, line) in lines.iter().enumerate() {
        if i >= max_lines {
            truncated_by = Some("lines");
            break;
        }
        let add = if i == 0 {
            line.len()
        } else {
            line.len() + 1
        };
        if out.len() + add > max_bytes {
            truncated_by = Some("bytes");
            break;
        }
        if i > 0 {
            out.push('\n');
        }
        out.push_str(line);
    }
    let output_lines = if out.is_empty() { 0 } else { out.lines().count() };
    TruncationResult {
        output_bytes: out.len(),
        content: out,
        truncated: truncated_by.is_some(),
        truncated_by,
        total_lines,
        total_bytes,
        output_lines,
    }
}

pub fn truncate_tail(content: &str, max_lines: usize, max_bytes: usize) -> TruncationResult {
    let lines = split_lines(content);
    let total_lines = lines.len();
    let total_bytes = content.len();
    let mut kept = Vec::new();
    let mut bytes = 0usize;
    for line in lines.iter().rev() {
        if kept.len() >= max_lines {
            break;
        }
        let add = if kept.is_empty() {
            line.len()
        } else {
            line.len() + 1
        };
        if bytes + add > max_bytes {
            break;
        }
        bytes += add;
        kept.push(*line);
    }
    kept.reverse();
    let truncated = kept.len() < total_lines;
    let content = kept.join("\n");
    TruncationResult {
        output_lines: kept.len(),
        output_bytes: content.len(),
        content,
        truncated,
        truncated_by: if truncated {
            Some(if kept.len() >= max_lines {
                "lines"
            } else {
                "bytes"
            })
        } else {
            None
        },
        total_lines,
        total_bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_truncates_by_lines() {
        let text = (0..10).map(|i| format!("l{i}")).collect::<Vec<_>>().join("\n");
        let r = truncate_head(&text, 3, 10_000);
        assert!(r.truncated);
        assert_eq!(r.truncated_by, Some("lines"));
        assert_eq!(r.content, "l0\nl1\nl2");
    }
}
