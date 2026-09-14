//! 进程内工作区编辑器（Rust/TUI）。不移植 Pi 的进程内 TypeScript UI。
//! 打开启动 cwd 沙箱内文件、编辑、Ctrl+S 写回；`/edit <path>` 或快捷键进入。

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::path::{Component, Path, PathBuf};

/// 单文件上限：避免把大二进制/日志整屏拉进 TUI。
pub const MAX_BYTES: u64 = 1_048_576;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorAction {
    Continue,
    Close,
}

#[derive(Debug, Clone)]
pub struct FileEditor {
    pub root: PathBuf,
    pub rel: String,
    pub abs: PathBuf,
    lines: Vec<String>,
    cursor_row: usize,
    cursor_col: usize,
    scroll: usize,
    dirty: bool,
    created: bool,
    crlf: bool,
    status: String,
    confirm_quit: bool,
    /// `Some`：尚未打开文件，输入框在问路径。
    prompt: Option<String>,
}

impl FileEditor {
    pub fn prompt(root: impl Into<PathBuf>, hint: &str) -> Self {
        Self {
            root: root.into(),
            rel: String::new(),
            abs: PathBuf::new(),
            lines: vec![String::new()],
            cursor_row: 0,
            cursor_col: 0,
            scroll: 0,
            dirty: false,
            created: false,
            crlf: false,
            status: "[edit] type a workspace path, Enter open, Esc cancel".into(),
            confirm_quit: false,
            prompt: Some(hint.to_string()),
        }
    }

    /// 有路径则打开；空路径进入问询。
    pub fn from_hint(root: &Path, hint: &str) -> Result<Self, String> {
        let hint = hint.trim();
        if hint.is_empty() {
            Ok(Self::prompt(root, ""))
        } else {
            Self::open(root, hint)
        }
    }

    pub fn open(root: &Path, path: &str) -> Result<Self, String> {
        let abs = resolve_workspace_path(root, path)?;
        let root = root
            .canonicalize()
            .map_err(|e| format!("workspace root: {e}"))?;
        let rel = abs
            .strip_prefix(&root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| abs.to_string_lossy().into_owned());
        if abs.is_dir() {
            return Err(format!("path is a directory: {rel}"));
        }
        let created = !abs.exists();
        let (lines, crlf, status) = if created {
            (
                vec![String::new()],
                false,
                format!("new {rel} — Ctrl+S save, Esc close"),
            )
        } else {
            let meta = std::fs::metadata(&abs).map_err(|e| format!("stat {rel}: {e}"))?;
            if meta.len() > MAX_BYTES {
                return Err(format!("{rel} is {} bytes (limit {MAX_BYTES})", meta.len()));
            }
            let raw = std::fs::read(&abs).map_err(|e| format!("read {rel}: {e}"))?;
            if raw.contains(&0) {
                return Err(format!("{rel} looks binary"));
            }
            let text = String::from_utf8(raw).map_err(|_| format!("{rel} is not UTF-8"))?;
            let crlf = text.contains("\r\n");
            (split_lines(&text), crlf, format!("{rel} — Ctrl+S save, Esc close"))
        };
        Ok(Self {
            root,
            rel,
            abs,
            lines,
            cursor_row: 0,
            cursor_col: 0,
            scroll: 0,
            dirty: false,
            created,
            crlf,
            status,
            confirm_quit: false,
            prompt: None,
        })
    }

    pub fn is_prompt(&self) -> bool {
        self.prompt.is_some()
    }

