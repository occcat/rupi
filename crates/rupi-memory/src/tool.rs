use async_trait::async_trait;
use rupi_agent_core::{AgentTool, AgentToolResult};
use rupi_ai::ToolDefinition;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

use crate::snapshot::{render_memory_block, MemorySnapshot};
use crate::store::{MemoryStore, StoreKind};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryAction {
    Add,
    Replace,
    Remove,
    Search,
}

pub struct MemoryTool {
    pub store: Arc<Mutex<MemoryStore>>,
    pub snapshot: MemorySnapshot,
    pub write_approval: bool,
    pub pending: Arc<Mutex<Vec<Value>>>,
}

impl MemoryTool {
    pub fn new(store: MemoryStore) -> Self {
        let snapshot = MemorySnapshot::capture(&store);
        Self {
            store: Arc::new(Mutex::new(store)),
            snapshot,
            write_approval: false,
            pending: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn frozen_prompt_block(&self) -> String {
        render_memory_block(&self.snapshot)
    }

    pub fn apply(&self, args: &Value) -> Result<String, String> {
        let action = args["action"].as_str().unwrap_or("");
        let target = StoreKind::parse(args["target"].as_str().unwrap_or("memory"))
            .ok_or_else(|| "target must be `memory` or `user`".to_string())?;
        let mut store = self.store.lock().map_err(|e| e.to_string())?;
        match action {
            "add" => {
                let content = args["content"].as_str().ok_or("content required")?;
                if self.write_approval {
                    self.pending.lock().unwrap().push(args.clone());
                    return Ok("staged for approval".into());
                }
                store.add(target, content)
            }
            "replace" => {
                let old = args["old_text"].as_str().ok_or("old_text required")?;
                let content = args["content"].as_str().ok_or("content required")?;
                if self.write_approval {
                    self.pending.lock().unwrap().push(args.clone());
                    return Ok("staged for approval".into());
                }
                store.replace(target, old, content)
            }
            "remove" => {
                let old = args["old_text"].as_str().ok_or("old_text required")?;
                if self.write_approval {
                    self.pending.lock().unwrap().push(args.clone());
                    return Ok("staged for approval".into());
                }
                store.remove(target, old)
            }
            "search" => {
                let q = args["query"].as_str().or(args["content"].as_str()).unwrap_or("");
                let hits = store.search(q);
                if hits.is_empty() {
                    Ok("no matches".into())
                } else {
                    Ok(hits
                        .into_iter()
                        .map(|(k, e)| format!("[{}] {}", k.as_str(), e.raw))
                        .collect::<Vec<_>>()
                        .join("\n"))
                }
            }
            _ => Err("action must be add, replace, remove, or search".into()),
        }
    }
}

pub fn memory_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "memory".into(),
        description: "Manage durable MEMORY.md / USER.md notes. Actions: add, replace, remove, search. \
             Frozen snapshot is in the system prompt; tool responses show live state. \
             Prefix an entry with [core] to always inject it; other entries are extended and retrieved via search."
            .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["add", "replace", "remove", "search"]},
                "target": {"type": "string", "enum": ["memory", "user"], "default": "memory"},
                "content": {"type": "string"},
                "old_text": {"type": "string"},
                "query": {"type": "string"}
            },
            "required": ["action"]
        }),
        label: Some("memory".into()),
    }
}

#[async_trait]
impl AgentTool for MemoryTool {
    fn name(&self) -> &str {
        "memory"
    }
    fn description(&self) -> &str {
        "Manage durable MEMORY.md / USER.md notes (Hermes-style)."
    }
    fn parameters(&self) -> Value {
        memory_tool_definition().parameters
    }
    async fn execute(&self, _id: &str, args: Value) -> AgentToolResult {
        match self.apply(&args) {
            Ok(s) => AgentToolResult::ok(s),
            Err(e) => AgentToolResult::err(e),
        }
    }
}
