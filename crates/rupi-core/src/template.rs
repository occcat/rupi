//! 提示模板展开：`$ARGUMENTS` / `$1` 与 `{{var}}`（对标 Pi prompts）。
//!
//! - `$ARGUMENTS` / `$@`：用户参数原文（去首尾空白）
//! - `$1` `$2` …：位置参数（跳过 `name=value` / `--name`）
//! - `${1:-default}` / `${ARGUMENTS:-default}`：缺省值
//! - `{{name}}` / `{{name:-default}}`：命名变量（`name=value` 或 `--name value`）
//! - `{{arguments}}` / `{{args}}`：剩余位置参数；若无位置参数则回退到原文
//!
//! 正文不含上述占位符时：有参数则追加到末尾（旧行为）。

use std::collections::HashMap;

/// 解析后的模板变量。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateVars {
    pub named: HashMap<String, String>,
    pub positional: Vec<String>,
    /// `$ARGUMENTS` / `$@` 用的原文。
    pub raw: String,
}

impl TemplateVars {
    pub fn leftover(&self) -> String {
        self.positional.join(" ")
    }

    pub fn arguments_mustache(&self) -> String {
        let left = self.leftover();
        if left.is_empty() {
            self.raw.clone()
        } else {
            left
        }
    }
}

/// 展开模板正文。
pub fn expand(body: &str, args: &str) -> String {
    if !has_placeholders(body) {
        if args.is_empty() {
            return body.to_string();
        }
        return format!("{body}\n\n{args}");
    }
    let vars = parse_args(args);
    render(body, &vars)
}

/// 是否含可展开占位符。
pub fn has_placeholders(body: &str) -> bool {
    body.contains("$ARGUMENTS")
        || body.contains("$@")
        || body.contains("{{")
        || body.contains("${")
        || dollar_positional(body)
}

fn dollar_positional(body: &str) -> bool {
    let b = body.as_bytes();
    let mut i = 0;
    while i + 1 < b.len() {
        if b[i] == b'$' && b[i + 1].is_ascii_digit() {
            return true;
        }
        i += 1;
    }
    false
}

/// 把参数拆成命名 + 位置参数。
pub fn parse_args(args: &str) -> TemplateVars {
    let tokens = tokenize(args);
    let mut named = HashMap::new();
    let mut positional = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let t = &tokens[i];
        if let Some(rest) = t.strip_prefix("--") {
            if let Some((k, v)) = rest.split_once('=') {
                if is_ident(k) {
                    named.insert(k.to_ascii_lowercase(), v.to_string());
                    i += 1;
                    continue;
                }
            } else if is_ident(rest) {
                if i + 1 < tokens.len() && !tokens[i + 1].starts_with('-') {
                    named.insert(rest.to_ascii_lowercase(), tokens[i + 1].clone());
                    i += 2;
                    continue;
                }
                named.insert(rest.to_ascii_lowercase(), "true".into());
                i += 1;
                continue;
            }
        }
        if let Some((k, v)) = t.split_once('=') {
            if is_ident(k) {
                named.insert(k.to_ascii_lowercase(), v.to_string());
                i += 1;
                continue;
            }
        }
        positional.push(t.clone());
        i += 1;
    }
    TemplateVars {
        named,
        positional,
        raw: args.trim().to_string(),
    }
}

