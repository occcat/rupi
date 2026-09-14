//! Kitty 图形协议：探测、编解码、占位行与帧后叠加（对标 Pi `terminal-image.ts`）。

use std::io::{self, IsTerminal, Write};

const KITTY_PREFIX: &str = "\x1b_G";
const CHUNK: usize = 4096;
const CELL_W: u32 = 9;
const CELL_H: u32 = 18;
const MAX_COLS: u16 = 48;
const MAX_ROWS: u16 = 12;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineImage {
    pub row: usize,
    pub alt: String,
    pub media_type: String,
    pub data: String,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TermCaps {
    pub kitty_window_id: bool,
    pub term: String,
    pub term_program: String,
    pub tmux: bool,
    pub ghostty: bool,
    pub wezterm: bool,
    pub warp: bool,
    /// `RUPI_KITTY=1|0` 强制开/关。
    pub force: Option<bool>,
}

impl TermCaps {
    pub fn from_env() -> Self {
        let env = |k: &str| std::env::var(k).ok().filter(|s| !s.is_empty());
        let term = env("TERM").unwrap_or_default().to_ascii_lowercase();
        let term_program = env("TERM_PROGRAM").unwrap_or_default().to_ascii_lowercase();
        let force = env("RUPI_KITTY").and_then(|v| match v.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" | "kitty" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        });
        Self {
            kitty_window_id: env("KITTY_WINDOW_ID").is_some(),
            tmux: env("TMUX").is_some() || term.starts_with("tmux"),
            ghostty: term_program == "ghostty"
                || term.contains("ghostty")
                || env("GHOSTTY_RESOURCES_DIR").is_some(),
            wezterm: env("WEZTERM_PANE").is_some() || term_program == "wezterm",
            warp: term_program == "warpterminal"
                || env("WARP_SESSION_ID").is_some()
                || env("WARP_TERMINAL_SESSION_UUID").is_some(),
            term,
            term_program,
            force,
        }
    }

    pub fn kitty_images(&self) -> bool {
        if let Some(f) = self.force {
            return f;
        }
        if self.tmux || self.term.starts_with("screen") {
            return false;
        }
        self.kitty_window_id
            || self.term_program == "kitty"
            || self.term.contains("kitty")
            || self.ghostty
            || self.wezterm
            || self.warp
    }
}

pub fn supported() -> bool {
    TermCaps::from_env().kitty_images() && io::stdout().is_terminal()
}

pub fn encode_kitty(base64_data: &str, cols: u16, rows: u16, image_id: u32) -> String {
    let mut params = vec!["a=T".into(), "f=100".into(), "q=2".into(), "C=1".into()];
    if cols > 0 {
        params.push(format!("c={cols}"));
    }
    if rows > 0 {
        params.push(format!("r={rows}"));
    }
    if image_id > 0 {
        params.push(format!("i={image_id}"));
    }
    let head = params.join(",");
    if base64_data.len() <= CHUNK {
        return format!("{KITTY_PREFIX}{head};{base64_data}\x1b\\");
    }
    let mut out = String::new();
    let mut offset = 0;
    let mut first = true;
    while offset < base64_data.len() {
        let end = (offset + CHUNK).min(base64_data.len());
        let chunk = &base64_data[offset..end];
        let last = end == base64_data.len();
        if first {
            out.push_str(&format!("{KITTY_PREFIX}{head},m=1;{chunk}\x1b\\"));
            first = false;
        } else if last {
            out.push_str(&format!("{KITTY_PREFIX}m=0;{chunk}\x1b\\"));
        } else {
            out.push_str(&format!("{KITTY_PREFIX}m=1;{chunk}\x1b\\"));
        }
        offset = end;
    }
    out
}

pub fn delete_all_placements() -> &'static str {
    "\x1b_Ga=d,d=a,q=2\x1b\\"
}

pub fn image_id(data: &str) -> u32 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    data.hash(&mut h);
    (h.finish() as u32).saturating_add(1)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PxSize {
    pub width: u32,
    pub height: u32,
}

pub fn decode_base64(data: &str) -> Option<Vec<u8>> {
    let s: String = data.chars().filter(|c| !c.is_whitespace()).collect();
    if !s.len().is_multiple_of(4) {
        return None;
    }
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            b'=' => Some(0),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut i = 0;
    while i < bytes.len() {
        let a = val(bytes[i])?;
        let b = val(bytes[i + 1])?;
        let c = val(bytes[i + 2])?;
        let d = val(bytes[i + 3])?;
        out.push((a << 2) | (b >> 4));
        if bytes[i + 2] != b'=' {
            out.push((b << 4) | (c >> 2));
        }
        if bytes[i + 3] != b'=' {
            out.push((c << 6) | d);
        }
        i += 4;
    }
    Some(out)
}

pub fn dimensions(media_type: &str, base64_data: &str) -> Option<PxSize> {
    let buf = decode_base64(base64_data)?;
    match media_type {
        "image/png" => png_size(&buf),
        "image/jpeg" | "image/jpg" => jpeg_size(&buf),
        "image/gif" => gif_size(&buf),
        "image/webp" => webp_size(&buf),
        _ => None,
    }
}

fn png_size(buf: &[u8]) -> Option<PxSize> {
    if buf.len() < 24 || buf[0] != 0x89 || &buf[1..4] != b"PNG" {
        return None;
    }
    Some(PxSize {
        width: u32::from_be_bytes(buf[16..20].try_into().ok()?),
        height: u32::from_be_bytes(buf[20..24].try_into().ok()?),
    })
}

fn jpeg_size(buf: &[u8]) -> Option<PxSize> {
    if buf.len() < 4 || buf[0] != 0xff || buf[1] != 0xd8 {
        return None;
    }
    let mut offset = 2usize;
    while offset + 9 < buf.len() {
        if buf[offset] != 0xff {
            offset += 1;
            continue;
        }
        let marker = buf[offset + 1];
        if (0xc0..=0xc2).contains(&marker) {
            return Some(PxSize {
                height: u16::from_be_bytes(buf[offset + 5..offset + 7].try_into().ok()?) as u32,
                width: u16::from_be_bytes(buf[offset + 7..offset + 9].try_into().ok()?) as u32,
            });
        }
        if offset + 3 >= buf.len() {
            return None;
        }
        let len = u16::from_be_bytes(buf[offset + 2..offset + 4].try_into().ok()?) as usize;
        if len < 2 {
            return None;
        }
        offset += 2 + len;
    }
    None
}

fn gif_size(buf: &[u8]) -> Option<PxSize> {
    if buf.len() < 10 {
        return None;
    }
    let sig = &buf[..6];
    if sig != b"GIF87a" && sig != b"GIF89a" {
        return None;
    }
    Some(PxSize {
        width: u16::from_le_bytes(buf[6..8].try_into().ok()?) as u32,
        height: u16::from_le_bytes(buf[8..10].try_into().ok()?) as u32,
    })
}

fn webp_size(buf: &[u8]) -> Option<PxSize> {
    if buf.len() < 30 || &buf[..4] != b"RIFF" || &buf[8..12] != b"WEBP" {
        return None;
    }
    match &buf[12..16] {
        b"VP8 " => Some(PxSize {
            width: (u16::from_le_bytes(buf[26..28].try_into().ok()?) & 0x3fff) as u32,
            height: (u16::from_le_bytes(buf[28..30].try_into().ok()?) & 0x3fff) as u32,
        }),
        b"VP8L" => {
            let bits = u32::from_le_bytes(buf[21..25].try_into().ok()?);
            Some(PxSize {
                width: (bits & 0x3fff) + 1,
                height: ((bits >> 14) & 0x3fff) + 1,
            })
        }
        b"VP8X" => Some(PxSize {
            width: (u32::from_le_bytes([buf[24], buf[25], buf[26], 0]) + 1),
            height: (u32::from_le_bytes([buf[27], buf[28], buf[29], 0]) + 1),
        }),
        _ => None,
    }
}

pub fn cell_size(px: Option<PxSize>) -> (u16, u16) {
    let PxSize { width, height } = px.unwrap_or(PxSize {
        width: 180,
        height: 108,
    });
    let width = width.max(1);
    let height = height.max(1);
    let mut cols = width.div_ceil(CELL_W).max(1) as u16;
    let mut rows = height.div_ceil(CELL_H).max(1) as u16;
    if cols > MAX_COLS {
        let scale = MAX_COLS as f64 / cols as f64;
        cols = MAX_COLS;
        rows = ((rows as f64 * scale).ceil() as u16).max(1);
    }
    if rows > MAX_ROWS {
        let scale = MAX_ROWS as f64 / rows as f64;
        rows = MAX_ROWS;
        cols = ((cols as f64 * scale).ceil() as u16).max(1);
    }
    (cols.max(1), rows.max(1))
}

pub fn caption(alt: &str, media_type: &str, px: Option<PxSize>) -> String {
    let kind = media_type.strip_prefix("image/").unwrap_or(media_type);
    let dim = px
        .map(|p| format!(" {}×{}", p.width, p.height))
        .unwrap_or_default();
    if alt.is_empty() {
        format!("[image {kind}{dim}]")
    } else {
        format!("[image {alt} {kind}{dim}]")
    }
}

pub fn spec(alt: &str, media_type: &str, data: &str) -> InlineImage {
    let px = dimensions(media_type, data);
    let (cols, rows) = if data.is_empty() {
        (0, 1)
    } else {
        cell_size(px)
    };
    InlineImage {
        row: 0,
        alt: alt.to_string(),
        media_type: media_type.to_string(),
        data: data.to_string(),
        cols,
        rows,
    }
}

/// 在消息区内叠加可见图。`start` 与聊天 `lines[start..]` 对齐。
pub fn paint_visible<W: Write>(
    mut out: W,
    images: &[InlineImage],
    start: usize,
    inner: (u16, u16, u16, u16),
) -> io::Result<()> {
    let (x, y, _w, h) = inner;
    out.write_all(delete_all_placements().as_bytes())?;
    for img in images {
        if img.data.is_empty() || img.row < start {
            continue;
        }
        let rel = img.row - start;
        if rel >= h as usize {
            continue;
        }
        let gy = y + rel as u16;
        let seq = encode_kitty(&img.data, img.cols, img.rows, image_id(&img.data));
        write!(out, "\x1b[{};{}H{seq}", gy + 1, x + 1)?;
    }
    out.flush()
}

pub fn parse_data_url(src: &str) -> Option<(String, String)> {
    let rest = src.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    if !meta.contains(";base64") {
        return None;
    }
    let media = meta.split(';').next()?.to_string();
    if !media.starts_with("image/") {
        return None;
    }
    Some((media, data.trim().to_string()))
}

pub fn load_local_image(src: &str) -> Option<(String, String)> {
    if src.starts_with("http://") || src.starts_with("https://") || src.starts_with("data:") {
        return None;
    }
    let path = std::path::Path::new(src.trim().trim_matches(['<', '>']));
    if !path.is_file() {
        return None;
    }
    let meta = path.metadata().ok()?;
    if meta.len() > 5 * 1024 * 1024 {
        return None;
    }
    let media = rupi_core::image_media_type(path)?.to_string();
    let bytes = std::fs::read(path).ok()?;
    Some((media, rupi_core::encode_base64(&bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    // 1×1 PNG
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

    #[test]
    fn encode_contains_apc_and_chunks() {
        let seq = encode_kitty("abc", 10, 4, 7);
        assert!(seq.starts_with("\x1b_G"), "{seq:?}");
        assert!(seq.contains("a=T"));
        assert!(seq.contains("f=100"));
        assert!(seq.contains("c=10"));
        assert!(seq.contains("r=4"));
        assert!(seq.contains("i=7"));
        assert!(seq.ends_with("\x1b\\"));
        let big = "x".repeat(5000);
        let chunked = encode_kitty(&big, 1, 1, 1);
        assert!(chunked.contains("m=1"));
        assert!(chunked.contains("m=0"));
    }

    #[test]
    fn png_dimensions_1x1() {
        let px = dimensions("image/png", PNG).expect("png");
        assert_eq!(
            px,
            PxSize {
                width: 1,
                height: 1
            }
        );
        let (c, r) = cell_size(Some(px));
        assert!(c >= 1 && r >= 1);
    }

    #[test]
    fn detect_kitty_and_tmux() {
        let mut caps = TermCaps {
            kitty_window_id: true,
            ..TermCaps::default()
        };
        assert!(caps.kitty_images());
        caps.tmux = true;
        assert!(!caps.kitty_images());
        caps.force = Some(true);
        assert!(caps.kitty_images());
        caps.force = Some(false);
        caps.tmux = false;
        assert!(!caps.kitty_images());
        let ghost = TermCaps {
            ghostty: true,
            ..TermCaps::default()
        };
        assert!(ghost.kitty_images());
    }

    #[test]
    fn paint_writes_move_and_payload() {
        let img = spec("dot", "image/png", PNG);
        let mut buf = Vec::new();
        paint_visible(&mut buf, &[img], 0, (2, 3, 40, 10)).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains(delete_all_placements()));
        assert!(s.contains("\x1b_G"));
        assert!(s.contains("\x1b[4;3H"), "{s:?}");
    }

    #[test]
    fn data_url_and_caption() {
        let (m, d) = parse_data_url(&format!("data:image/png;base64,{PNG}")).unwrap();
        assert_eq!(m, "image/png");
        assert_eq!(d, PNG);
        assert!(caption(
            "dot",
            "image/png",
            Some(PxSize {
                width: 1,
                height: 1
            })
        )
        .contains("1×1"));
    }

    #[test]
    fn decode_roundtrip_small() {
        let raw = b"hi!";
        let enc = rupi_core::encode_base64(raw);
        assert_eq!(decode_base64(&enc).as_deref(), Some(raw.as_slice()));
    }
}
