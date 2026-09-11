//! 增量 UTF-8 解码：跨 chunk 的不完整序列留给下一段，而不是像逐段
//! `from_utf8_lossy` 那样立刻变成 U+FFFD。真非法字节仍替换为 U+FFFD。
//! 对标 `rupi-llm::sse` 的跨 chunk 解码，供 bash / 扩展进程输出共用。

/// 跨 chunk 的增量 UTF-8 解码器（pending 最多 3 字节）。
#[derive(Debug, Default)]
pub struct Utf8Decoder {
    pending: Vec<u8>,
}

impl Utf8Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一段字节，返回本次能确定的文本（尾部不完整序列留在 pending）。
    pub fn push(&mut self, chunk: &[u8]) -> String {
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
                    out.push_str(std::str::from_utf8(&rest[..valid]).unwrap_or(""));
                    match e.error_len() {
                        None => {
                            self.pending = rest[valid..].to_vec();
                            break;
                        }
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

    /// 流结束：残留的不完整序列按 lossy 语义写成 U+FFFD。
    pub fn finish(&mut self) -> String {
        if self.pending.is_empty() {
            return String::new();
        }
        self.pending.clear();
        "\u{FFFD}".to_string()
    }

    /// 整段缓冲一次性解码（完整输出路径：bash/ext 收齐 stdout 后）。
    pub fn decode_all(bytes: &[u8]) -> String {
        let mut d = Self::new();
        let mut s = d.push(bytes);
        s.push_str(&d.finish());
        s
    }
}

/// [`Utf8Decoder::decode_all`] 的短名。
pub fn decode_utf8(bytes: &[u8]) -> String {
    Utf8Decoder::decode_all(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incremental_joins_split_multibyte() {
        // 你 = E4 BD A0；按字节切开时 lossy 会出 U+FFFD，增量应拼回汉字。
        let mut d = Utf8Decoder::new();
        assert_eq!(d.push(&[0xe4]), "");
        assert_eq!(d.push(&[0xbd, 0xa0, 0xe5]), "你");
        assert_eq!(d.push(&[0xa5, 0xbd]), "好");
        assert_eq!(d.finish(), "");
    }

    #[test]
    fn decode_all_matches_lossy_on_complete_buffers() {
        let ok = "你好 world".as_bytes();
        assert_eq!(decode_utf8(ok), String::from_utf8_lossy(ok));
        let bad = [0x61, 0xff, 0x62];
        assert_eq!(decode_utf8(&bad), String::from_utf8_lossy(&bad));
        let cut = [0xe4, 0xbd];
        assert_eq!(decode_utf8(&cut), String::from_utf8_lossy(&cut));
    }
}
