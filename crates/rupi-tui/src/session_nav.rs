//! TUI `/sessions` 选择器（启动 `-r` 复用）：过滤、移动、回车打开。

use rupi_memory::SessionStore;

#[derive(Debug, Clone)]
pub struct SessionRow {
    pub id: String,
    pub profile: String,
    pub created: String,
    pub count: i64,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct SessionNavigator {
    pub rows: Vec<SessionRow>,
    pub selected: usize,
    pub query: String,
    pub filtering: bool,
}

impl SessionNavigator {
    pub fn from_store(store: &SessionStore, limit: usize) -> anyhow::Result<Self> {
        let rows = store
            .list_sessions(limit)?
            .into_iter()
            .map(|(id, profile, created, count, name)| SessionRow {
                id,
                profile,
                created,
                count,
                name,
            })
            .collect();
        Ok(Self {
            rows,
            selected: 0,
            query: String::new(),
            filtering: false,
        })
    }

    pub fn visible(&self) -> Vec<&SessionRow> {
        if self.query.is_empty() {
            return self.rows.iter().collect();
        }
        let q = self.query.to_ascii_lowercase();
        self.rows
            .iter()
            .filter(|r| {
                r.id.to_ascii_lowercase().contains(&q)
                    || r.profile.to_ascii_lowercase().contains(&q)
                    || r.name.to_ascii_lowercase().contains(&q)
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
        self.visible().get(self.selected).map(|r| r.id.as_str())
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

    #[test]
    fn filter_and_select_latest() {
        let home = std::env::temp_dir().join(format!(
            "rupi-sess-nav-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&home);
        let store = SessionStore::open(&home).unwrap();
        let old = store
            .create_session_ex("old", Some("alpha-old"), None, None)
            .unwrap();
        store.add_message(&old, "user", "one").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let new = store
            .create_session_ex("work", Some("unique-qzx-marker"), None, None)
            .unwrap();
        store.add_message(&new, "user", "two").unwrap();
        let mut nav = SessionNavigator::from_store(&store, 20).unwrap();
        assert!(nav.rows.len() >= 2);
        assert_eq!(nav.selected_id(), Some(new.as_str()), "最新一条应在顶");
        nav.filtering = true;
        for c in "qzx".chars() {
            nav.type_char(c);
        }
        nav.clamp_selected();
        assert_eq!(nav.visible().len(), 1);
        assert_eq!(nav.selected_id(), Some(new.as_str()));
        nav.move_by(-1);
        assert_eq!(nav.selected, 0);
        let _ = std::fs::remove_dir_all(&home);
    }
}
