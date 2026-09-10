use rusqlite::{params, Connection};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct SessionSearchHit {
    pub session_id: String,
    pub role: String,
    pub text: String,
    pub timestamp: i64,
    pub rank: f64,
}

/// Evidence layer: unbounded FTS5 index of past conversation turns.
pub struct SessionSearchIndex {
    conn: Connection,
}

impl SessionSearchIndex {
    pub fn open(path: impl AsRef<Path>) -> rusqlite::Result<Self> {
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                text TEXT NOT NULL,
                timestamp INTEGER NOT NULL
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
                text,
                content='messages',
                content_rowid='id'
            );
            CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
                INSERT INTO messages_fts(rowid, text) VALUES (new.id, new.text);
            END;
            "#,
        )?;
        Ok(Self { conn })
    }

    pub fn in_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            r#"
            CREATE TABLE messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                text TEXT NOT NULL,
                timestamp INTEGER NOT NULL
            );
            CREATE VIRTUAL TABLE messages_fts USING fts5(
                text,
                content='messages',
                content_rowid='id'
            );
            CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
                INSERT INTO messages_fts(rowid, text) VALUES (new.id, new.text);
            END;
            "#,
        )?;
        Ok(Self { conn })
    }

    pub fn insert(
        &self,
        session_id: &str,
        role: &str,
        text: &str,
        timestamp: i64,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO messages (session_id, role, text, timestamp) VALUES (?1, ?2, ?3, ?4)",
            params![session_id, role, text, timestamp],
        )?;
        Ok(())
    }

    pub fn search(&self, query: &str, limit: usize) -> rusqlite::Result<Vec<SessionSearchHit>> {
        let q = query.trim();
        if q.is_empty() {
            return Ok(vec![]);
        }
        // Quote as an FTS5 phrase to avoid syntax errors from user punctuation.
        let escaped = q.replace('"', " ");
        let match_query = format!("\"{escaped}\"");
        let mut stmt = self.conn.prepare(
            "SELECT m.session_id, m.role, m.text, m.timestamp, bm25(messages_fts)
             FROM messages_fts
             JOIN messages m ON m.id = messages_fts.rowid
             WHERE messages_fts MATCH ?1
             ORDER BY bm25(messages_fts)
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![match_query, limit as i64], |row| {
            Ok(SessionSearchHit {
                session_id: row.get(0)?,
                role: row.get(1)?,
                text: row.get(2)?,
                timestamp: row.get(3)?,
                rank: row.get(4)?,
            })
        })?;
        rows.collect()
    }

    pub fn scroll(
        &self,
        session_id: &str,
        after_ts: i64,
        limit: usize,
    ) -> rusqlite::Result<Vec<SessionSearchHit>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id, role, text, timestamp, 0.0
             FROM messages
             WHERE session_id = ?1 AND timestamp > ?2
             ORDER BY timestamp ASC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![session_id, after_ts, limit as i64], |row| {
            Ok(SessionSearchHit {
                session_id: row.get(0)?,
                role: row.get(1)?,
                text: row.get(2)?,
                timestamp: row.get(3)?,
                rank: row.get(4)?,
            })
        })?;
        rows.collect()
    }
}
