//! 围栏 `mermaid` → 终端 Unicode（flowchart / sequence；其余框起来源）。

use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dir {
    Td,
    Lr,
}

#[derive(Debug, Clone)]
struct Node {
    id: String,
    label: String,
}

#[derive(Debug, Clone)]
struct Edge {
    from: String,
    to: String,
    label: String,
}

#[derive(Debug, Clone)]
struct SeqMsg {
    from: String,
    to: String,
    text: String,
    dashed: bool,
}

pub fn is_mermaid_lang(lang: &str) -> bool {
    lang.trim().eq_ignore_ascii_case("mermaid")
}

/// 完整围栏渲染：能画则画，否则框起来源。
pub fn render_mermaid(src: &str) -> Vec<String> {
    let src = strip_init(src);
    let first = first_code_line(&src).unwrap_or("");
    let head = first.to_ascii_lowercase();
    if head.starts_with("graph") || head.starts_with("flowchart") {
        if let Some(lines) = render_flowchart(&src) {
            return lines;
        }
    } else if head.starts_with("sequencediagram") {
        if let Some(lines) = render_sequence(&src) {
            return lines;
        }
    }
    frame_source(&src, "mermaid")
}

fn strip_init(src: &str) -> String {
    let mut s = src.trim().to_string();
    while let Some(start) = s.find("%%{") {
        if let Some(rel) = s[start..].find("}%%") {
            let end = start + rel + 3;
            s.replace_range(start..end, " ");
        } else {
            break;
        }
    }
    s
}

fn first_code_line(src: &str) -> Option<&str> {
    src.lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("%%"))
}

fn frame_source(src: &str, title: &str) -> Vec<String> {
    let body: Vec<&str> = src.lines().map(|l| l.trim_end()).collect();
    let w = body
        .iter()
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(0)
        .max(title.chars().count() + 2)
        .min(78);
    let mut out = Vec::new();
    out.push(format!(
        "┌ {title} {}",
        "─".repeat(w.saturating_sub(title.chars().count()).max(1))
    ));
    for line in body {
        out.push(format!("│ {line}"));
    }
    out.push(format!("└{}", "─".repeat(w + 2)));
    out
}

fn render_flowchart(src: &str) -> Option<Vec<String>> {
    let mut dir = Dir::Td;
    let mut nodes: HashMap<String, Node> = HashMap::new();
    let mut edges = Vec::new();
    for raw in src.lines() {
        let line = strip_line_comment(raw.trim());
        if line.is_empty() {
            continue;
        }
        let low = line.to_ascii_lowercase();
        if low.starts_with("graph") || low.starts_with("flowchart") {
            if low.contains("lr") || low.contains("rl") {
                dir = Dir::Lr;
            } else {
                dir = Dir::Td;
            }
            continue;
        }
        if low.starts_with("subgraph")
            || low == "end"
            || low.starts_with("style ")
            || low.starts_with("class")
            || low.starts_with("click ")
            || low.starts_with("linkstyle")
        {
            continue;
        }
        for part in line.split(';') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            if let Some(e) = parse_edge(part, &mut nodes) {
                edges.push(e);
            } else if let Some((node, rest)) = parse_node(part) {
                if rest.trim().is_empty() {
                    nodes.insert(node.id.clone(), node);
                }
            }
        }
    }
    if nodes.is_empty() {
        return None;
    }
    if let Some(lines) = render_path(dir, &nodes, &edges) {
        return Some(lines);
    }
    Some(render_edge_list(&nodes, &edges))
}

fn strip_line_comment(line: &str) -> &str {
    if let Some(i) = line.find("%%") {
        line[..i].trim()
    } else {
        line
    }
}

