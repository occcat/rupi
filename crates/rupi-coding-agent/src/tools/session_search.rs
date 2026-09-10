use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rupi_agent_core::{AgentTool, AgentToolResult};
use rupi_memory::SessionSearchIndex;
use serde_json::{json, Value};

pub struct SessionSearchTool {
    pub index: Arc<Mutex<SessionSearchIndex>>,
}

#[async_trait]
impl AgentTool for SessionSearchTool {
    fn name(&self) -> &str {
        "session_search"
    }
    fn description(&self) -> &str {
        "Search unbounded past session transcripts (Hermes evidence layer, SQLite FTS5). \
         action=search (default) runs a full-text query; action=scroll reads a session forward from a timestamp."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["search", "scroll"]},
                "query": {"type": "string"},
                "session_id": {"type": "string"},
                "after_ts": {"type": "integer"},
                "limit": {"type": "integer"}
            }
        })
    }
    async fn execute(&self, _id: &str, args: Value) -> AgentToolResult {
        let action = args["action"].as_str().unwrap_or("search");
        let limit = args["limit"].as_u64().unwrap_or(8) as usize;
        let idx = match self.index.lock() {
            Ok(g) => g,
            Err(e) => return AgentToolResult::err(e.to_string()),
        };
        let result = match action {
            "scroll" => {
                let sid = args["session_id"].as_str().unwrap_or("");
                let after = args["after_ts"].as_i64().unwrap_or(0);
                idx.scroll(sid, after, limit)
            }
            _ => {
                let q = args["query"].as_str().or(args["content"].as_str()).unwrap_or("");
                if q.trim().is_empty() {
                    return AgentToolResult::err("query required");
                }
                idx.search(q, limit)
            }
        };
        match result {
            Ok(hits) if hits.is_empty() => AgentToolResult::ok("no matches"),
            Ok(hits) => {
                let text = hits
                    .iter()
                    .map(|h| {
                        format!(
                            "[{} {} {}] {}",
                            h.session_id, h.role, h.timestamp, h.text
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                AgentToolResult::ok(text)
            }
            Err(e) => AgentToolResult::err(e.to_string()),
        }
    }
}
