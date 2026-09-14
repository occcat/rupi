//! 围栏代码语法高亮（对标 Pi 的 9 个 syntax* token，无 syntect）。

use crate::theme::Theme;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line as RLine, Span};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Text,
    Comment,
    Keyword,
    Function,
    Variable,
    String,
    Number,
    Type,
    Operator,
    Punctuation,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lang {
    Rust,
    Python,
    Js,
    Go,
    Json,
    Bash,
    Toml,
    Yaml,
    Sql,
    Html,
    Css,
    C,
    Java,
    Ruby,
    Generic,
}

impl Lang {
    fn from_fence(lang: &str) -> Self {
        match lang.trim().to_ascii_lowercase().as_str() {
            "rs" | "rust" => Self::Rust,
            "py" | "python" => Self::Python,
            "js" | "javascript" | "jsx" | "ts" | "tsx" | "typescript" => Self::Js,
            "go" | "golang" => Self::Go,
            "json" | "jsonc" => Self::Json,
            "bash" | "sh" | "zsh" | "shell" => Self::Bash,
            "toml" => Self::Toml,
            "yaml" | "yml" => Self::Yaml,
            "sql" => Self::Sql,
            "html" | "xml" => Self::Html,
            "css" | "scss" => Self::Css,
            "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" => Self::C,
            "java" | "kt" | "kotlin" => Self::Java,
            "rb" | "ruby" => Self::Ruby,
            _ => Self::Generic,
        }
    }

