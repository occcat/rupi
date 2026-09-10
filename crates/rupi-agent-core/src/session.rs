use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use rupi_ai::Message;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Durable session tree. Pi persists leaf changes as `leaf` entries;
/// reopening reconstructs the current leaf from the latest leaf-affecting entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEntry {
    Session {
        id: String,
        cwd: String,
        model: String,
        #[serde(default)]
        name: Option<String>,
        timestamp: i64,
    },
    Message {
        id: String,
        parent_id: Option<String>,
        timestamp: i64,
        message: Message,
    },
    Leaf {
        id: String,
        target_id: Option<String>,
        timestamp: i64,
    },
    Compaction {
        id: String,
        parent_id: Option<String>,
        timestamp: i64,
        summary: String,
        tokens_before: u32,
        tokens_after: u32,
    },
}

impl SessionEntry {
    pub fn id(&self) -> &str {
        match self {
            Self::Session { id, .. }
            | Self::Message { id, .. }
            | Self::Leaf { id, .. }
            | Self::Compaction { id, .. } => id,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SessionStore {
    pub path: PathBuf,
    pub entries: Vec<SessionEntry>,
    pub leaf_id: Option<String>,
    pub session_id: String,
}

impl SessionStore {
    pub fn create(dir: impl AsRef<Path>, cwd: impl Into<String>, model: impl Into<String>) -> std::io::Result<Self> {
        fs::create_dir_all(dir.as_ref())?;
        let session_id = Uuid::now_v7().to_string();
        let path = dir.as_ref().join(format!("{session_id}.jsonl"));
        let mut store = Self {
            path,
            entries: Vec::new(),
            leaf_id: None,
            session_id: session_id.clone(),
        };
        store.append(SessionEntry::Session {
            id: session_id,
            cwd: cwd.into(),
            model: model.into(),
            name: None,
            timestamp: rupi_ai::now_ms(),
        })?;
        Ok(store)
    }

    pub fn in_memory(cwd: impl Into<String>, model: impl Into<String>) -> Self {
        let session_id = Uuid::now_v7().to_string();
        Self {
            path: PathBuf::from(":memory:"),
            entries: vec![SessionEntry::Session {
                id: session_id.clone(),
                cwd: cwd.into(),
                model: model.into(),
                name: None,
                timestamp: rupi_ai::now_ms(),
            }],
            leaf_id: None,
            session_id,
        }
    }

    pub fn open(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = fs::File::open(&path)?;
        let reader = BufReader::new(file);
        let mut entries = Vec::new();
        let mut leaf_id = None;
        let mut session_id = String::new();
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let entry: SessionEntry = serde_json::from_str(&line)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            match &entry {
                SessionEntry::Session { id, .. } => session_id = id.clone(),
                SessionEntry::Leaf { target_id, .. } => leaf_id = target_id.clone(),
                SessionEntry::Message { id, .. } | SessionEntry::Compaction { id, .. } => {
                    leaf_id = Some(id.clone());
                }
            }
            entries.push(entry);
        }
        Ok(Self {
            path,
            entries,
            leaf_id,
            session_id,
        })
    }

    pub fn append(&mut self, entry: SessionEntry) -> std::io::Result<()> {
        if self.path != PathBuf::from(":memory:") {
            if let Some(parent) = self.path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?;
            serde_json::to_writer(&mut file, &entry)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            file.write_all(b"\n")?;
        }
        match &entry {
            SessionEntry::Leaf { target_id, .. } => self.leaf_id = target_id.clone(),
            SessionEntry::Message { id, .. } | SessionEntry::Compaction { id, .. } => {
                self.leaf_id = Some(id.clone());
            }
            SessionEntry::Session { .. } => {}
        }
        self.entries.push(entry);
        Ok(())
    }

    pub fn append_message(&mut self, message: Message) -> std::io::Result<String> {
        let id = Uuid::now_v7().to_string();
        self.append(SessionEntry::Message {
            id: id.clone(),
            parent_id: self.leaf_id.clone(),
            timestamp: rupi_ai::now_ms(),
            message,
        })?;
        Ok(id)
    }

    pub fn set_leaf(&mut self, target_id: Option<String>) -> std::io::Result<()> {
        self.append(SessionEntry::Leaf {
            id: Uuid::now_v7().to_string(),
            target_id,
            timestamp: rupi_ai::now_ms(),
        })
    }

    /// Active branch: walk parents from current leaf back to root.
    pub fn active_messages(&self) -> Vec<Message> {
        let mut by_id = std::collections::HashMap::new();
        for e in &self.entries {
            if let SessionEntry::Message { id, parent_id, message, .. } = e {
                by_id.insert(id.clone(), (parent_id.clone(), message.clone()));
            }
        }
        let mut chain = Vec::new();
        let mut cursor = self.leaf_id.clone();
        let mut guard = 0;
        while let Some(id) = cursor {
            guard += 1;
            if guard > 100_000 {
                break;
            }
            if let Some((parent, msg)) = by_id.get(&id) {
                chain.push(msg.clone());
                cursor = parent.clone();
            } else {
                break;
            }
        }
        chain.reverse();
        chain
    }

    pub fn list_sessions(dir: impl AsRef<Path>) -> std::io::Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        if !dir.as_ref().exists() {
            return Ok(out);
        }
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if entry.path().extension().and_then(|s| s.to_str()) == Some("jsonl") {
                out.push(entry.path());
            }
        }
        out.sort();
        Ok(out)
    }
}