fn parse_edge(s: &str, nodes: &mut HashMap<String, Node>) -> Option<Edge> {
    for op in ["-->", "-.->", "==>", "---", "--"] {
        if let Some(idx) = find_op(s, op) {
            let (left, mut right) = s.split_at(idx);
            right = &right[op.len()..];
            let mut label = String::new();
            if let Some(rest) = right.strip_prefix('|') {
                if let Some(end) = rest.find('|') {
                    label = rest[..end].trim().to_string();
                    right = rest[end + 1..].trim();
                }
            } else if op == "--" {
                // `A -- lab --> B`
                let arrow = right.find("-->")?;
                label = right[..arrow].trim().to_string();
                right = right[arrow + 3..].trim();
            }
            let (from, _) = parse_node(left.trim())?;
            let (to, _) = parse_node(right.trim())?;
            nodes.entry(from.id.clone()).or_insert_with(|| from.clone());
            if !from.label.is_empty() && from.label != from.id {
                nodes.insert(from.id.clone(), from.clone());
            }
            nodes.entry(to.id.clone()).or_insert_with(|| to.clone());
            if !to.label.is_empty() && to.label != to.id {
                nodes.insert(to.id.clone(), to.clone());
            }
            return Some(Edge {
                from: from.id,
                to: to.id,
                label,
            });
        }
    }
    None
}

fn find_op(s: &str, op: &str) -> Option<usize> {
    let mut in_label = false;
    let mut depth = 0i32;
    let chars: Vec<char> = s.chars().collect();
    let op_c: Vec<char> = op.chars().collect();
    let mut i = 0;
    while i + op_c.len() <= chars.len() {
        let c = chars[i];
        if c == '"' {
            in_label = !in_label;
        }
        if !in_label {
            if "[({".contains(c) {
                depth += 1;
            } else if "])}".contains(c) {
                depth -= 1;
            } else if depth == 0 && chars[i..i + op_c.len()] == op_c[..] {
                return Some(s.char_indices().nth(i)?.0);
            }
        }
        i += 1;
    }
    None
}

fn parse_node(s: &str) -> Option<(Node, &str)> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut i = 0;
    let chars: Vec<char> = s.chars().collect();
    if !chars[0].is_ascii_alphabetic() && chars[0] != '_' {
        return None;
    }
    i += 1;
    while i < chars.len()
        && (chars[i].is_ascii_alphanumeric() || chars[i] == '_' || chars[i] == '-')
    {
        i += 1;
    }
    let id: String = chars[..i].iter().collect();
    let rest_off = s.char_indices().nth(i).map(|(b, _)| b).unwrap_or(s.len());
    let rest = &s[rest_off..];
    let (label, consumed) = parse_shape(rest).unwrap_or((id.clone(), 0));
    let after = rest.get(consumed..).unwrap_or("");
    Some((Node { id, label }, after))
}

fn parse_shape(rest: &str) -> Option<(String, usize)> {
    let rest = rest.trim_start();
    let skip = rest.len().saturating_sub(rest.trim_start().len());
    let r = rest.trim_start();
    let (open, close) = if r.starts_with("(((") {
        return None;
    } else if r.starts_with("(( ") || r.starts_with("((") {
        ("((", "))")
    } else if r.starts_with('[') {
        ("[", "]")
    } else if r.starts_with("{{") {
        ("{{", "}}")
    } else if r.starts_with('{') {
        ("{", "}")
    } else if r.starts_with("([") {
        ("([", "])")
    } else if r.starts_with("[[") {
        ("[[", "]]")
    } else if r.starts_with('(') {
        ("(", ")")
    } else {
        return None;
    };
    let inner = r.get(open.len()..)?;
    let end = if let Some(quoted) = inner.strip_prefix('"') {
        let q = quoted.find('"')?;
        open.len() + 1 + q + 1 + close.len()
    } else {
        let pos = inner.find(close)?;
        open.len() + pos + close.len()
    };
    if end > r.len() {
        return None;
    }
    let raw = &r[open.len()..end - close.len()];
    let label = raw.trim().trim_matches('"').to_string();
    Some((label, skip + end))
}