    fn keywords(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &[
                "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else",
                "enum", "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match",
                "mod", "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct",
                "super", "trait", "true", "type", "unsafe", "use", "where", "while",
            ],
            Self::Python => &[
                "and", "as", "assert", "async", "await", "break", "class", "continue", "def",
                "del", "elif", "else", "except", "False", "finally", "for", "from", "global", "if",
                "import", "in", "is", "lambda", "None", "not", "or", "pass", "raise", "return",
                "True", "try", "while", "with", "yield",
            ],
            Self::Js => &[
                "async",
                "await",
                "break",
                "case",
                "catch",
                "class",
                "const",
                "continue",
                "debugger",
                "default",
                "delete",
                "do",
                "else",
                "export",
                "extends",
                "false",
                "finally",
                "for",
                "function",
                "if",
                "import",
                "in",
                "instanceof",
                "let",
                "new",
                "null",
                "return",
                "static",
                "super",
                "switch",
                "this",
                "throw",
                "true",
                "try",
                "typeof",
                "undefined",
                "var",
                "void",
                "while",
                "with",
                "yield",
                "of",
            ],
            Self::Go => &[
                "break",
                "case",
                "chan",
                "const",
                "continue",
                "default",
                "defer",
                "else",
                "fallthrough",
                "for",
                "func",
                "go",
                "goto",
                "if",
                "import",
                "interface",
                "map",
                "package",
                "range",
                "return",
                "select",
                "struct",
                "switch",
                "type",
                "var",
            ],
            Self::Sql => &[
                "select",
                "from",
                "where",
                "and",
                "or",
                "insert",
                "into",
                "values",
                "update",
                "set",
                "delete",
                "create",
                "table",
                "index",
                "join",
                "left",
                "right",
                "inner",
                "on",
                "as",
                "order",
                "by",
                "group",
                "having",
                "limit",
                "offset",
                "not",
                "null",
                "primary",
                "key",
                "foreign",
                "references",
                "unique",
                "boolean",
                "text",
            ],
            Self::C => &[
                "auto",
                "break",
                "case",
                "const",
                "continue",
                "default",
                "do",
                "else",
                "enum",
                "extern",
                "for",
                "goto",
                "if",
                "inline",
                "register",
                "return",
                "sizeof",
                "static",
                "struct",
                "switch",
                "typedef",
                "union",
                "volatile",
                "while",
                "class",
                "namespace",
                "template",
                "typename",
                "using",
                "public",
                "private",
                "protected",
                "virtual",
                "new",
                "delete",
                "this",
                "true",
                "false",
                "nullptr",
            ],
            Self::Java => &[
                "abstract",
                "assert",
                "break",
                "case",
                "catch",
                "class",
                "const",
                "continue",
                "default",
                "do",
                "else",
                "enum",
                "extends",
                "final",
                "finally",
                "for",
                "if",
                "implements",
                "import",
                "instanceof",
                "interface",
                "native",
                "new",
                "package",
                "private",
                "protected",
                "public",
                "return",
                "static",
                "super",
                "switch",
                "synchronized",
                "this",
                "throw",
                "throws",
                "transient",
                "try",
                "void",
                "volatile",
                "while",
                "true",
                "false",
                "null",
            ],
            Self::Ruby => &[
                "alias", "and", "begin", "break", "case", "class", "def", "defined?", "do", "else",
                "elsif", "end", "ensure", "false", "for", "if", "in", "module", "next", "nil",
                "not", "or", "redo", "rescue", "retry", "return", "self", "super", "then", "true",
                "undef", "unless", "until", "when", "while", "yield",
            ],
            Self::Bash => &[
                "if", "then", "else", "elif", "fi", "for", "in", "do", "done", "while", "until",
                "case", "esac", "function", "return", "local", "export", "unset", "readonly",
                "true", "false",
            ],
            Self::Css => &["important", "from", "to", "and", "or", "not", "only"],
            Self::Json => &["true", "false", "null"],
            Self::Toml | Self::Yaml | Self::Html | Self::Generic => &[],
        }
    }

    fn types(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &[
                "bool", "char", "str", "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16",
                "i32", "i64", "i128", "isize", "f32", "f64", "String", "Vec", "Option", "Result",
                "Box", "Self",
            ],
            Self::Go => &[
                "bool",
                "byte",
                "complex64",
                "complex128",
                "error",
                "float32",
                "float64",
                "int",
                "int8",
                "int16",
                "int32",
                "int64",
                "rune",
                "string",
                "uint",
                "uint8",
                "uint16",
                "uint32",
                "uint64",
                "uintptr",
            ],
            Self::C => &[
                "void", "int", "char", "short", "long", "float", "double", "unsigned", "signed",
                "bool", "size_t",
            ],
            Self::Java => &[
                "void", "boolean", "byte", "char", "short", "int", "long", "float", "double",
                "String",
            ],
            _ => &[],
        }
    }

    fn line_comment(self) -> Option<&'static str> {
        match self {
            Self::Python | Self::Bash | Self::Toml | Self::Yaml | Self::Ruby => Some("#"),
            Self::Sql => Some("--"),
            Self::Html | Self::Generic => None,
            _ => Some("//"),
        }
    }

    fn block_comment(self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::Python | Self::Bash | Self::Toml | Self::Yaml | Self::Json | Self::Ruby => None,
            Self::Html => Some(("<!--", "-->")),
            Self::Generic => None,
            _ => Some(("/*", "*/")),
        }
    }
}

pub fn highlight(code: &str, lang: &str, theme: &Theme) -> Vec<RLine<'static>> {
    tokenize(code, lang)
        .into_iter()
        .map(|row| {
            let spans: Vec<Span<'static>> = row
                .into_iter()
                .map(|(text, kind)| Span::styled(text, style_of(kind, theme)))
                .collect();
            if spans.is_empty() {
                RLine::from("")
            } else {
                RLine::from(spans)
            }
        })
        .collect()
}

pub fn tokenize(code: &str, lang: &str) -> Vec<Vec<(String, TokenKind)>> {
    let lang = Lang::from_fence(lang);
    let mut out = Vec::new();
    let mut in_block = false;
    for raw in code.lines() {
        let (row, next) = tokenize_line(raw, lang, in_block);
        out.push(row);
        in_block = next;
    }
    if out.is_empty() {
        out.push(Vec::new());
    }
    out
}