    pub fn prompt_draft(&self) -> &str {
        self.prompt.as_deref().unwrap_or("")
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn is_created(&self) -> bool {
        self.created
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    pub fn cursor(&self) -> (usize, usize) {
        (self.cursor_row, self.cursor_col)
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn text(&self) -> String {
        join_lines(&self.lines, self.crlf)
    }

    pub fn title(&self) -> String {
        if self.is_prompt() {
            return "rupi /edit  path?".into();
        }
        let mark = if self.dirty { " +" } else { "" };
        let new = if self.created { " (new)" } else { "" };
        format!("rupi /edit  {}{new}{mark}  Ctrl+S 保存  Esc 关闭", self.rel)
    }

    pub fn save(&mut self) -> Result<(), String> {
        if self.is_prompt() {
            return Err("no file open".into());
        }
        let dest = resolve_workspace_path(&self.root, &self.rel)?;
        if dest != self.abs && !dest.starts_with(&self.root) {
            return Err("path escapes workspace".into());
        }
        if let Some(parent) = self.abs.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
            }
        }
        let body = self.text();
        if body.len() as u64 > MAX_BYTES {
            return Err(format!("buffer is {} bytes (limit {MAX_BYTES})", body.len()));
        }
        std::fs::write(&self.abs, body).map_err(|e| format!("write {}: {e}", self.rel))?;
        self.dirty = false;
        self.created = false;
        self.confirm_quit = false;
        self.status = format!("saved {}", self.rel);
        Ok(())
    }

    pub fn insert_str(&mut self, s: &str) {
        if let Some(draft) = self.prompt.as_mut() {
            for c in s.chars() {
                if c == '\n' || c == '\r' {
                    continue;
                }
                draft.push(c);
            }
            return;
        }
        for c in s.chars() {
            if c == '\r' {
                continue;
            }
            if c == '\n' {
                self.newline();
            } else {
                self.insert_char(c);
            }
        }
    }

    pub fn insert_char(&mut self, c: char) {
        if c == '\n' {
            self.newline();
            return;
        }
        self.confirm_quit = false;
        let line = &mut self.lines[self.cursor_row];
        let mut chars: Vec<char> = line.chars().collect();
        let col = self.cursor_col.min(chars.len());
        chars.insert(col, c);
        *line = chars.into_iter().collect();
        self.cursor_col = col + 1;
        self.dirty = true;
    }

    pub fn newline(&mut self) {
        self.confirm_quit = false;
        let line = &self.lines[self.cursor_row];
        let chars: Vec<char> = line.chars().collect();
        let col = self.cursor_col.min(chars.len());
        let left: String = chars[..col].iter().collect();
        let right: String = chars[col..].iter().collect();
        self.lines[self.cursor_row] = left;
        self.lines.insert(self.cursor_row + 1, right);
        self.cursor_row += 1;
        self.cursor_col = 0;
        self.dirty = true;
    }

    pub fn backspace(&mut self) {
        self.confirm_quit = false;
        if let Some(draft) = self.prompt.as_mut() {
            draft.pop();
            return;
        }
        if self.cursor_col > 0 {
            let line = &mut self.lines[self.cursor_row];
            let mut chars: Vec<char> = line.chars().collect();
            let col = self.cursor_col.min(chars.len());
            chars.remove(col - 1);
            *line = chars.into_iter().collect();
            self.cursor_col = col - 1;
            self.dirty = true;
            return;
        }
        if self.cursor_row == 0 {
            return;
        }
        let rest = self.lines.remove(self.cursor_row);
        self.cursor_row -= 1;
        self.cursor_col = self.lines[self.cursor_row].chars().count();
        self.lines[self.cursor_row].push_str(&rest);
        self.dirty = true;
    }

    pub fn delete(&mut self) {
        if self.is_prompt() {
            return;
        }
        self.confirm_quit = false;
        let len = self.lines[self.cursor_row].chars().count();
        if self.cursor_col < len {
            let line = &mut self.lines[self.cursor_row];
            let mut chars: Vec<char> = line.chars().collect();
            chars.remove(self.cursor_col);
            *line = chars.into_iter().collect();
            self.dirty = true;
            return;
        }
        if self.cursor_row + 1 < self.lines.len() {
            let rest = self.lines.remove(self.cursor_row + 1);
            self.lines[self.cursor_row].push_str(&rest);
            self.dirty = true;
        }
    }

    pub fn move_left(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        } else if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.cursor_col = self.lines[self.cursor_row].chars().count();
        }
    }

    pub fn move_right(&mut self) {
        let len = self.lines[self.cursor_row].chars().count();
        if self.cursor_col < len {
            self.cursor_col += 1;
        } else if self.cursor_row + 1 < self.lines.len() {
            self.cursor_row += 1;
            self.cursor_col = 0;
        }
    }

    pub fn move_up(&mut self) {
        if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.clamp_col();
        }
    }

    pub fn move_down(&mut self) {
        if self.cursor_row + 1 < self.lines.len() {
            self.cursor_row += 1;
            self.clamp_col();
        }
    }

    pub fn move_home(&mut self) {
        self.cursor_col = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor_col = self.lines[self.cursor_row].chars().count();
    }

    pub fn page_by(&mut self, delta: i32, page: usize) {
        let page = page.max(1) as i32;
        let next = (self.cursor_row as i32 + delta * page).clamp(0, self.lines.len() as i32 - 1);
        self.cursor_row = next as usize;
        self.clamp_col();
    }

    pub fn scroll_by(&mut self, delta: i32, height: usize) {
        let max = self.lines.len().saturating_sub(height.max(1));
        let next = (self.scroll as i32 + delta).clamp(0, max as i32);
        self.scroll = next as usize;
    }

    fn clamp_col(&mut self) {
        let len = self.lines[self.cursor_row].chars().count();
        if self.cursor_col > len {
            self.cursor_col = len;
        }
    }

    pub fn ensure_visible(&mut self, height: usize) {
        let height = height.max(1);
        if self.cursor_row < self.scroll {
            self.scroll = self.cursor_row;
        } else if self.cursor_row >= self.scroll + height {
            self.scroll = self.cursor_row + 1 - height;
        }
    }

    /// `(is_cursor_row, gutter+text)`，行号从 1 起。
    pub fn display_rows(&mut self, height: usize) -> Vec<(bool, String)> {
        self.ensure_visible(height);
        let width = ((self.lines.len() as f64).log10().floor() as usize + 1).max(3);
        self.lines
            .iter()
            .enumerate()
            .skip(self.scroll)
            .take(height)
            .map(|(i, line)| {
                (
                    i == self.cursor_row,
                    format!("{:>width$}│{line}", i + 1, width = width),
                )
            })
            .collect()
    }

    /// 正文区内相对坐标（不含边框）；`gutter` 与 [`display_rows`] 对齐。
    pub fn cursor_in_inner(&mut self, height: usize) -> (u16, u16) {
        self.ensure_visible(height);
        let width = ((self.lines.len() as f64).log10().floor() as usize + 1).max(3);
        let x = (width + 1 + self.cursor_col) as u16;
        let y = (self.cursor_row.saturating_sub(self.scroll)) as u16;
        (x, y)
    }

    pub fn handle_key(&mut self, key: &KeyEvent) -> EditorAction {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('s') => {
                    if self.is_prompt() {
                        return EditorAction::Continue;
                    }
                    match self.save() {
                        Ok(()) => {}
                        Err(e) => self.status = format!("[edit] {e}"),
                    }
                    return EditorAction::Continue;
                }
                KeyCode::Char('q') => {
                    return self.request_close();
                }
                _ => return EditorAction::Continue,
            }
        }
        if self.is_prompt() {
            return self.handle_prompt_key(key);
        }
        match key.code {
            KeyCode::Esc => self.request_close(),
            KeyCode::Enter => {
                self.newline();
                EditorAction::Continue
            }
            KeyCode::Backspace => {
                self.backspace();
                EditorAction::Continue
            }
            KeyCode::Delete => {
                self.delete();
                EditorAction::Continue
            }
            KeyCode::Left => {
                self.move_left();
                EditorAction::Continue
            }
            KeyCode::Right => {
                self.move_right();
                EditorAction::Continue
            }
            KeyCode::Up => {
                self.move_up();
                EditorAction::Continue
            }
            KeyCode::Down => {
                self.move_down();
                EditorAction::Continue
            }
            KeyCode::Home => {
                self.move_home();
                EditorAction::Continue
            }
            KeyCode::End => {
                self.move_end();
                EditorAction::Continue
            }
            KeyCode::PageUp => {
                self.page_by(-1, 10);
                EditorAction::Continue
            }
            KeyCode::PageDown => {
                self.page_by(1, 10);
                EditorAction::Continue
            }
            KeyCode::Tab => {
                self.insert_str("    ");
                EditorAction::Continue
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::ALT) => {
                self.insert_char(c);
                EditorAction::Continue
            }
            _ => EditorAction::Continue,
        }
    }

    fn handle_prompt_key(&mut self, key: &KeyEvent) -> EditorAction {
        match key.code {
            KeyCode::Esc => EditorAction::Close,
            KeyCode::Enter => {
                let draft = self.prompt.clone().unwrap_or_default();
                match Self::open(&self.root, &draft) {
                    Ok(opened) => {
                        *self = opened;
                        EditorAction::Continue
                    }
                    Err(e) => {
                        self.status = format!("[edit] {e}");
                        EditorAction::Continue
                    }
                }
            }
            KeyCode::Backspace => {
                self.backspace();
                EditorAction::Continue
            }
            KeyCode::Char(c) if !c.is_control() => {
                if let Some(draft) = self.prompt.as_mut() {
                    draft.push(c);
                }
                EditorAction::Continue
            }
            _ => EditorAction::Continue,
        }
    }

    fn request_close(&mut self) -> EditorAction {
        if self.is_prompt() {
            return EditorAction::Close;
        }
        if self.dirty && !self.confirm_quit {
            self.confirm_quit = true;
            self.status = "unsaved — Esc again to discard, Ctrl+S to save".into();
            return EditorAction::Continue;
        }
        EditorAction::Close
    }
}

