//! Minimal SSE parser for OpenAI / Anthropic streaming responses.

use bytes::Bytes;
use futures::Stream;
use futures::StreamExt;

pub struct SseEvent {
    pub event: String,
    pub data: String,
}

#[allow(dead_code)]
pub async fn collect_sse<S, E>(mut stream: S) -> Result<Vec<SseEvent>, String>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    let mut buf = String::new();
    let mut events = Vec::new();
    while let Some(chunk) = stream.next().await {
        let bytes = chunk.map_err(|e| e.to_string())?;
        buf.push_str(&String::from_utf8_lossy(&bytes));
        drain_sse(&mut buf, &mut events);
    }
    if !buf.trim().is_empty() {
        drain_sse(&mut buf, &mut events);
        if !buf.trim().is_empty() {
            // leftover without trailing blank line
            if let Some(ev) = parse_event(&buf) {
                events.push(ev);
            }
        }
    }
    Ok(events)
}

/// Incremental SSE: feed bytes, emit complete events.
pub struct SseParser {
    buf: String,
}

impl Default for SseParser {
    fn default() -> Self {
        Self { buf: String::new() }
    }
}

impl SseParser {
    pub fn push(&mut self, chunk: &str) -> Vec<SseEvent> {
        self.buf.push_str(chunk);
        let mut events = Vec::new();
        drain_sse(&mut self.buf, &mut events);
        events
    }
}

fn drain_sse(buf: &mut String, out: &mut Vec<SseEvent>) {
    loop {
        let split_at = if let Some(idx) = buf.find("\n\n") {
            idx + 2
        } else if let Some(idx) = buf.find("\r\n\r\n") {
            idx + 4
        } else {
            break;
        };
        let block = buf[..split_at].to_string();
        buf.drain(..split_at);
        if let Some(ev) = parse_event(&block) {
            out.push(ev);
        }
    }
}

fn parse_event(block: &str) -> Option<SseEvent> {
    let mut event = String::from("message");
    let mut data_lines = Vec::new();
    for line in block.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("event:") {
            event = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.trim_start().to_string());
        }
    }
    if data_lines.is_empty() {
        return None;
    }
    Some(SseEvent {
        event,
        data: data_lines.join("\n"),
    })
}
