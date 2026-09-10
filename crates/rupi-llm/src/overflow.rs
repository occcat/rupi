//! 上下文溢出检测（对标上游 `pi-ai/utils/overflow.ts` 的错误文本路径）。
//!
//! 主循环阈值压实管“渐进变长”，这里管“一步撑爆”：provider 直接 400/413 报错时，
//! 靠错误文本识别出溢出，触发强制压实 + 重试，而不是把整轮 abort。
//! usage 口径的静默溢出（z.ai / MiMo 系）需要回包 usage  plumbing，
//! 当前 `ChatResponse` 无 usage 字段，暂不覆盖——三家原生 provider 溢出都走显式报错。

use std::sync::OnceLock;

const OVERFLOW_PATTERNS: &[&str] = &[
    r"prompt is too long",
    r"request_too_large",
    r"input is too long for requested model",
    r"exceeds the context window",
    r"exceeds (?:the )?(?:model'?s )?maximum context length(?: of [\d,]+ tokens?|\s*\([\d,]+\))",
    r"input token count.*exceeds the maximum",
    r"maximum prompt length is \d+",
    r"reduce the length of the messages",
    r"maximum context length is \d+ tokens",
    r"exceeds (?:the )?maximum allowed input length of [\d,]+ tokens?",
    r"input \(\d+ tokens\) is longer than the model'?s context length \(\d+ tokens\)",
    r"exceeds the limit of \d+",
    r"exceeds the available context size",
    r"greater than the context length",
    r"context window exceeds limit",
    r"exceeded model token limit",
    r"too large for model with \d+ maximum context length",
    r"prompt has [\d,]+ tokens?, but the configured context size is [\d,]+ tokens?",
    r"model_context_window_exceeded",
    r"prompt too long; exceeded (?:max )?context length",
    r"range of input length should be",
    r"context[_ ]length[_ ]exceeded",
    r"too many tokens",
    r"token limit exceeded",
    r"^4(?:00|13)\s*(?:status code)?\s*\(no body\)",
];

const NON_OVERFLOW_PATTERNS: &[&str] = &[
    r"^(Throttling error|Service unavailable):",
    r"rate limit",
    r"too many requests",
];

fn overflow_set() -> &'static regex::RegexSet {
    static SET: OnceLock<regex::RegexSet> = OnceLock::new();
    SET.get_or_init(|| {
        let pats: Vec<String> = OVERFLOW_PATTERNS
            .iter()
            .map(|p| format!("(?i){p}"))
            .collect();
        regex::RegexSet::new(&pats).expect("overflow patterns compile")
    })
}

fn non_overflow_set() -> &'static regex::RegexSet {
    static SET: OnceLock<regex::RegexSet> = OnceLock::new();
    SET.get_or_init(|| {
        let pats: Vec<String> = NON_OVERFLOW_PATTERNS
            .iter()
            .map(|p| format!("(?i){p}"))
            .collect();
        regex::RegexSet::new(&pats).expect("non-overflow patterns compile")
    })
}

/// 错误文本是否为上下文溢出（对标 `isContextOverflow` 的报错路径）。
/// 限流/节流等非溢出先排除——Bedrock 的 "ThrottlingException: Too many tokens"
/// 字面含 "too many tokens"，无此排除会误判。
pub fn is_overflow_error(msg: &str) -> bool {
    if non_overflow_set().is_match(msg) {
        return false;
    }
    overflow_set().is_match(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_overflow_across_providers() {
        // 三家原生 + 常见网关/本地形态
        for msg in [
            "anthropic 400: prompt is too long: 213462 tokens > 200000 maximum",
            "413 {\"error\":{\"type\":\"request_too_large\"}}",
            "Your input exceeds the context window of this model",
            "Requested token count exceeds the model's maximum context length of 131072 tokens",
            "Input length (265330) exceeds model's maximum context length (262144).",
            "The input token count (1196265) exceeds the maximum number of tokens allowed (1048575)",
            "maximum prompt length is 131072 but the request contains 537812 tokens",
            "Please reduce the length of the messages or completion",
            "This endpoint's maximum context length is 200000 tokens. However, you requested about 250000 tokens",
            "The input (8 tokens) is longer than the model's context length (4 tokens).",
            "prompt token count of 90000 exceeds the limit of 80000",
            "the request exceeds the available context size, try increasing it",
            "Your request exceeded model token limit: 200000 (requested: 250000)",
            "Prompt contains 90000 tokens ... too large for model with 80000 maximum context length",
            "prompt too long; exceeded max context length by 100 tokens",
            "Range of input length should be [1, 8000]",
            "400 status code (no body)",
            "operational: context_length_exceeded while streaming",
        ] {
            assert!(is_overflow_error(msg), "missed: {msg}");
        }
    }

    #[test]
    fn rejects_non_overflow_errors() {
        for msg in [
            "anthropic 401: invalid x-api-key",
            "openai 429: rate limit exceeded for requests",
            // Bedrock 节流经 formatBedrockError 归一化为 "Throttling error:" 前缀后排除
            //（与上游同语义；原始 ThrottlingException 文本上游同样判溢出，保持一致）
            "Throttling error: Too many tokens, please wait before trying again.",
            "Service unavailable: Temporary failure",
            "too many requests, slow down",
            "connection reset by peer",
            "anthropic 400: max_tokens exceeded", // 输出帽，不是上下文溢出
            "thinking blocks with signatures must be contiguous", // 签名失配，走另一条恢复
        ] {
            assert!(!is_overflow_error(msg), "false positive: {msg}");
        }
    }
}