fn style_of(kind: TokenKind, theme: &Theme) -> Style {
    let s = Style::default();
    match kind {
        TokenKind::Text => s.fg(theme.assistant),
        TokenKind::Comment => s.fg(theme.syntax_comment).add_modifier(Modifier::ITALIC),
        TokenKind::Keyword => s.fg(theme.syntax_keyword).add_modifier(Modifier::BOLD),
        TokenKind::Function => s.fg(theme.syntax_function),
        TokenKind::Variable => s.fg(theme.syntax_variable),
        TokenKind::String => s.fg(theme.syntax_string),
        TokenKind::Number => s.fg(theme.syntax_number),
        TokenKind::Type => s.fg(theme.syntax_type),
        TokenKind::Operator => s.fg(theme.syntax_operator),
        TokenKind::Punctuation => s.fg(theme.syntax_punctuation),
    }
}

fn tokenize_line(line: &str, lang: Lang, mut in_block: bool) -> (Vec<(String, TokenKind)>, bool) {
    if lang == Lang::Json {
        return (tokenize_json(line), false);
    }
    if lang == Lang::Html {
        return (tokenize_html(line, in_block), in_block);
    }
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    let mut spans = Vec::new();
    let line_cmt = lang.line_comment();
    let block = lang.block_comment();

    if in_block {
        if let Some((_, end)) = block {
            if let Some(pos) = find_str(&chars, 0, end) {
                push(
                    &mut spans,
                    chars_str(&chars[0..pos + end.chars().count()]),
                    TokenKind::Comment,
                );
                i = pos + end.chars().count();
                in_block = false;
            } else {
                push(&mut spans, line.to_string(), TokenKind::Comment);
                return (spans, true);
            }
        }
    }

    while i < chars.len() {
        if let Some((start, end)) = block {
            if starts_at(&chars, i, start) {
                if let Some(pos) = find_str(&chars, i + start.chars().count(), end) {
                    let j = pos + end.chars().count();
                    push(&mut spans, chars_str(&chars[i..j]), TokenKind::Comment);
                    i = j;
                    continue;
                }
                push(&mut spans, chars_str(&chars[i..]), TokenKind::Comment);
                return (spans, true);
            }
        }
        if let Some(cmt) = line_cmt {
            if starts_at(&chars, i, cmt) {
                push(&mut spans, chars_str(&chars[i..]), TokenKind::Comment);
                break;
            }
        }
        let c = chars[i];
        if c == '"' || c == '\'' || (c == '`' && matches!(lang, Lang::Js | Lang::Generic)) {
            let (s, j) = take_string(&chars, i, c);
            push(&mut spans, s, TokenKind::String);
            i = j;
            continue;
        }
        if c.is_ascii_digit() {
            let j = take_while(&chars, i, |ch| {
                ch.is_ascii_hexdigit()
                    || ch == '.'
                    || ch == '_'
                    || ch == 'x'
                    || ch == 'b'
                    || ch == 'o'
            });
            push(&mut spans, chars_str(&chars[i..j]), TokenKind::Number);
            i = j;
            continue;
        }
        if is_ident_start(c) {
            let j = take_while(&chars, i, is_ident);
            let word = chars_str(&chars[i..j]);
            let kind = classify_ident(&word, lang, &chars, j);
            push(&mut spans, word, kind);
            i = j;
            continue;
        }
        if "{}[]()".contains(c) || c == ',' || c == ';' || c == '.' {
            push(&mut spans, c.to_string(), TokenKind::Punctuation);
            i += 1;
            continue;
        }
        if "+-*/%=!<>&|^~?:".contains(c) {
            let j = take_while(&chars, i, |ch| "+-*/%=!<>&|^~?:".contains(ch));
            push(&mut spans, chars_str(&chars[i..j]), TokenKind::Operator);
            i = j;
            continue;
        }
        if c.is_whitespace() {
            let j = take_while(&chars, i, |ch| ch.is_whitespace());
            push(&mut spans, chars_str(&chars[i..j]), TokenKind::Text);
            i = j;
            continue;
        }
        push(&mut spans, c.to_string(), TokenKind::Text);
        i += 1;
    }
    (spans, in_block)
}

