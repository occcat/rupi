//! `~/.rupi/keybindings.json` 最小集 + 扩展 `registerKeybinding` 覆盖。

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyChord {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub code: ChordCode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChordCode {
    Enter,
    Tab,
    Char(char),
}

impl KeyChord {
    pub fn parse(raw: &str) -> Option<Self> {
        let mut ctrl = false;
        let mut alt = false;
        let mut shift = false;
        let mut code = None;
        for part in raw.split('+').map(str::trim).filter(|s| !s.is_empty()) {
            match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => ctrl = true,
                "alt" | "option" => alt = true,
                "shift" => shift = true,
                "enter" | "return" => code = Some(ChordCode::Enter),
                "tab" => code = Some(ChordCode::Tab),
                other if other.chars().count() == 1 => {
                    code = Some(ChordCode::Char(other.chars().next()?));
                }
                _ => return None,
            }
        }
        Some(Self {
            ctrl,
            alt,
            shift,
            code: code?,
        })
    }

    pub fn matches(&self, key: &KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        if self.ctrl != ctrl || self.alt != alt {
            return false;
        }
        match (&self.code, key.code) {
            (ChordCode::Enter, KeyCode::Enter) => self.shift == shift,
            (ChordCode::Tab, KeyCode::Tab) => self.shift == shift,
            (ChordCode::Tab, KeyCode::BackTab) => self.shift,
            (ChordCode::Char(want), KeyCode::Char(got)) => {
                self.shift == shift && want.eq_ignore_ascii_case(&got)
            }
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct KeyTable {
    pub send: KeyChord,
    pub newline: KeyChord,
    pub paste: KeyChord,
    pub model: KeyChord,
    pub thinking: KeyChord,
    pub fold: KeyChord,
    pub enabled_models: KeyChord,
}

impl Default for KeyTable {
    fn default() -> Self {
        Self {
            send: KeyChord::parse("enter").unwrap(),
            newline: KeyChord::parse("shift+enter").unwrap(),
            paste: KeyChord::parse("ctrl+v").unwrap(),
            model: KeyChord::parse("ctrl+l").unwrap(),
            thinking: KeyChord::parse("shift+tab").unwrap(),
            fold: KeyChord::parse("ctrl+o").unwrap(),
            enabled_models: KeyChord::parse("ctrl+p").unwrap(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FileKeys {
    send: Option<String>,
    newline: Option<String>,
    paste: Option<String>,
    model: Option<String>,
    thinking: Option<String>,
    fold: Option<String>,
    #[serde(alias = "enabledModels")]
    enabled_models: Option<String>,
}

impl KeyTable {
    pub fn load(home: &Path) -> Self {
        let mut table = Self::default();
        let path = home.join("keybindings.json");
        if let Ok(raw) = std::fs::read_to_string(&path) {
            if let Ok(file) = serde_json::from_str::<FileKeys>(&raw) {
                apply_opt(&mut table.send, file.send.as_deref());
                apply_opt(&mut table.newline, file.newline.as_deref());
                apply_opt(&mut table.paste, file.paste.as_deref());
                apply_opt(&mut table.model, file.model.as_deref());
                apply_opt(&mut table.thinking, file.thinking.as_deref());
                apply_opt(&mut table.fold, file.fold.as_deref());
                apply_opt(&mut table.enabled_models, file.enabled_models.as_deref());
            }
        }
        for kb in rupi_ext::registered_keybindings() {
            let Some(chord) = KeyChord::parse(&kb.key) else {
                continue;
            };
            match kb.action.as_str() {
                "send" => table.send = chord,
                "newline" => table.newline = chord,
                "paste" => table.paste = chord,
                "model" => table.model = chord,
                "thinking" => table.thinking = chord,
                "fold" => table.fold = chord,
                "enabledModels" | "enabled_models" => table.enabled_models = chord,
                _ => {}
            }
        }
        table
    }
}

fn apply_opt(slot: &mut KeyChord, raw: Option<&str>) {
    if let Some(c) = raw.and_then(KeyChord::parse) {
        *slot = c;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_match_defaults() {
        let t = KeyTable::default();
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let shift_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
        let ctrl_v = KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL);
        let backtab = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);
        assert!(t.send.matches(&enter));
        assert!(!t.send.matches(&shift_enter));
        assert!(t.newline.matches(&shift_enter));
        assert!(t.paste.matches(&ctrl_v));
        assert!(t.thinking.matches(&backtab));
    }

    #[test]
    fn load_file_overrides_send() {
        let dir = std::env::temp_dir().join(format!("rupi-keys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&dir.join("keybindings.json"), r#"{"send":"ctrl+s"}"#).unwrap();
        let t = KeyTable::load(&dir);
        let ctrl_s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert!(t.send.matches(&ctrl_s));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
