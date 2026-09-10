mod bash;
mod edit;
mod find;
mod grep;
mod ls;
mod read;
mod write;

use async_trait::async_trait;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use bash::BashTool;
pub use edit::EditTool;
pub use find::FindTool;
pub use grep::GrepTool;
pub use ls::LsTool;
pub use read::ReadTool;
pub use write::WriteTool;

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
    pub details: Value,
}

impl ToolResult {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            details: Value::Null,
        }
    }

    pub fn err(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            details: Value::Null,
        }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = details;
        self
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("{0}")]
    Message(String),
    #[error("aborted")]
    Aborted,
}

#[derive(Clone)]
pub struct ToolContext {
    pub cwd: PathBuf,
    pub abort: Option<tokio::sync::watch::Receiver<bool>>,
}

impl ToolContext {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            abort: None,
        }
    }

    pub fn aborted(&self) -> bool {
        self.abort.as_ref().map(|r| *r.borrow()).unwrap_or(false)
    }

    pub fn resolve(&self, path: &str) -> PathBuf {
        resolve_to_cwd(path, &self.cwd)
    }
}

pub fn resolve_to_cwd(path: &str, cwd: &Path) -> PathBuf {
    let expanded = if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            PathBuf::from(home).join(rest)
        } else {
            PathBuf::from(path)
        }
    } else if path == "~" {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(path))
    } else {
        PathBuf::from(path)
    };
    if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;
    fn prompt_snippet(&self) -> &str {
        self.description()
    }
    fn prompt_guidelines(&self) -> &[&str] {
        &[]
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> ToolResult;
}

impl dyn Tool {
    pub fn spec(&self) -> rupi_ai::ToolSpec {
        rupi_ai::ToolSpec {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: self.parameters(),
        }
    }
}

pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new(tools: Vec<Arc<dyn Tool>>) -> Self {
        Self { tools }
    }

    pub fn empty() -> Self {
        Self { tools: Vec::new() }
    }

    pub fn push(&mut self, tool: Arc<dyn Tool>) {
        self.tools.push(tool);
    }

    pub fn extend(&mut self, tools: impl IntoIterator<Item = Arc<dyn Tool>>) {
        self.tools.extend(tools);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.name() == name).cloned()
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.name().to_string()).collect()
    }

    pub fn specs(&self) -> Vec<rupi_ai::ToolSpec> {
        self.tools.iter().map(|t| t.spec()).collect()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Arc<dyn Tool>> {
        self.tools.iter()
    }

    pub fn snippets(&self) -> Vec<(String, String)> {
        self.tools
            .iter()
            .map(|t| (t.name().to_string(), t.prompt_snippet().to_string()))
            .collect()
    }

    pub fn guidelines(&self) -> Vec<String> {
        let mut out = Vec::new();
        for t in &self.tools {
            for g in t.prompt_guidelines() {
                out.push((*g).to_string());
            }
        }
        out
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolName {
    Read,
    Write,
    Edit,
    Bash,
    Grep,
    Find,
    Ls,
}

impl ToolName {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Edit => "edit",
            Self::Bash => "bash",
            Self::Grep => "grep",
            Self::Find => "find",
            Self::Ls => "ls",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "read" => Some(Self::Read),
            "write" => Some(Self::Write),
            "edit" => Some(Self::Edit),
            "bash" => Some(Self::Bash),
            "grep" => Some(Self::Grep),
            "find" => Some(Self::Find),
            "ls" => Some(Self::Ls),
            _ => None,
        }
    }

    pub fn all() -> &'static [ToolName] {
        &[
            Self::Read,
            Self::Bash,
            Self::Edit,
            Self::Write,
            Self::Grep,
            Self::Find,
            Self::Ls,
        ]
    }
}

pub fn create_tool(name: ToolName) -> Arc<dyn Tool> {
    match name {
        ToolName::Read => Arc::new(ReadTool),
        ToolName::Write => Arc::new(WriteTool),
        ToolName::Edit => Arc::new(EditTool),
        ToolName::Bash => Arc::new(BashTool),
        ToolName::Grep => Arc::new(GrepTool),
        ToolName::Find => Arc::new(FindTool),
        ToolName::Ls => Arc::new(LsTool),
    }
}

pub fn builtin_tools(exclude: &[String]) -> Vec<Arc<dyn Tool>> {
    ToolName::all()
        .iter()
        .copied()
        .filter(|n| !exclude.iter().any(|e| e == n.as_str()))
        .map(create_tool)
        .collect()
}

impl Clone for ToolRegistry {
    fn clone(&self) -> Self {
        Self {
            tools: self.tools.clone(),
        }
    }
}
