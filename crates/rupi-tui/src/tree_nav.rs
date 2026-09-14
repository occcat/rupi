//! `/tree` 交互导航器：过滤、移动、回车跳转、节点标签与折叠持久化。

use rupi_core::{SessionTree, TreeEntry};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TreeBookmarks {
    #[serde(default)]
    pub labels: HashMap<String, String>,
    #[serde(default)]
    pub collapsed: HashSet<String>,
}

impl TreeBookmarks {
    pub fn store_path(home: &Path, session_id: &str) -> PathBuf {
        home.join("tree").join(format!("{session_id}.json"))
    }

    pub fn load(home: &Path, session_id: &str) -> Self {
        if session_id.is_empty() {
            return Self::default();
        }
        let path = Self::store_path(home, session_id);
        let Ok(raw) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        serde_json::from_str(raw.trim_start_matches('\u{FEFF}')).unwrap_or_default()
    }

    pub fn save(&self, home: &Path, session_id: &str) -> anyhow::Result<()> {
        if session_id.is_empty() {
            return Ok(());
        }
        let path = Self::store_path(home, session_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = serde_json::to_string_pretty(self)?;
        std::fs::write(path, format!("{body}\n"))?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct TreeNavigator {
    pub entries: Vec<TreeEntry>,
    pub selected: usize,
    pub query: String,
    pub filtering: bool,
    pub labels: HashMap<String, String>,
    pub collapsed: HashSet<String>,
    /// 正在编辑的标签草稿。
    pub labeling: Option<String>,
}

impl TreeNavigator {
    pub fn from_session(session: &SessionTree) -> Self {
        Self::from_session_bookmarks(session, TreeBookmarks::default())
    }

    pub fn from_session_persisted(session: &SessionTree, home: &Path, session_id: &str) -> Self {
        Self::from_session_bookmarks(session, TreeBookmarks::load(home, session_id))
    }

    pub fn from_session_bookmarks(session: &SessionTree, marks: TreeBookmarks) -> Self {
        let entries = session.tree_entries();
        let selected = entries.iter().rposition(|e| e.on_path).unwrap_or(0);
        Self {
            entries,
            selected,
            query: String::new(),
            filtering: false,
            labels: marks.labels,
            collapsed: marks.collapsed,
            labeling: None,
        }
    }

    pub fn bookmarks(&self) -> TreeBookmarks {
        TreeBookmarks {
            labels: self.labels.clone(),
            collapsed: self.collapsed.clone(),
        }
    }

    pub fn persist(&self, home: &Path, session_id: &str) {
        if let Err(e) = self.bookmarks().save(home, session_id) {
            tracing::warn!("tree bookmarks persist failed: {e:#}");
        }
    }

    pub fn visible(&self) -> Vec<&TreeEntry> {
        if !self.query.is_empty() {
            let q = self.query.to_ascii_lowercase();
            return self
                .entries
                .iter()
                .filter(|e| {
                    e.preview.to_ascii_lowercase().contains(&q)
                        || e.id.to_ascii_lowercase().contains(&q)
                        || self
                            .labels
                            .get(&e.id)
                            .is_some_and(|l| l.to_ascii_lowercase().contains(&q))
                })
                .collect();
        }
        let mut hidden_until_depth = None;
        let mut out = Vec::new();
        for e in &self.entries {
            if let Some(d) = hidden_until_depth {
                if e.depth > d {
                    continue;
                }
                hidden_until_depth = None;
            }
            out.push(e);
            if self.collapsed.contains(&e.id) {
                hidden_until_depth = Some(e.depth);
            }
        }
        out
    }

    pub fn has_children(&self, id: &str) -> bool {
        let Some(idx) = self.entries.iter().position(|e| e.id == id) else {
            return false;
        };
        self.entries
            .get(idx + 1)
            .is_some_and(|n| n.depth > self.entries[idx].depth)
    }

    pub fn is_collapsed(&self, id: &str) -> bool {
        self.collapsed.contains(id)
    }

    pub fn toggle_collapse(&mut self) -> bool {
        let Some(id) = self.selected_id().map(str::to_string) else {
            return false;
        };
        if !self.has_children(&id) {
            return false;
        }
        if !self.collapsed.remove(&id) {
            self.collapsed.insert(id);
        }
        self.clamp_selected();
        true
    }

    pub fn begin_label(&mut self) {
        let Some(id) = self.selected_id() else {
            return;
        };
        self.labeling = Some(self.labels.get(id).cloned().unwrap_or_default());
    }

    pub fn label_char(&mut self, c: char) {
        if let Some(s) = self.labeling.as_mut() {
            s.push(c);
        }
    }

    pub fn label_backspace(&mut self) {
        if let Some(s) = self.labeling.as_mut() {
            s.pop();
        }
    }

    pub fn commit_label(&mut self) {
        let Some(draft) = self.labeling.take() else {
            return;
        };
        let Some(id) = self.selected_id().map(str::to_string) else {
            return;
        };
        let t = draft.trim();
        if t.is_empty() {
            self.labels.remove(&id);
        } else {
            self.labels.insert(id, t.to_string());
        }
    }

    pub fn cancel_label(&mut self) {
        self.labeling = None;
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

    fn branched() -> SessionTree {
        let mut s = SessionTree::new();
        s.push(Message::text(Role::User, "alpha-root"));
        s.push(Message::text(Role::Assistant, "unique-qzx-marker"));
        s
    }

    #[test]
    fn filter_and_select() {
        let mut s = SessionTree::new();
        // 预览用 g–z（非 hex）：过滤串不能撞上节点 UUID（0-9a-f），
        // 也不因大小写折叠把两条预览收成同一命中。
        s.push(Message::text(Role::User, "alpha-hello"));
        s.push(Message::text(Role::Assistant, "unique-qzx-marker"));
        let mut nav = TreeNavigator::from_session(&s);
        assert_eq!(nav.visible().len(), 2);
        nav.filtering = true;
        for c in "qzx".chars() {
            nav.type_char(c);
        }
        nav.clamp_selected();
        assert_eq!(nav.visible().len(), 1);
        assert!(nav.selected_id().is_some());
        nav.move_by(-1);
        assert_eq!(nav.selected, 0);
    }

    #[test]
    fn collapse_hides_children_and_label_persists() {
        let s = branched();
        let mut nav = TreeNavigator::from_session(&s);
        assert_eq!(nav.visible().len(), 2);
        nav.selected = 0;
        assert!(nav.has_children(&nav.entries[0].id));
        assert!(nav.toggle_collapse());
        assert_eq!(nav.visible().len(), 1);
        nav.begin_label();
        for c in "bookmark".chars() {
            nav.label_char(c);
        }
        nav.commit_label();
        assert_eq!(
            nav.labels.get(&nav.entries[0].id).map(String::as_str),
            Some("bookmark")
        );

        let dir =
            std::env::temp_dir().join(format!("rupi-tree-bm-{}-{}", std::process::id(), line!()));
        let _ = std::fs::remove_dir_all(&dir);
        nav.persist(&dir, "sess-tree");
        let loaded = TreeBookmarks::load(&dir, "sess-tree");
        assert_eq!(
            loaded.labels.values().next().map(String::as_str),
            Some("bookmark")
        );
        assert_eq!(loaded.collapsed.len(), 1);
        let nav2 = TreeNavigator::from_session_bookmarks(&s, loaded);
        assert_eq!(nav2.visible().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
