use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::Mutex;

pub struct SessionIndex {
    conn: Mutex<Connection>,
}

impl SessionIndex {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                title TEXT,
                cwd TEXT,
                started_at TEXT
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS session_fts USING fts5(
                session_id,
                role,
                content,
                timestamp,
                tokenize = 'porter'
            );
            "#,
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn upsert_session(&self, id: &str, title: &str, cwd: &str, started_at: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO sessions(id, title, cwd, started_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET title=excluded.title",
            params![id, title, cwd, started_at],
        )?;
        Ok(())
    }

    pub fn index_message(
        &self,
        session_id: &str,
        role: &str,
        content: &str,
        timestamp: &str,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO session_fts(session_id, role, content, timestamp) VALUES (?1, ?2, ?3, ?4)",
            params![session_id, role, content, timestamp],
        )?;
        Ok(())
    }

    pub fn search(&self, query: &str, limit: usize) -> rusqlite::Result<Vec<SearchHit>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT session_id, role, content, timestamp FROM session_fts
             WHERE session_fts MATCH ?1
             ORDER BY rank
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![query, limit as i64], |row| {
            Ok(SearchHit {
                session_id: row.get(0)?,
                role: row.get(1)?,
                content: row.get(2)?,
                timestamp: row.get(3)?,
            })
        })?;
        rows.collect()
    }

    pub fn browse(
        &self,
        session_id: &str,
        offset: usize,
        limit: usize,
    ) -> rusqlite::Result<Vec<SearchHit>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT session_id, role, content, timestamp FROM session_fts
             WHERE session_id = ?1
             LIMIT ?2 OFFSET ?3",
        )?;
        let rows = stmt.query_map(params![session_id, limit as i64, offset as i64], |row| {
            Ok(SearchHit {
                session_id: row.get(0)?,
                role: row.get(1)?,
                content: row.get(2)?,
                timestamp: row.get(3)?,
            })
        })?;
        rows.collect()
    }
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub session_id: String,
    pub role: String,
    pub content: String,
    pub timestamp: String,
}
