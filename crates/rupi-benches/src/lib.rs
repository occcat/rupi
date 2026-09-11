//! 仓内 criterion 基准的共享夹具（SSE 流、大文件原文）。

/// 拼一段接近真实模型流的 SSE：`n` 个 `data:` 事件，按 `chunk` 字节切开。
pub fn sse_stream(n: usize, payload: &str) -> Vec<u8> {
    let mut raw = String::with_capacity(n * (payload.len() + 16));
    for i in 0..n {
        raw.push_str("data: ");
        raw.push_str(payload);
        raw.push_str(&format!(" {i}\n\n"));
    }
    raw.into_bytes()
}

/// 按 `chunk` 切成网络块（末块可短）。
pub fn chunked(bytes: &[u8], chunk: usize) -> Vec<&[u8]> {
    bytes.chunks(chunk.max(1)).collect()
}

/// 一段带唯一锚点的“源文件”，供 edit 基准。
pub fn source_file(lines: usize) -> String {
    let mut s = String::with_capacity(lines * 40);
    for i in 0..lines {
        s.push_str(&format!("fn item_{i}() {{ let x = {i}; }}\n"));
    }
    s.push_str("const UNIQUE_ANCHOR: u32 = 42;\n");
    s
}
