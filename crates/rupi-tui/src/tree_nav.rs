//! `/tree` 交互导航器：过滤、移动、回车跳转。

use rupi_core::{SessionTree, TreeEntry};

#[derive(Debug, Clone)]
pub struct TreeNavigator {
    pub entries: Vec<TreeEntry>,
    pub selected: usize,
    pub query: String,
    pub filtering: bool,
}

impl TreeNavigator {
    pub fn from_session(session: &SessionTree) -> Self {
        let entries = session.tree_entries();
        let selected = entries.iter().rposition(|e| e.on_path).unwrap_or(0);
        Self {
            entries,
            selected,
            query: String::new(),
            filtering: false,
        }
    }

    pub fn visible(&self) -> Vec<&TreeEntry> {
        if self.query.is_empty() {
            return self.entries.iter().collect();
        }
        let q = self.query.to_ascii_lowercase();
        self.entries
            .iter()
            .filter(|e| {
                e.preview.to_ascii_lowercase().contains(&q)
                    || e.id.to_ascii_lowercase().contains(&q)
            })
            .collect()
    }

    pub fn clamp_selected(&mut self) {
        let n = self.visible().len();
        if n == 0 {
            self.selected = 0;
        } else if self.selected >= n {
            self.selected = n - 1;
        }
    }

    pub fn move_by(&mut self, delta: i32) {
        let n = self.visible().len() as i32;
        if n == 0 {
            return;
        }
        let next = (self.selected as i32 + delta).clamp(0, n - 1);
        self.selected = next as usize;
    }

    pub fn selected_id(&self) -> Option<&str> {
        self.visible().get(self.selected).map(|e| e.id.as_str())
    }

    pub fn type_char(&mut self, c: char) {
        if self.filtering {
            self.query.push(c);
            self.selected = 0;
        }
    }

    pub fn backspace(&mut self) {
        if self.filtering {
            self.query.pop();
            self.selected = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rupi_core::{Message, Role};

    #[test]
    fn filter_and_select() {
        let mut s = SessionTree::new();
        s.push(Message::text(Role::User, "alpha hello"));
        s.push(Message::text(Role::Assistant, "beta world"));
        let mut nav = TreeNavigator::from_session(&s);
        assert_eq!(nav.visible().len(), 2);
        nav.filtering = true;
        nav.type_char('b');
        nav.type_char('e');
        nav.clamp_selected();
        assert_eq!(nav.visible().len(), 1);
        assert!(nav.selected_id().is_some());
        nav.move_by(-1);
        assert_eq!(nav.selected, 0);
    }
}