/// 从输入框猜要打开的相对路径：`/edit p`、`@p`、或整段相对路径。
pub fn path_hint_from_input(text: &str) -> Option<String> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    if let Some((name, args)) = rupi_core::commands::split(t) {
        if name == "edit" && !args.is_empty() {
            return Some(args.to_string());
        }
        return None;
    }
    if let Some(at) = t.find('@') {
        let before_ok = at == 0 || t[..at].chars().last().is_some_and(|c| c.is_whitespace());
        if before_ok {
            let token = t[at + 1..].split_whitespace().next().unwrap_or("");
            if !token.is_empty() && !token.starts_with('/') {
                return Some(token.to_string());
            }
        }
    }
    if !t.contains(char::is_whitespace) && !t.starts_with('/') && !t.contains('\0') {
        return Some(t.to_string());
    }
    None
}

pub fn resolve_workspace_path(root: &Path, path: &str) -> Result<PathBuf, String> {
    let path = path.trim();
    if path.is_empty() {
        return Err("usage: /edit <path>".into());
    }
    if path.contains('\0') {
        return Err("invalid path".into());
    }
    let root = root
        .canonicalize()
        .map_err(|e| format!("workspace root: {e}"))?;
    let raw = Path::new(path);
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        root.join(raw)
    };
    let dest = resolve_existing_or_new(&joined)?;
    if !dest.starts_with(&root) {
        return Err(format!("path escapes workspace: {path}"));
    }
    Ok(dest)
}