fn render_path(dir: Dir, nodes: &HashMap<String, Node>, edges: &[Edge]) -> Option<Vec<String>> {
    if edges.is_empty() || nodes.len() != edges.len() + 1 {
        return None;
    }
    let mut outgoing: HashMap<&str, Vec<&Edge>> = HashMap::new();
    let mut indeg: HashMap<&str, usize> = HashMap::new();
    for n in nodes.keys() {
        indeg.insert(n.as_str(), 0);
    }
    for e in edges {
        outgoing.entry(e.from.as_str()).or_default().push(e);
        *indeg.entry(e.to.as_str()).or_default() += 1;
    }
    let roots: Vec<&str> = indeg
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(k, _)| *k)
        .collect();
    if roots.len() != 1 {
        return None;
    }
    let mut order = Vec::new();
    let mut cur = roots[0];
    let mut seen = HashSet::new();
    loop {
        if !seen.insert(cur) {
            return None;
        }
        order.push(cur);
        let outs = outgoing.get(cur).map(|v| v.as_slice()).unwrap_or(&[]);
        if outs.len() > 1 {
            return None;
        }
        if outs.is_empty() {
            break;
        }
        cur = outs[0].to.as_str();
    }
    if order.len() != nodes.len() {
        return None;
    }
    match dir {
        Dir::Td => Some(render_td_path(&order, nodes, edges)),
        Dir::Lr => Some(render_lr_path(&order, nodes, edges)),
    }
}

fn node_label<'a>(nodes: &'a HashMap<String, Node>, id: &'a str) -> &'a str {
    nodes.get(id).map(|n| n.label.as_str()).unwrap_or(id)
}

fn edge_between<'a>(edges: &'a [Edge], from: &str, to: &str) -> Option<&'a Edge> {
    edges.iter().find(|e| e.from == from && e.to == to)
}

fn box_of(label: &str) -> Vec<String> {
    let inner = label.chars().count().max(1);
    let w = inner + 2;
    let pad = format!(" {label} ");
    let pad = {
        let n = pad.chars().count();
        if n < w {
            format!("{pad}{}", " ".repeat(w - n))
        } else {
            pad
        }
    };
    vec![
        format!("┌{}┐", "─".repeat(w)),
        format!("│{pad}│"),
        format!("└{}┘", "─".repeat(w)),
    ]
}

fn render_td_path(order: &[&str], nodes: &HashMap<String, Node>, edges: &[Edge]) -> Vec<String> {
    let boxes: Vec<Vec<String>> = order
        .iter()
        .map(|id| box_of(node_label(nodes, id)))
        .collect();
    let max_w = boxes
        .iter()
        .filter_map(|b| b.first().map(|l| l.chars().count()))
        .max()
        .unwrap_or(3);
    let mut out = Vec::new();
    for (i, b) in boxes.iter().enumerate() {
        let w = b[0].chars().count();
        let left = (max_w.saturating_sub(w)) / 2;
        for row in b {
            out.push(format!("{}{row}", " ".repeat(left)));
        }
        if i + 1 < boxes.len() {
            let mid = max_w / 2;
            if let Some(e) = edge_between(edges, order[i], order[i + 1]) {
                if !e.label.is_empty() {
                    let lab = format!(" {} ", e.label);
                    let lp = mid.saturating_sub(lab.chars().count() / 2);
                    out.push(format!("{}{lab}", " ".repeat(lp)));
                }
            }
            out.push(format!("{}│", " ".repeat(mid)));
            out.push(format!("{}▼", " ".repeat(mid)));
        }
    }
    out
}

fn render_lr_path(order: &[&str], nodes: &HashMap<String, Node>, edges: &[Edge]) -> Vec<String> {
    let boxes: Vec<Vec<String>> = order
        .iter()
        .map(|id| box_of(node_label(nodes, id)))
        .collect();
    let mut top = String::new();
    let mut mid = String::new();
    let mut bot = String::new();
    for (i, b) in boxes.iter().enumerate() {
        top.push_str(&b[0]);
        mid.push_str(&b[1]);
        bot.push_str(&b[2]);
        if i + 1 < boxes.len() {
            let lab = edge_between(edges, order[i], order[i + 1])
                .map(|e| e.label.as_str())
                .unwrap_or("");
            let bridge = if lab.is_empty() {
                " ──► ".to_string()
            } else {
                format!(" ─{lab}─► ")
            };
            let gap = " ".repeat(bridge.chars().count());
            top.push_str(&gap);
            mid.push_str(&bridge);
            bot.push_str(&gap);
        }
    }
    vec![top, mid, bot]
}

