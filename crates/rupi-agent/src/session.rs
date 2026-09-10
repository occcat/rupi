use chrono::{DateTime, Utc};
use rupi_ai::Message;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::io::{AsyncBufReadExt, BufReader};
use uuid::Uuid;

pub const CURRENT_SESSION_VERSION: u32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionHeader {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub version: Option<u32>,
    pub id: String,
    pub timestamp: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEntry {
    Message {
        id: String,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: String,
        message: Message,
    },
    ModelChange {
        id: String,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: String,
        provider: String,
        #[serde(rename = "modelId")]
        model_id: String,
    },
    Compaction {
        id: String,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: String,
        summary: String,
        #[serde(rename = "tokensBefore")]
        tokens_before: u32,
    },
}

impl SessionEntry {
    pub fn id(&self) -> &str {
        match self {
            Self::Message { id, .. }
            | Self::ModelChange { id, .. }
            | Self::Compaction { id, .. } => id,
        }
    }

    pub fn as_message(&self) -> Option<&Message> {
        match self {
            Self::Message { message, .. } => Some(message),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Session {
    pub header: SessionHeader,
    pub entries: Vec<SessionEntry>,
    pub path: PathBuf,
}

impl Session {
    pub fn messages(&self) -> Vec<Message> {
        self.entries
            .iter()
            .filter_map(|e| e.as_message().cloned())
            .collect()
    }

    pub fn last_entry_id(&self) -> Option<String> {
        self.entries.last().map(|e| e.id().to_string())
    }
}

pub struct SessionManager {
    dir: PathBuf,
}

impl SessionManager {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub async fn create(&self, cwd: &Path, name: Option<String>) -> std::io::Result<Session> {
        fs::create_dir_all(&self.dir).await?;
        let id = Uuid::now_v7().to_string();
        let timestamp = Utc::now().to_rfc3339();
        let header = SessionHeader {
            kind: "session".into(),
            version: Some(CURRENT_SESSION_VERSION),
            id: id.clone(),
            timestamp: timestamp.clone(),
            cwd: cwd.display().to_string(),
            parent_session: None,
            name,
        };
        let path = self.dir.join(format!("{id}.jsonl"));
        let line = serde_json::to_string(&header).unwrap();
        fs::write(&path, format!("{line}\n")).await?;
        Ok(Session {
            header,
            entries: Vec::new(),
            path,
        })
    }

    pub async fn load(&self, path: &Path) -> std::io::Result<Session> {
        let file = fs::File::open(path).await?;
        let reader = BufReader::new(file);
        let mut lines = reader.lines();
        let header_line = lines
            .next_line()
            .await?
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "empty session"))?;
        let header: SessionHeader = serde_json::from_str(&header_line)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let mut entries = Vec::new();
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(entry) = serde_json::from_str::<SessionEntry>(&line) {
                entries.push(entry);
            }
        }
        Ok(Session {
            header,
            entries,
            path: path.to_path_buf(),
        })
    }

    pub async fn latest(&self) -> std::io::Result<Option<Session>> {
        fs::create_dir_all(&self.dir).await?;
        let mut latest: Option<(DateTime<Utc>, PathBuf)> = None;
        let mut rd = fs::read_dir(&self.dir).await?;
        while let Some(entry) = rd.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            let meta = entry.metadata().await?;
            let modified = meta.modified().ok().map(DateTime::<Utc>::from);
            if let Some(m) = modified {
                if latest.as_ref().map(|(t, _)| m > *t).unwrap_or(true) {
                    latest = Some((m, path));
                }
            }
        }
        match latest {
            Some((_, path)) => Ok(Some(self.load(&path).await?)),
            None => Ok(None),
        }
    }

    pub async fn list(&self) -> std::io::Result<Vec<PathBuf>> {
        fs::create_dir_all(&self.dir).await?;
        let mut out = Vec::new();
        let mut rd = fs::read_dir(&self.dir).await?;
        while let Some(entry) = rd.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                out.push(path);
            }
        }
        out.sort();
        out.reverse();
        Ok(out)
    }

    pub async fn append(&self, session: &mut Session, message: Message) -> std::io::Result<()> {
        let parent = session.last_entry_id();
        let entry = SessionEntry::Message {
            id: Uuid::now_v7().to_string(),
            parent_id: parent,
            timestamp: Utc::now().to_rfc3339(),
            message,
        };
        let line = serde_json::to_string(&entry).unwrap();
        let mut data = fs::read(&session.path).await.unwrap_or_default();
        data.extend_from_slice(line.as_bytes());
        data.push(b'\n');
        fs::write(&session.path, data).await?;
        session.entries.push(entry);
        Ok(())
    }

    pub async fn append_compaction(
        &self,
        session: &mut Session,
        summary: String,
        tokens_before: u32,
    ) -> std::io::Result<()> {
        let parent = session.last_entry_id();
        let entry = SessionEntry::Compaction {
            id: Uuid::now_v7().to_string(),
            parent_id: parent,
            timestamp: Utc::now().to_rfc3339(),
            summary,
            tokens_before,
        };
        let line = serde_json::to_string(&entry).unwrap();
        let mut data = fs::read(&session.path).await.unwrap_or_default();
        data.extend_from_slice(line.as_bytes());
        data.push(b'\n');
        fs::write(&session.path, data).await?;
        session.entries.push(entry);
        Ok(())
    }
}