fn resolve_existing_or_new(path: &Path) -> Result<PathBuf, String> {
    let joined = normalize_path(path);
    let comps: Vec<Component<'_>> = joined.components().collect();
    if comps.is_empty() {
        return Err("invalid path".into());
    }
    let mut existing = PathBuf::new();
    let mut consumed = 0usize;
    for (idx, c) in comps.iter().enumerate() {
        let mut trial = existing.clone();
        trial.push(c);
        if trial.exists() {
            existing = trial;
            consumed = idx + 1;
        } else {
            break;
        }
    }
    if existing.as_os_str().is_empty() {
        return Err("invalid path".into());
    }
    let mut dest = existing
        .canonicalize()
        .map_err(|e| format!("resolve: {e}"))?;
    if consumed < comps.len() && dest.is_file() {
        return Err("path walks through a file".into());
    }
    for c in comps.iter().skip(consumed) {
        match c {
            Component::Normal(name) => dest.push(name),
            Component::CurDir => {}
            _ => return Err("path escapes workspace".into()),
        }
    }
    Ok(dest)
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Prefix(_) | Component::RootDir => out.push(c),
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = out.pop();
            }
            Component::Normal(_) => out.push(c),
        }
    }
    out
}

fn split_lines(text: &str) -> Vec<String> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    if text.is_empty() {
        return vec![String::new()];
    }
    text.split('\n').map(|s| s.to_string()).collect()
}