fn render_edge_list(nodes: &HashMap<String, Node>, edges: &[Edge]) -> Vec<String> {
    let mut ids: Vec<&String> = nodes.keys().collect();
    ids.sort();
    let mut body = Vec::new();
    for id in ids {
        let n = &nodes[id];
        if n.label == n.id {
            body.push(format!("• {id}"));
        } else {
            body.push(format!("• {id}: {}", n.label));
        }
    }
    for e in edges {
        if e.label.is_empty() {
            body.push(format!("{} ──► {}", e.from, e.to));
        } else {
            body.push(format!("{} ─ {} ─► {}", e.from, e.label, e.to));
        }
    }
    frame_lines(&body, "mermaid flowchart")
}

fn frame_lines(body: &[String], title: &str) -> Vec<String> {
    let w = body
        .iter()
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(0)
        .max(title.chars().count() + 2)
        .min(78);
    let mut out = vec![format!(
        "┌ {title} {}",
        "─".repeat(w.saturating_sub(title.chars().count()).max(1))
    )];
    for line in body {
        out.push(format!("│ {line}"));
    }
    out.push(format!("└{}", "─".repeat(w + 2)));
    out
}

fn render_sequence(src: &str) -> Option<Vec<String>> {
    let mut names: Vec<String> = Vec::new();
    let mut alias: HashMap<String, String> = HashMap::new();
    let mut msgs = Vec::new();
    for raw in src.lines() {
        let line = strip_line_comment(raw.trim());
        if line.is_empty() {
            continue;
        }
        let low = line.to_ascii_lowercase();
        if low.starts_with("sequencediagram") {
            continue;
        }
        if low.starts_with("participant ") || low.starts_with("actor ") {
            let rest = line.split_once(' ').map(|(_, r)| r.trim()).unwrap_or("");
            if let Some((id, as_name)) = rest.split_once(" as ") {
                let id = id.trim().to_string();
                let as_name = as_name.trim().to_string();
                alias.insert(id.clone(), as_name.clone());
                push_unique(&mut names, as_name);
            } else if !rest.is_empty() {
                push_unique(&mut names, rest.to_string());
            }
            continue;
        }
        if let Some(msg) = parse_seq_msg(line) {
            push_unique(&mut names, display_name(&alias, &msg.from));
            push_unique(&mut names, display_name(&alias, &msg.to));
            msgs.push(msg);
        }
    }
    if names.is_empty() || msgs.is_empty() {
        return None;
    }
    Some(draw_sequence(&names, &alias, &msgs))
}

fn push_unique(v: &mut Vec<String>, s: String) {
    if !v.iter().any(|x| x == &s) {
        v.push(s);
    }
}

fn display_name(alias: &HashMap<String, String>, id: &str) -> String {
    alias.get(id).cloned().unwrap_or_else(|| id.to_string())
}

fn parse_seq_msg(line: &str) -> Option<SeqMsg> {
    for (op, dashed) in [("-->>", true), ("->>", false), ("-->", true), ("->", false)] {
        if let Some(idx) = line.find(op) {
            let from = line[..idx].trim().to_string();
            let rest = line[idx + op.len()..].trim();
            let (to, text) = rest
                .split_once(':')
                .map(|(t, m)| (t.trim().to_string(), m.trim().to_string()))
                .unwrap_or_else(|| (rest.to_string(), String::new()));
            if from.is_empty() || to.is_empty() {
                return None;
            }
            return Some(SeqMsg {
                from,
                to,
                text,
                dashed,
            });
        }
    }
    None
}

