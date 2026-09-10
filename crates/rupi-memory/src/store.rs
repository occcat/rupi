use std::fs;
use std::path::{Path, PathBuf};

use crate::security::scan_memory_entry;

pub const MEMORY_CHAR_LIMIT: usize = 2200;
pub const USER_CHAR_LIMIT: usize = 1375;
pub const PROJECT_CHAR_LIMIT: usize = 2200;
pub const ENTRY_DELIM: &str = "§";
pub const CORE_PREFIX: &str = "[core]";

/// Hermes-style stores: agent notes, user profile, and per-project facts.
/// Session history lives in the FTS5 evidence layer (`session_search`), not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreKind {
    Memory,
    User,
    Project,
}

impl StoreKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::User => "user",
            Self::Project => "project",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "memory" => Some(Self::Memory),
            "user" => Some(Self::User),
            "project" => Some(Self::Project),
            _ => None,
        }
    }

    pub fn limit(self) -> usize {
        match self {
            Self::Memory => MEMORY_CHAR_LIMIT,
            Self::User => USER_CHAR_LIMIT,
            Self::Project => PROJECT_CHAR_LIMIT,
        }
    }

    pub fn filename(self) -> &'static str {
        match self {
            Self::Memory => "MEMORY.md",
            Self::User => "USER.md",
            Self::Project => "PROJECT.md",
        }
    }
}

#[derive(Debug, Clone)]
pub struct MemoryLimits {
    pub memory: usize,
    pub user: usize,
    pub project: usize,
}

