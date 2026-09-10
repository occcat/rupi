use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct McpServersFile {
    #[serde(default, rename = "mcpServers")]
    pub mcp_servers: HashMap<String, McpServerConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct McpServerConfig {
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Promote listed tools as native rupi tools (`mcp_<server>_<tool>`).
    #[serde(default, rename = "directTools")]
    pub direct_tools: Option<bool>,
    #[serde(default)]
    pub disabled: bool,
}

pub fn load_mcp_config(path: &Path) -> std::io::Result<McpServersFile> {
    if !path.exists() {
        return Ok(McpServersFile::default());
    }
    let text = std::fs::read_to_string(path)?;
    let parsed: McpServersFile = serde_json::from_str(&text)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(parsed)
}

pub fn merge_mcp_files(global: McpServersFile, project: McpServersFile) -> McpServersFile {
    let mut out = global;
    for (k, v) in project.mcp_servers {
        out.mcp_servers.insert(k, v);
    }
    out
}