fn join_lines(lines: &[String], crlf: bool) -> String {
    let n = if crlf { "\r\n" } else { "\n" };
    lines.join(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn workspace(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "rupi-edit-{tag}-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn open_edit_save_roundtrip() {
        let root = workspace("round");
        let file = root.join("note.md");
        std::fs::write(&file, "hello\n世界\n").unwrap();
        let mut ed = FileEditor::open(&root, "note.md").unwrap();
        assert_eq!(ed.rel, "note.md");
        assert!(!ed.is_dirty());
        assert_eq!(ed.text(), "hello\n世界\n");
        ed.move_end();
        ed.insert_str("!");
        assert!(ed.is_dirty());
        ed.save().unwrap();
        assert!(!ed.is_dirty());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello!\n世界\n");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn create_new_file_on_save() {
        let root = workspace("new");
        let mut ed = FileEditor::open(&root, "src/a.rs").unwrap();
        assert!(ed.is_created());
        ed.insert_str("fn main() {}\n");
        ed.save().unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("src/a.rs")).unwrap(),
            "fn main() {}\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_escape_and_binary() {
        let root = workspace("jail");
        std::fs::write(root.join("ok.txt"), "x").unwrap();
        assert!(resolve_workspace_path(&root, "../secret").is_err());
        assert!(resolve_workspace_path(&root, "/etc/passwd").is_err());
        assert!(FileEditor::open(&root, "..").is_err());
        std::fs::write(root.join("bin.dat"), [b'a', 0, b'b']).unwrap();
        assert!(FileEditor::open(&root, "bin.dat").unwrap_err().contains("binary"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn keys_edit_and_confirm_quit() {
        let root = workspace("keys");
        std::fs::write(root.join("t.txt"), "ab").unwrap();
        let mut ed = FileEditor::open(&root, "t.txt").unwrap();
        assert_eq!(ed.handle_key(&key(KeyCode::End)), EditorAction::Continue);
        assert_eq!(
            ed.handle_key(&key(KeyCode::Char('c'))),
            EditorAction::Continue
        );
        assert_eq!(ed.text(), "abc");
        assert_eq!(ed.handle_key(&key(KeyCode::Esc)), EditorAction::Continue);
        assert!(ed.status().contains("unsaved"));
        assert_eq!(ed.handle_key(&key(KeyCode::Esc)), EditorAction::Close);
        assert_eq!(std::fs::read_to_string(root.join("t.txt")).unwrap(), "ab");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ctrl_s_writes_and_esc_closes_clean() {
        let root = workspace("ctrls");
        std::fs::write(root.join("t.txt"), "x").unwrap();
        let mut ed = FileEditor::open(&root, "t.txt").unwrap();
        ed.insert_char('y');
        let save = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert_eq!(ed.handle_key(&save), EditorAction::Continue);
        assert!(!ed.is_dirty());
        assert_eq!(std::fs::read_to_string(root.join("t.txt")).unwrap(), "yx");
        assert_eq!(ed.handle_key(&key(KeyCode::Esc)), EditorAction::Close);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn prompt_enter_opens_and_esc_cancels() {
        let root = workspace("prompt");
        std::fs::write(root.join("p.txt"), "hi").unwrap();
        let mut ed = FileEditor::prompt(&root, "");
        ed.insert_str("p.txt");
        assert_eq!(ed.handle_key(&key(KeyCode::Enter)), EditorAction::Continue);
        assert!(!ed.is_prompt());
        assert_eq!(ed.text(), "hi");
        let mut ed = FileEditor::prompt(&root, "nope");
        assert_eq!(ed.handle_key(&key(KeyCode::Esc)), EditorAction::Close);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn path_hint_from_slash_at_and_bare() {
        assert_eq!(
            path_hint_from_input("/edit src/main.rs"),
            Some("src/main.rs".into())
        );
        assert_eq!(path_hint_from_input("/edit"), None);
        assert_eq!(
            path_hint_from_input("看 @foo/bar.rs 后面"),
            Some("foo/bar.rs".into())
        );
        assert_eq!(path_hint_from_input("@foo/bar.rs"), Some("foo/bar.rs".into()));
        assert_eq!(path_hint_from_input("联系 foo@bar.com"), None);
        assert_eq!(path_hint_from_input("lib.rs"), Some("lib.rs".into()));
        assert_eq!(path_hint_from_input("/tree"), None);
        assert_eq!(path_hint_from_input(""), None);
    }

    #[test]
    fn newline_and_backspace_join() {
        let root = workspace("join");
        let mut ed = FileEditor::open(&root, "n.txt").unwrap();
        ed.insert_str("ab");
        ed.move_left();
        ed.newline();
        assert_eq!(ed.text(), "a\nb");
        ed.backspace();
        assert_eq!(ed.text(), "ab");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn preserves_crlf_on_save() {
        let root = workspace("crlf");
        std::fs::write(root.join("w.txt"), "a\r\nb\r\n").unwrap();
        let mut ed = FileEditor::open(&root, "w.txt").unwrap();
        ed.move_end();
        ed.insert_char('!');
        ed.save().unwrap();
        assert_eq!(std::fs::read_to_string(root.join("w.txt")).unwrap(), "a!\r\nb\r\n");
        let _ = std::fs::remove_dir_all(&root);
    }
}