fn is_ident(s: &str) -> bool {
    let mut cs = s.chars();
    match cs.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    cs.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn tokenize(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else if c == '\\' {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            } else {
                cur.push(c);
            }
        } else if c == '"' || c == '\'' {
            quote = Some(c);
        } else if c.is_whitespace() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn render(body: &str, vars: &TemplateVars) -> String {
    let chars: Vec<char> = body.chars().collect();
    let mut out = String::with_capacity(body.len() + vars.raw.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '{' && i + 1 < chars.len() && chars[i + 1] == '{' {
            if let Some((end, inner)) = take_delimited(&chars, i + 2, '}', '}') {
                out.push_str(&resolve_mustache(&inner, vars));
                i = end;
                continue;
            }
        }
        if chars[i] == '$' {
            if let Some((end, val)) = take_dollar(&chars, i, vars) {
                out.push_str(&val);
                i = end;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn take_delimited(chars: &[char], start: usize, a: char, b: char) -> Option<(usize, String)> {
    let mut j = start;
    while j + 1 < chars.len() {
        if chars[j] == a && chars[j + 1] == b {
            let inner: String = chars[start..j].iter().collect();
            return Some((j + 2, inner));
        }
        j += 1;
    }
    None
}

fn split_default(inner: &str) -> (&str, Option<&str>) {
    if let Some((k, d)) = inner.split_once(":-") {
        (k.trim(), Some(d))
    } else {
        (inner.trim(), None)
    }
}

fn resolve_mustache(inner: &str, vars: &TemplateVars) -> String {
    let (key, default) = split_default(inner);
    lookup(key, vars).unwrap_or_else(|| default.unwrap_or("").to_string())
}

fn lookup(key: &str, vars: &TemplateVars) -> Option<String> {
    if key.eq_ignore_ascii_case("arguments") || key.eq_ignore_ascii_case("args") {
        return Some(vars.arguments_mustache());
    }
    if let Ok(n) = key.parse::<usize>() {
        if n >= 1 {
            return vars.positional.get(n - 1).cloned();
        }
    }
    vars.named.get(&key.to_ascii_lowercase()).cloned()
}

fn take_dollar(chars: &[char], i: usize, vars: &TemplateVars) -> Option<(usize, String)> {
    let rest: String = chars[i + 1..].iter().collect();
    if rest.starts_with("ARGUMENTS") {
        let after = i + 1 + "ARGUMENTS".len();
        // 避免 $ARGUMENTS_FOO 被误切
        if after < chars.len() && (chars[after].is_ascii_alphanumeric() || chars[after] == '_') {
            return None;
        }
        return Some((after, vars.raw.clone()));
    }
    if rest.starts_with('@') {
        return Some((i + 2, vars.raw.clone()));
    }
    if rest.starts_with('{') {
        if let Some((end, inner)) = take_braces(chars, i + 2) {
            return Some((end, resolve_dollar_braces(&inner, vars)));
        }
        return None;
    }
    if rest.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        let mut j = i + 1;
        while j < chars.len() && chars[j].is_ascii_digit() {
            j += 1;
        }
        let n: usize = chars[i + 1..j].iter().collect::<String>().parse().ok()?;
        let val = if n >= 1 {
            vars.positional.get(n - 1).cloned().unwrap_or_default()
        } else {
            String::new()
        };
        // `$1` 在缺省值语法里走 `${1:-x}`；裸 `$1` 未提供则空串。
        return Some((j, val));
    }
    None
}

fn take_braces(chars: &[char], start: usize) -> Option<(usize, String)> {
    let mut j = start;
    while j < chars.len() {
        if chars[j] == '}' {
            let inner: String = chars[start..j].iter().collect();
            return Some((j + 1, inner));
        }
        j += 1;
    }
    None
}

fn resolve_dollar_braces(inner: &str, vars: &TemplateVars) -> String {
    let (key, default) = split_default(inner);
    if key == "@" || key.eq_ignore_ascii_case("ARGUMENTS") {
        if vars.raw.is_empty() {
            return default.unwrap_or("").to_string();
        }
        return vars.raw.clone();
    }
    if let Some(rest) = key.strip_prefix("@:") {
        // ${@:N} / ${@:N:L}
        let mut parts = rest.split(':');
        let start: usize = parts.next().unwrap_or("1").parse().unwrap_or(1);
        let len: Option<usize> = parts.next().and_then(|s| s.parse().ok());
        if start >= 1 {
            let idx = start - 1;
            let slice = if idx >= vars.positional.len() {
                &[][..]
            } else if let Some(l) = len {
                let end = (idx + l).min(vars.positional.len());
                &vars.positional[idx..end]
            } else {
                &vars.positional[idx..]
            };
            let joined = slice.join(" ");
            if joined.is_empty() {
                return default.unwrap_or("").to_string();
            }
            return joined;
        }
    }
    lookup(key, vars).unwrap_or_else(|| default.unwrap_or("").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_placeholder_appends_args() {
        assert_eq!(expand("Review.", "all"), "Review.\n\nall");
        assert_eq!(expand("Review.", ""), "Review.");
    }

    #[test]
    fn arguments_and_positional() {
        assert_eq!(
            expand("Fix: $ARGUMENTS", "null pointer"),
            "Fix: null pointer"
        );
        assert_eq!(
            expand("A $1 B $2", r#"Button "click handler""#),
            "A Button B click handler"
        );
        assert_eq!(expand("all=$@", "x y"), "all=x y");
        assert_eq!(expand("${1:-7} bullets", ""), "7 bullets");
        assert_eq!(expand("${ARGUMENTS:-none}", ""), "none");
        assert_eq!(expand("${@:2}", "a b c"), "b c");
    }

    #[test]
    fn mustache_named_and_defaults() {
        assert_eq!(
            expand(
                "Focus: {{focus}}\n{{arguments}}",
                "focus=security extra notes"
            ),
            "Focus: security\nextra notes"
        );
        assert_eq!(expand("Focus: {{focus:-general}}", ""), "Focus: general");
        assert_eq!(expand("n={{1}}", r#""quoted name""#), "n=quoted name");
        assert_eq!(expand("{{missing}}", ""), "");
        assert_eq!(expand("x={{flag}}", "--flag"), "x=true");
        assert_eq!(expand("{{theme}}", "--theme dark"), "dark");
    }

    #[test]
    fn dollar_ten_is_not_dollar_one() {
        let vars = parse_args("a b c d e f g h i ten");
        assert_eq!(vars.positional.len(), 10);
        assert_eq!(expand("$10", "a b c d e f g h i ten"), "ten");
        assert_eq!(expand("$1x", "Z"), "Zx");
    }
}
