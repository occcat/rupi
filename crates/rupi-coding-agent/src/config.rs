use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct ConfigPaths {
    pub home: PathBuf,
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    pub sessions_dir: PathBuf,
    pub memories_dir: PathBuf,
    pub project_memories_dir: PathBuf,
    pub skills_dir: PathBuf,
}

impl ConfigPaths {
    pub fn resolve(cwd: impl Into<PathBuf>) -> Self {
        let cwd = cwd.into();
        let home = std::env::var("RUPI_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                dirs_home().join(".rupi")
            });
        Self {
            agent_dir: home.join("agent"),
            sessions_dir: home.join("sessions"),
            memories_dir: home.join("memories"),
            project_memories_dir: cwd.join(".rupi").join("memories"),
            skills_dir: home.join("skills"),
            home,
            cwd,
        }
    }

    pub fn mcp_config_paths(&self) -> Vec<PathBuf> {
        vec![
            self.home.join("mcp.json"),
            self.agent_dir.join("mcp.json"),
            self.cwd.join(".rupi").join("mcp.json"),
            self.cwd.join(".pi").join("mcp.json"),
        ]
    }
}

fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSettings {
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default)]
    pub thinking_level: Option<String>,
    #[serde(default = "default_tools")]
    pub default_tools: Vec<String>,
    #[serde(default)]
    pub memory_enabled: bool,
    #[serde(default)]
    pub skill_accumulation: bool,
    #[serde(default)]
    pub write_approval: bool,
    #[serde(default)]
    pub sandbox: bool,
}

fn default_model() -> String {
    "openai/gpt-4o".into()
}

fn default_tools() -> Vec<String> {
    vec![
        "read".into(),
        "bash".into(),
        "edit".into(),
        "write".into(),
    ]
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            model: default_model(),
            thinking_level: Some("medium".into()),
            default_tools: default_tools(),
            memory_enabled: true,
            skill_accumulation: true,
            write_approval: false,
            sandbox: true,
        }
    }
}

impl AgentSettings {
    pub fn load(paths: &ConfigPaths) -> Self {
        let candidates = [
            paths.agent_dir.join("settings.json"),
            paths.home.join("settings.json"),
            paths.cwd.join(".rupi").join("settings.json"),
        ];
        for p in candidates {
            if let Ok(raw) = std::fs::read_to_string(p) {
                if let Ok(s) = serde_json::from_str(&raw) {
                    return s;
                }
            }
        }
        Self::default()
    }
}

pub fn read_optional(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}