fn draw_sequence(
    names: &[String],
    alias: &HashMap<String, String>,
    msgs: &[SeqMsg],
) -> Vec<String> {
    let col_w: Vec<usize> = names.iter().map(|n| n.chars().count().max(8) + 2).collect();
    let mut header = String::new();
    for (i, n) in names.iter().enumerate() {
        header.push_str(&format!("{:^w$}", n, w = col_w[i]));
    }
    let mut out = vec![header];
    let lifeline = {
        let mut s = String::new();
        for w in &col_w {
            let mid = w / 2;
            s.push_str(&format!("{}│{}", " ".repeat(mid), " ".repeat(w - mid - 1)));
        }
        s
    };
    out.push(lifeline.clone());
    for m in msgs {
        out.push(lifeline.clone());
        let Some(a) = index_of(names, &display_name(alias, &m.from)) else {
            continue;
        };
        let Some(b) = index_of(names, &display_name(alias, &m.to)) else {
            continue;
        };
        out.push(seq_arrow(&col_w, a, b, &m.text, m.dashed));
    }
    out
}

fn index_of(names: &[String], n: &str) -> Option<usize> {
    names.iter().position(|x| x == n)
}

fn seq_arrow(col_w: &[usize], from: usize, to: usize, text: &str, dashed: bool) -> String {
    let pos = |i: usize| -> usize { col_w[..i].iter().sum::<usize>() + col_w[i] / 2 };
    let a = pos(from);
    let b = pos(to);
    let (left, right, fwd) = if a <= b { (a, b, true) } else { (b, a, false) };
    let span = right.saturating_sub(left).max(2);
    let fill = if dashed { '╌' } else { '─' };
    let label: String = text.chars().take(span.saturating_sub(2)).collect();
    let inner = if label.is_empty() {
        fill.to_string().repeat(span.saturating_sub(1))
    } else {
        let room = span.saturating_sub(label.chars().count() + 1);
        let l = room / 2;
        let r = room.saturating_sub(l);
        format!(
            "{}{label}{}",
            fill.to_string().repeat(l),
            fill.to_string().repeat(r)
        )
    };
    let body = if fwd {
        format!(
            "{}►",
            inner
                .chars()
                .take(inner.chars().count().saturating_sub(1))
                .collect::<String>()
        )
    } else {
        format!("◄{}", inner.chars().skip(1).collect::<String>())
    };
    let mut chars: Vec<char> = std::iter::repeat_n(' ', right + 1).collect();
    for (i, ch) in body.chars().enumerate() {
        if left + i < chars.len() {
            chars[left + i] = ch;
        }
    }
    chars.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flowchart_path_boxes_labels() {
        let lines = render_mermaid("graph TD\n  A[Hello] --> B[World]\n");
        let text = lines.join("\n");
        assert!(text.contains("Hello"), "{text}");
        assert!(text.contains("World"), "{text}");
        assert!(text.contains("┌"), "{text}");
        assert!(text.contains("▼"), "{text}");
        assert!(!text.contains("graph TD"), "{text}");
    }

    #[test]
    fn flowchart_lr_uses_arrow() {
        let text = render_mermaid("flowchart LR\n  A[L] --> B[R]\n").join("\n");
        assert!(text.contains("L") && text.contains("R"), "{text}");
        assert!(text.contains("►"), "{text}");
    }

    #[test]
    fn sequence_actors_and_message() {
        let text =
            render_mermaid("sequenceDiagram\n  Alice->>Bob: hi\n  Bob-->>Alice: yo\n").join("\n");
        assert!(text.contains("Alice"), "{text}");
        assert!(text.contains("Bob"), "{text}");
        assert!(text.contains("hi"), "{text}");
        assert!(text.contains("►") || text.contains("◄"), "{text}");
    }

    #[test]
    fn unknown_diagram_frames_source() {
        let text = render_mermaid("pie title Pets\n  \"Dogs\" : 1\n").join("\n");
        assert!(text.contains("mermaid"), "{text}");
        assert!(text.contains("pie title Pets"), "{text}");
        assert!(text.contains("┌"), "{text}");
    }

    #[test]
    fn lang_detect() {
        assert!(is_mermaid_lang("mermaid"));
        assert!(is_mermaid_lang("Mermaid"));
        assert!(!is_mermaid_lang("rust"));
    }
}