impl Default for MemoryLimits {
    fn default() -> Self {
        Self {
            memory: MEMORY_CHAR_LIMIT,
            user: USER_CHAR_LIMIT,
            project: PROJECT_CHAR_LIMIT,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MemoryEntry {
    pub raw: String,
}

impl MemoryEntry {
    pub fn is_core(&self) -> bool {
        self.raw
            .trim_start()
            .to_ascii_lowercase()
            .starts_with("[core]")
    }

    pub fn display_body(&self) -> String {
        let t = self.raw.trim_start();
        if t.len() >= 6 && t[..6].eq_ignore_ascii_case("[core]") {
            t[6..].trim_start().to_string()
        } else {
            self.raw.clone()
        }
    }

    pub fn chars(&self) -> usize {
        self.raw.chars().count()
    }
}

#[derive(Debug, Clone)]
pub struct MemoryStore {
    pub dir: PathBuf,
    pub project_dir: PathBuf,
    pub limits: MemoryLimits,
    memory: Vec<MemoryEntry>,
    user: Vec<MemoryEntry>,
    project: Vec<MemoryEntry>,
}

impl MemoryStore {
    pub fn open(dir: impl AsRef<Path>) -> std::io::Result<Self> {
        Self::open_layered(dir.as_ref(), dir.as_ref())
    }

    /// `home` holds MEMORY.md + USER.md; `project` holds PROJECT.md.
    pub fn open_layered(home: impl AsRef<Path>, project: impl AsRef<Path>) -> std::io::Result<Self> {
        let dir = home.as_ref().to_path_buf();
        let project_dir = project.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
        fs::create_dir_all(&project_dir)?;
        let mut store = Self {
            dir: dir.clone(),
            project_dir: project_dir.clone(),
            limits: MemoryLimits::default(),
            memory: Vec::new(),
            user: Vec::new(),
            project: Vec::new(),
        };
        store.memory = load_file(&dir.join("MEMORY.md"));
        store.user = load_file(&dir.join("USER.md"));
        store.project = load_file(&project_dir.join("PROJECT.md"));
        Ok(store)
    }

    pub fn entries(&self, kind: StoreKind) -> &[MemoryEntry] {
        match kind {
            StoreKind::Memory => &self.memory,
            StoreKind::User => &self.user,
            StoreKind::Project => &self.project,
        }
    }

    pub fn entries_mut(&mut self, kind: StoreKind) -> &mut Vec<MemoryEntry> {
        match kind {
            StoreKind::Memory => &mut self.memory,
            StoreKind::User => &mut self.user,
            StoreKind::Project => &mut self.project,
        }
    }

    pub fn usage_chars(&self, kind: StoreKind) -> usize {
        joined_len(self.entries(kind))
    }

    pub fn add(&mut self, kind: StoreKind, content: &str) -> Result<String, String> {
        scan_memory_entry(content)?;
        let content = content.trim();
        if content.is_empty() {
            return Err("empty memory entry".into());
        }
        if self
            .entries(kind)
            .iter()
            .any(|e| e.raw.trim() == content)
        {
            return Ok("no duplicate added".into());
        }
        let next_len = joined_len_with(self.entries(kind), content);
        if next_len > kind.limit() {
            return Err(format!(
                "Memory at {}/{} chars. Adding this entry ({} chars) would exceed the limit. Consolidate now: use 'replace' to merge overlapping entries into shorter ones or 'remove' stale entries, then retry this add — all in this turn.",
                self.usage_chars(kind),
                kind.limit(),
                content.chars().count()
            ));
        }
        self.entries_mut(kind).push(MemoryEntry {
            raw: content.to_string(),
        });
        self.persist(kind).map_err(|e| e.to_string())?;
        Ok("added".into())
    }

    pub fn replace(&mut self, kind: StoreKind, old_text: &str, content: &str) -> Result<String, String> {
        scan_memory_entry(content)?;
        let idx = self.find_unique(kind, old_text)?;
        let mut next = self.entries(kind).to_vec();
        next[idx] = MemoryEntry {
            raw: content.trim().to_string(),
        };
        if joined_len(&next) > kind.limit() {
            return Err(format!(
                "replace would exceed {}/{} chars; shorten the new content or remove another entry first",
                joined_len(&next),
                kind.limit()
            ));
        }
        *self.entries_mut(kind) = next;
        self.persist(kind).map_err(|e| e.to_string())?;
        Ok("replaced".into())
    }

    pub fn remove(&mut self, kind: StoreKind, old_text: &str) -> Result<String, String> {
        let idx = self.find_unique(kind, old_text)?;
        self.entries_mut(kind).remove(idx);
        self.persist(kind).map_err(|e| e.to_string())?;
        Ok("removed".into())
    }

    pub fn search(&self, query: &str) -> Vec<(StoreKind, MemoryEntry)> {
        let q = query.to_ascii_lowercase();
        let mut hits = Vec::new();
        for kind in [StoreKind::Memory, StoreKind::User, StoreKind::Project] {
            for e in self.entries(kind) {
                if e.raw.to_ascii_lowercase().contains(&q) {
                    hits.push((kind, e.clone()));
                }
            }
        }
        hits
    }

    fn find_unique(&self, kind: StoreKind, old_text: &str) -> Result<usize, String> {
        let needle = old_text.to_ascii_lowercase();
        let matches: Vec<usize> = self
            .entries(kind)
            .iter()
            .enumerate()
            .filter(|(_, e)| e.raw.to_ascii_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect();
        match matches.len() {
            0 => Err(format!("no entry matched `{old_text}`")),
            1 => Ok(matches[0]),
            n => Err(format!(
                "{n} entries matched `{old_text}`; provide a more specific substring"
            )),
        }
    }

    fn persist(&self, kind: StoreKind) -> std::io::Result<()> {
        let dir = match kind {
            StoreKind::Project => &self.project_dir,
            _ => &self.dir,
        };
        let path = dir.join(kind.filename());
        let body = self
            .entries(kind)
            .iter()
            .map(|e| e.raw.as_str())
            .collect::<Vec<_>>()
            .join(&format!("\n{ENTRY_DELIM}\n"));
        fs::write(path, body)
    }
}

fn load_file(path: &Path) -> Vec<MemoryEntry> {
    match fs::read_to_string(path) {
        Ok(s) if !s.trim().is_empty() => s
            .split(ENTRY_DELIM)
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
            .map(|p| MemoryEntry { raw: p.to_string() })
            .collect(),
        _ => Vec::new(),
    }
}

fn joined_len(entries: &[MemoryEntry]) -> usize {
    if entries.is_empty() {
        0
    } else {
        entries.iter().map(|e| e.chars()).sum::<usize>()
            + (entries.len().saturating_sub(1)) * ENTRY_DELIM.len()
    }
}

fn joined_len_with(entries: &[MemoryEntry], extra: &str) -> usize {
    if entries.is_empty() {
        extra.chars().count()
    } else {
        joined_len(entries) + ENTRY_DELIM.len() + extra.chars().count()
    }
}