fn classify_ident(word: &str, lang: Lang, chars: &[char], after: usize) -> TokenKind {
    if lang.keywords().contains(&word) {
        return TokenKind::Keyword;
    }
    if lang == Lang::Sql && lang.keywords().iter().any(|k| k.eq_ignore_ascii_case(word)) {
        return TokenKind::Keyword;
    }
    if lang.types().contains(&word) {
        return TokenKind::Type;
    }
    let mut k = after;
    while k < chars.len() && chars[k].is_whitespace() {
        k += 1;
    }
    if k < chars.len() && chars[k] == '(' {
        return TokenKind::Function;
    }
    if word.chars().next().is_some_and(|c| c.is_uppercase())
        && matches!(
            lang,
            Lang::Rust | Lang::Js | Lang::Go | Lang::Java | Lang::C | Lang::Python
        )
    {
        return TokenKind::Type;
    }
    TokenKind::Variable
}

fn tokenize_json(line: &str) -> Vec<(String, TokenKind)> {
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    let mut spans = Vec::new();
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            let j = take_while(&chars, i, |ch| ch.is_whitespace());
            push(&mut spans, chars_str(&chars[i..j]), TokenKind::Text);
            i = j;
            continue;
        }
        if c == '"' {
            let (s, j) = take_string(&chars, i, '"');
            let mut k = j;
            while k < chars.len() && chars[k].is_whitespace() {
                k += 1;
            }
            let kind = if k < chars.len() && chars[k] == ':' {
                TokenKind::Variable
            } else {
                TokenKind::String
            };
            push(&mut spans, s, kind);
            i = j;
            continue;
        }
        if c.is_ascii_digit() || (c == '-' && i + 1 < chars.len() && chars[i + 1].is_ascii_digit())
        {
            let start = i;
            if c == '-' {
                i += 1;
            }
            let j = take_while(&chars, i, |ch| {
                ch.is_ascii_digit() || ch == '.' || ch == 'e' || ch == 'E' || ch == '+' || ch == '-'
            });
            push(&mut spans, chars_str(&chars[start..j]), TokenKind::Number);
            i = j;
            continue;
        }
        if is_ident_start(c) {
            let j = take_while(&chars, i, is_ident);
            let word = chars_str(&chars[i..j]);
            let kind = if matches!(word.as_str(), "true" | "false" | "null") {
                TokenKind::Keyword
            } else {
                TokenKind::Text
            };
            push(&mut spans, word, kind);
            i = j;
            continue;
        }
        if "{}[]:,".contains(c) {
            push(&mut spans, c.to_string(), TokenKind::Punctuation);
            i += 1;
            continue;
        }
        push(&mut spans, c.to_string(), TokenKind::Text);
        i += 1;
    }
    spans
}

fn tokenize_html(line: &str, _in_block: bool) -> Vec<(String, TokenKind)> {
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    let mut spans = Vec::new();
    while i < chars.len() {
        if starts_at(&chars, i, "<!--") {
            if let Some(pos) = find_str(&chars, i + 4, "-->") {
                let j = pos + 3;
                push(&mut spans, chars_str(&chars[i..j]), TokenKind::Comment);
                i = j;
            } else {
                push(&mut spans, chars_str(&chars[i..]), TokenKind::Comment);
                break;
            }
            continue;
        }
        if chars[i] == '<' {
            let j = take_while(&chars, i, |ch| ch != '>' && !ch.is_whitespace());
            push(&mut spans, chars_str(&chars[i..j]), TokenKind::Keyword);
            i = j;
            continue;
        }
        if chars[i] == '"' || chars[i] == '\'' {
            let (s, j) = take_string(&chars, i, chars[i]);
            push(&mut spans, s, TokenKind::String);
            i = j;
            continue;
        }
        if is_ident_start(chars[i]) {
            let j = take_while(&chars, i, is_ident);
            push(&mut spans, chars_str(&chars[i..j]), TokenKind::Variable);
            i = j;
            continue;
        }
        push(&mut spans, chars[i].to_string(), TokenKind::Punctuation);
        i += 1;
    }
    spans
}

fn take_string(chars: &[char], start: usize, quote: char) -> (String, usize) {
    let mut i = start + 1;
    while i < chars.len() {
        if chars[i] == '\\' && i + 1 < chars.len() {
            i += 2;
            continue;
        }
        if chars[i] == quote {
            i += 1;
            break;
        }
        i += 1;
    }
    (chars_str(&chars[start..i]), i)
}

