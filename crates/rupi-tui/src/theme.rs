//! TUI 主题（`RUPI_THEME=dark|light`）。

use ratatui::style::Color;

#[derive(Debug, Clone)]
pub struct Theme {
    pub name: &'static str,
    pub id: u8,
    pub user: Color,
    pub assistant: Color,
    pub tool: Color,
    pub system: Color,
    pub thinking: Color,
    pub prompt: Color,
    pub border: Color,
    pub footer: Color,
    pub heading: Color,
    pub code: Color,
    pub accent: Color,
    pub syntax_comment: Color,
    pub syntax_keyword: Color,
    pub syntax_function: Color,
    pub syntax_variable: Color,
    pub syntax_string: Color,
    pub syntax_number: Color,
    pub syntax_type: Color,
    pub syntax_operator: Color,
    pub syntax_punctuation: Color,
}

impl Theme {
    pub fn dark() -> Self {
        Self {
            name: "dark",
            id: 0,
            user: Color::Cyan,
            assistant: Color::White,
            tool: Color::Yellow,
            system: Color::DarkGray,
            thinking: Color::Magenta,
            prompt: Color::Green,
            border: Color::Gray,
            footer: Color::DarkGray,
            heading: Color::LightBlue,
            code: Color::LightGreen,
            accent: Color::LightYellow,
            syntax_comment: Color::DarkGray,
            syntax_keyword: Color::LightBlue,
            syntax_function: Color::Cyan,
            syntax_variable: Color::White,
            syntax_string: Color::LightGreen,
            syntax_number: Color::LightYellow,
            syntax_type: Color::LightMagenta,
            syntax_operator: Color::Gray,
            syntax_punctuation: Color::DarkGray,
        }
    }

    pub fn light() -> Self {
        Self {
            name: "light",
            id: 1,
            user: Color::Blue,
            assistant: Color::Black,
            tool: Color::Rgb(160, 100, 0),
            system: Color::Gray,
            thinking: Color::Magenta,
            prompt: Color::Green,
            border: Color::DarkGray,
            footer: Color::Gray,
            heading: Color::Blue,
            code: Color::Rgb(0, 100, 40),
            accent: Color::Rgb(140, 80, 0),
            syntax_comment: Color::Gray,
            syntax_keyword: Color::Blue,
            syntax_function: Color::Rgb(0, 90, 140),
            syntax_variable: Color::Black,
            syntax_string: Color::Rgb(0, 100, 40),
            syntax_number: Color::Rgb(140, 80, 0),
            syntax_type: Color::Magenta,
            syntax_operator: Color::DarkGray,
            syntax_punctuation: Color::Gray,
        }
    }

    pub fn from_name(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "light" => Self::light(),
            _ => Self::dark(),
        }
    }

    pub fn from_env() -> Self {
        match std::env::var("RUPI_THEME") {
            Ok(v) if !v.is_empty() => Self::from_name(&v),
            _ => Self::dark(),
        }
    }

    /// 启动用：`RUPI_THEME` 覆盖 `settings.theme`。
    pub fn resolve(settings_theme: &str) -> Self {
        match std::env::var("RUPI_THEME") {
            Ok(v) if !v.trim().is_empty() => Self::from_name(&v),
            _ => Self::from_name(settings_theme),
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::from_env()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_differ() {
        assert_ne!(Theme::dark().id, Theme::light().id);
    }

    #[test]
    fn from_name_light_and_unknown() {
        assert_eq!(Theme::from_name("light").id, Theme::light().id);
        assert_eq!(Theme::from_name("LIGHT").name, "light");
        assert_eq!(Theme::from_name("nope").name, "dark");
        assert_eq!(Theme::from_name("").name, "dark");
    }
}
