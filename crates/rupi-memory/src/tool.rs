use crate::fts::SessionIndex;
use crate::store::{MemoryStore, MemoryTarget};
use async_trait::async_trait;
use rupi_agent::{Tool, ToolContext, ToolResult};
use serde_json::{json, Value};
use std::sync::Arc;

pub struct MemoryTool {
    store: Arc<MemoryStore>,
}

impl MemoryTool {
    pub fn new(store: Arc<MemoryStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for MemoryTool {
    fn name(&self) -> &str {
        "memory"
    }

    fn description(&self) -> &str {
        "Manage persistent memory that is injected into the system prompt at the start of each session. Actions: add, replace, remove. Targets: memory (environment/project facts, 2200 chars) and user (profile/preferences, 1375 chars). replace/remove match a unique substring via old_text. There is no read action — memory is already in the system prompt."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["add", "replace", "remove"]},
                "target": {"type": "string", "enum": ["memory", "user"]},
                "content": {"type": "string", "description": "New entry text for add/replace"},
                "old_text": {"type": "string", "description": "Unique substring identifying the entry for replace/remove"}
            },
            "required": ["action", "target"]
        })
    }

    fn prompt_snippet(&self) -> &str {
        "Save durable facts to MEMORY.md / USER.md"
    }

    fn prompt_guidelines(&self) -> &[&str] {
        &[
            "Save user preferences, environment facts, corrections, and conventions to memory proactively",
            "Skip trivia, easily re-discovered facts, and session-specific ephemera",
            "When memory is above 80% capacity, consolidate before adding",
        ]
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> ToolResult {
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let target = match args
            .get("target")
            .and_then(|v| v.as_str())
            .and_then(MemoryTarget::parse)
        {
            Some(t) => t,
            None => return ToolResult::err("target must be 'memory' or 'user'"),
        };
        let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
        let old_text = args.get("old_text").and_then(|v| v.as_str()).unwrap_or("");
        let result = match action {
            "add" => self.store.add(target, content),
            "replace" => self.store.replace(target, old_text, content),
            "remove" => self.store.remove(target, old_text),
            _ => return ToolResult::err("action must be add, replace, or remove"),
        };
        match result {
            Ok(msg) => {
                let entries = self.store.entries(target);
                let (used, limit) = self.store.usage(target);
                ToolResult::ok(format!(
                    "{msg}\nlive_entries ({}):\n{}\nusage: {used}/{limit}",
                    target.file_name(),
                    entries.join(&format!("\n{}\n", crate::store::ENTRY_SEP))
                ))
            }
            Err(e) => ToolResult::err(e),
        }
    }
}

pub struct SessionSearchTool {
    index: Arc<SessionIndex>,
}

impl SessionSearchTool {
    pub fn new(index: Arc<SessionIndex>) -> Self {
        Self { index }
    }
}

#[async_trait]
impl Tool for SessionSearchTool {
    fn name(&self) -> &str {
        "session_search"
    }

    fn description(&self) -> &str {
        "Search past session transcripts with FTS5. Pass query for discovery, or session_id plus offset/limit to browse a session."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "session_id": {"type": "string"},
                "offset": {"type": "integer"},
                "limit": {"type": "integer"}
            }
        })
    }

    fn prompt_snippet(&self) -> &str {
        "Search past sessions"
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> ToolResult {
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(8) as usize;
        if let Some(sid) = args.get("session_id").and_then(|v| v.as_str()) {
            let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            match self.index.browse(sid, offset, limit) {
                Ok(hits) => {
                    if hits.is_empty() {
                        ToolResult::ok("No messages in that window.")
                    } else {
                        let body = hits
                            .into_iter()
                            .map(|h| format!("[{} {}] {}: {}", h.session_id, h.timestamp, h.role, h.content))
                            .collect::<Vec<_>>()
                            .join("\n\n");
                        ToolResult::ok(body)
                    }
                }
                Err(e) => ToolResult::err(e.to_string()),
            }
        } else {
            let q = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            if q.trim().is_empty() {
                return ToolResult::err("provide query or session_id");
            }
            match self.index.search(q, limit) {
                Ok(hits) => {
                    if hits.is_empty() {
                        ToolResult::ok("No matching sessions.")
                    } else {
                        let body = hits
                            .into_iter()
                            .map(|h| {
                                format!(
                                    "session {} [{}]\n{}: {}",
                                    h.session_id, h.timestamp, h.role, h.content
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n\n");
                        ToolResult::ok(body)
                    }
                }
                Err(e) => ToolResult::err(e.to_string()),
            }
        }
    }
}