fn take_while(chars: &[char], start: usize, pred: impl Fn(char) -> bool) -> usize {
    let mut i = start;
    while i < chars.len() && pred(chars[i]) {
        i += 1;
    }
    i
}

fn starts_at(chars: &[char], i: usize, s: &str) -> bool {
    let needle: Vec<char> = s.chars().collect();
    if i + needle.len() > chars.len() {
        return false;
    }
    chars[i..i + needle.len()] == needle[..]
}

fn find_str(chars: &[char], from: usize, s: &str) -> Option<usize> {
    let needle: Vec<char> = s.chars().collect();
    if needle.is_empty() {
        return Some(from);
    }
    let mut i = from;
    while i + needle.len() <= chars.len() {
        if chars[i..i + needle.len()] == needle[..] {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || c == '$'
}

fn is_ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$'
}

fn chars_str(chars: &[char]) -> String {
    chars.iter().collect()
}

fn push(spans: &mut Vec<(String, TokenKind)>, text: String, kind: TokenKind) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = spans.last_mut() {
        if last.1 == kind {
            last.0.push_str(&text);
            return;
        }
    }
    spans.push((text, kind));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(code: &str, lang: &str) -> Vec<(String, TokenKind)> {
        tokenize(code, lang).into_iter().flatten().collect()
    }

    fn first_kind(code: &str, lang: &str, needle: &str) -> TokenKind {
        kinds(code, lang)
            .into_iter()
            .find(|(t, _)| t == needle)
            .unwrap_or_else(|| panic!("missing {needle} in {code}"))
            .1
    }

    #[test]
    fn rust_keywords_strings_and_fn() {
        let src = "fn hello() { let x = \"hi\"; }";
        assert_eq!(first_kind(src, "rust", "fn"), TokenKind::Keyword);
        assert_eq!(first_kind(src, "rust", "hello"), TokenKind::Function);
        assert_eq!(first_kind(src, "rust", "let"), TokenKind::Keyword);
        assert_eq!(first_kind(src, "rust", "\"hi\""), TokenKind::String);
    }

    #[test]
    fn python_comment_and_def() {
        let src = "# hi\ndef foo():\n    return 1";
        assert_eq!(first_kind(src, "python", "# hi"), TokenKind::Comment);
        assert_eq!(first_kind(src, "python", "def"), TokenKind::Keyword);
        assert_eq!(first_kind(src, "python", "foo"), TokenKind::Function);
        assert_eq!(first_kind(src, "python", "1"), TokenKind::Number);
    }

    #[test]
    fn json_keys_and_values() {
        let src = r#"{"a": 2, "b": true}"#;
        assert_eq!(first_kind(src, "json", "\"a\""), TokenKind::Variable);
        assert_eq!(first_kind(src, "json", "2"), TokenKind::Number);
        assert_eq!(first_kind(src, "json", "true"), TokenKind::Keyword);
    }

    #[test]
    fn block_comment_spans_lines() {
        let rows = tokenize("a /*\nb\n*/ c", "rust");
        assert!(rows[0].iter().any(|(_, k)| *k == TokenKind::Comment));
        assert!(rows[1].iter().all(|(_, k)| *k == TokenKind::Comment));
        assert!(rows[2]
            .iter()
            .any(|(t, k)| t.contains("c") && *k == TokenKind::Variable));
    }

    #[test]
    fn highlight_uses_distinct_theme_colors() {
        let theme = Theme::dark();
        let lines = highlight("fn x() { let s = \"z\"; }", "rust", &theme);
        let mut saw_kw = false;
        let mut saw_str = false;
        for line in &lines {
            for s in &line.spans {
                if s.content.as_ref() == "fn" {
                    assert_eq!(s.style.fg, Some(theme.syntax_keyword));
                    saw_kw = true;
                }
                if s.content.as_ref() == "\"z\"" {
                    assert_eq!(s.style.fg, Some(theme.syntax_string));
                    saw_str = true;
                }
            }
        }
        assert!(saw_kw && saw_str);
    }
}
