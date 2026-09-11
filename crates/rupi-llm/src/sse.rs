//! SSE 增量解析器（自 `rust-pi-agent-2c40` 搬运并加强）：三家 provider 共用，替代各自的
//! 逐行 `find('\n')` 循环。处理：跨 chunk 的 UTF-8 边界（不再逐 chunk `from_utf8_lossy`
//! 产生 U+FFFD）、多行 `data:`（按规范以 `\n` 拼接）、`event:` 名、`:` 注释行、CRLF。

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// `event:` 字段；未给则为空串（OpenAI 兼容流通常只有 data）。
    pub event: String,
    /// 一个事件内全部 `data:` 行以 `\n` 拼接后的文本。
    pub data: String,
}

#[derive(Debug, Default)]
pub struct SseParser {
    buf: String,
    /// 上个 chunk 末尾不完整的 UTF-8 序列（最多 3 字节），并入下个 chunk 再解码。
    pending: Vec<u8>,
}

impl SseParser {
    /// 喂入原始字节：先做增量 UTF-8 解码，再切事件块。
    pub fn push_bytes(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        let text = self.decode(chunk);
        self.push(&text)
    }

    /// 喂入文本，返回本次凑齐的完整事件（以空行分隔）。
    pub fn push(&mut self, chunk: &str) -> Vec<SseEvent> {
        self.buf.push_str(chunk);
        let mut out = vec![];
        loop {
            let (idx, sep) = match (self.buf.find("\n\n"), self.buf.find("\r\n\r\n")) {
                (Some(a), Some(b)) => {
                    if a < b {
                        (a, 2)
                    } else {
                        (b, 4)
                    }
                }
                (Some(a), None) => (a, 2),
                (None, Some(b)) => (b, 4),
                (None, None) => break,
            };
            let block = self.buf[..idx].to_string();
            self.buf.drain(..idx + sep);
            if let Some(ev) = parse_block(&block) {
                out.push(ev);
            }
        }
        out
    }

    /// 流结束：末尾没有空行收尾的残块也解析出来（部分网关最后一个事件不带空行）。
    pub fn finish(&mut self) -> Option<SseEvent> {
        let rest = std::mem::take(&mut self.buf);
        self.pending.clear();
        parse_block(&rest)
    }

    fn decode(&mut self, chunk: &[u8]) -> String {
        let mut bytes = std::mem::take(&mut self.pending);
        bytes.extend_from_slice(chunk);
        let mut out = String::with_capacity(bytes.len());
        let mut rest: &[u8] = &bytes;
        loop {
            match std::str::from_utf8(rest) {
                Ok(s) => {
                    out.push_str(s);
                    break;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    // valid_up_to 之前保证合法
                    out.push_str(std::str::from_utf8(&rest[..valid]).unwrap_or(""));
                    match e.error_len() {
                        // 尾部不完整：留到下个 chunk
                        None => {
                            self.pending = rest[valid..].to_vec();
                            break;
                        }
                        // 真非法字节：替换后继续
                        Some(n) => {
                            out.push('\u{FFFD}');
                            rest = &rest[valid + n..];
                        }
                    }
                }
            }
        }
        out
    }
}

fn parse_block(block: &str) -> Option<SseEvent> {
    let mut event = String::new();
    let mut data: Vec<&str> = vec![];
    for line in block.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => event = value.trim().to_string(),
            "data" => data.push(value),
            _ => {}
        }
    }
    if data.is_empty() {
        return None;
    }
    Some(SseEvent {
        event,
        data: data.join("\n"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_events_and_joins_multiline_data() {
        let mut p = SseParser::default();
        let evs = p.push("event: a\ndata: {\"x\":\ndata: 1}\n\n: comment\ndata: [DONE]\n\n");
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].event, "a");
        assert_eq!(evs[0].data, "{\"x\":\n1}");
        assert_eq!(evs[1].event, "");
        assert_eq!(evs[1].data, "[DONE]");
    }

    #[test]
    fn handles_crlf_and_partial_chunks() {
        let mut p = SseParser::default();
        assert!(p.push("data: he").is_empty());
        assert!(p.push("llo\r\n").is_empty());
        let evs = p.push("\r\ndata: x\r\n\r\n");
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].data, "hello");
        assert_eq!(evs[1].data, "x");
    }

    #[test]
    fn utf8_split_across_chunks_is_not_corrupted() {
        let text = "data: 中文测试\n\n";
        let bytes = text.as_bytes();
        // 在多字节字符中间切开
        let cut = text.find('文').unwrap() + 1;
        let mut p = SseParser::default();
        assert!(p.push_bytes(&bytes[..cut]).is_empty());
        let evs = p.push_bytes(&bytes[cut..]);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].data, "中文测试");
        assert!(!evs[0].data.contains('\u{FFFD}'));
    }

    #[test]
    fn invalid_bytes_become_replacement_char() {
        let mut p = SseParser::default();
        let mut raw = b"data: a".to_vec();
        raw.push(0xff);
        raw.extend_from_slice(b"b\n\n");
        let evs = p.push_bytes(&raw);
        assert_eq!(evs[0].data, "a\u{FFFD}b");
    }

    #[test]
    fn finish_flushes_trailing_block_without_blank_line() {
        let mut p = SseParser::default();
        assert!(p.push("data: tail").is_empty());
        assert_eq!(p.finish().unwrap().data, "tail");
        assert!(p.finish().is_none());
    }
}
