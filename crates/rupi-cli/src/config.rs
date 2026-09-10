use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub home: PathBuf,
    pub sessions: PathBuf,
    pub skills: PathBuf,
    pub memories: PathBuf,
    pub settings: PathBuf,
    pub mcp: PathBuf,
    pub state_db: PathBuf,
}

impl AppPaths {
    pub fn resolve(session_dir: Option<String>) -> Result<Self> {
        let home = if let Ok(h) = std::env::var("RUPI_HOME") {
            PathBuf::from(h)
        } else {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".rupi")
        };
        let sessions = session_dir
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("sessions"));
        Ok(Self {
            sessions,
            skills: home.join("skills"),
            memories: home.join("memories"),
            settings: home.join("settings.json"),
            mcp: home.join("mcp.json"),
            state_db: home.join("state.db"),
            home,
        })
    }

    pub fn ensure(&self) -> Result<()> {
        std::fs::create_dir_all(&self.home)?;
        std::fs::create_dir_all(&self.sessions)?;
        std::fs::create_dir_all(&self.skills)?;
        std::fs::create_dir_all(&self.memories)?;
        if !self.settings.exists() {
            std::fs::write(&self.settings, "{}\n")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub thinking: Option<String>,
    #[serde(default)]
    pub compaction: CompactionSettingsFile,
    #[serde(default)]
    pub memory: MemorySettingsFile,
    #[serde(default)]
    pub skills: SkillsSettingsFile,
    #[serde(default)]
    pub mcp: McpSettingsFile,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionSettingsFile {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_reserve")]
    pub reserve_tokens: u32,
}

impl Default for CompactionSettingsFile {
    fn default() -> Self {
        Self {
            enabled: true,
            reserve_tokens: 16_384,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemorySettingsFile {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub write_approval: bool,
}

impl Default for MemorySettingsFile {
    fn default() -> Self {
        Self {
            enabled: true,
            write_approval: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillsSettingsFile {
    #[serde(default = "default_true")]
    pub self_accumulate: bool,
    #[serde(default)]
    pub write_approval: bool,
}

impl Default for SkillsSettingsFile {
    fn default() -> Self {
        Self {
            self_accumulate: true,
            write_approval: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpSettingsFile {
    #[serde(default = "default_true")]
    pub direct_tools: bool,
}

impl Default for McpSettingsFile {
    fn default() -> Self {
        Self { direct_tools: true }
    }
}

fn default_true() -> bool {
    true
}
fn default_reserve() -> u32 {
    16_384
}

pub fn load_settings(path: &Path) -> Settings {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn project_rupi_dir(cwd: &Path) -> PathBuf {
    cwd.join(".rupi")
}

pub fn seed_bundled_skills(skills_dir: &Path) -> Result<()> {
    let dest = skills_dir.join("self-improvement");
    if dest.join("SKILL.md").exists() {
        return Ok(());
    }
    std::fs::create_dir_all(dest.join("references"))?;
    std::fs::write(
        dest.join("SKILL.md"),
        r#"---
name: self-improvement
description: How rupi persists memory and accumulates skills across sessions. Use when deciding what to save after a task, or when creating/updating SKILL.md files.
---

# Self-improvement

## Memory vs skills

- `memory` tool → short durable facts (MEMORY.md / USER.md). Always in the next session's system prompt.
- `skill_manage` → procedures that should load only when relevant.

## When to save a skill

Non-trivial workflows, user corrections, tool quirks, or a sequence that would be expensive to rediscover.

## Skill shape

Class-level names (`rust-testing`, `postgres-ops`), never session artifacts. Keep SKILL.md actionable; put transcripts in `references/`.

## Read before write

Call `skill_view` before `skill_manage` patch/edit in background review.
"#,
    )?;
    Ok(())
}
