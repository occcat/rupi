//! 云记忆：权威在 Postgres。`MEMORY.md` 不是主存。

use crate::cache::Cache;
use crate::db::{self, PgPool};
use async_trait::async_trait;
use rupi_core::ToolDefinition;
use rupi_memory::{FrozenMemory, MemoryProvider, RecallStatus};

#[derive(Clone)]
pub struct PostgresMemory {
    pool: PgPool,
    tenant_id: String,
    session_id: String,
    cache: Cache,
}

impl PostgresMemory {
    pub fn new(pool: PgPool, tenant_id: String, session_id: String, cache: Cache) -> Self {
        Self {
            pool,
            tenant_id,
            session_id,
            cache,
        }
    }

    pub async fn frozen(&self) -> FrozenMemory {
        let rows = db::list_memories(&self.pool, &self.tenant_id, Some(&self.session_id))
            .await
            .unwrap_or_default();
        let mut memory = String::new();
        let mut user = String::new();
        let mut failures = String::new();
        let has_core = rows.iter().any(|(layer, _, _)| layer == "core");
        for (layer, _scope, content) in rows {
            match layer.as_str() {
                "user" => {
                    user.push_str(&content);
                    if !content.ends_with('\n') {
                        user.push('\n');
                    }
                }
                "failure" => {
                    failures.push_str(&content);
                    if !content.ends_with('\n') {
                        failures.push('\n');
                    }
                }
                "core" => {
                    memory.push_str(&content);
                    if !content.ends_with('\n') {
                        memory.push('\n');
                    }
                }
                _ if !has_core => {
                    memory.push_str(&content);
                    if !content.ends_with('\n') {
                        memory.push('\n');
                    }
                }
                _ => {}
            }
        }
        FrozenMemory {
            memory,
            user,
            failures,
        }
    }
}

#[async_trait]
impl MemoryProvider for PostgresMemory {
    async fn initialize(&mut self, _home: &std::path::Path) -> anyhow::Result<()> {
        Ok(())
    }

    fn system_prompt_block(&self) -> String {
        "\n<MemoryGuidance>\nLong-term memory is stored in the cloud database (not sandbox MEMORY.md). Save via the `memory` tool (op=add). Prefix `[core]` to pin into every prompt. Recall with `memory_search`; use `session_search` for past chats. Do NOT store secrets.\n</MemoryGuidance>\n".into()
    }

    fn tool_schemas(&self) -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "memory".into(),
                description: "Manage long-term tenant memory in Postgres".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "op": {"type": "string", "enum": ["add", "replace", "remove"]},
                        "entry": {"type": "string"},
                        "scope": {"type": "string", "enum": ["global", "project", "tenant"], "default": "tenant"}
                    },
                    "required": ["op", "entry"]
                }),
                prompt_snippet: Some(
                    "memory(op, entry, scope=tenant): persist durable facts in the cloud db".into(),
                ),
            },
            rupi_memory::MemoryStore::memory_search_tool_definition(),
            rupi_memory::MemoryStore::session_search_tool_definition(),
        ]
    }

    async fn handle_tool_call(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> anyhow::Result<Option<String>> {
        if name == "memory" {
            let op = args.get("op").and_then(|v| v.as_str()).unwrap_or("add");
            let entry = args.get("entry").and_then(|v| v.as_str()).unwrap_or("");
            let scope = args
                .get("scope")
                .and_then(|v| v.as_str())
                .unwrap_or("tenant");
            match op {
                "add" => {
                    db::insert_memory(
                        &self.pool,
                        &self.tenant_id,
                        Some(&self.session_id),
                        if scope == "project" || scope == "workspace" {
                            "workspace"
                        } else {
                            "tenant"
                        },
                        entry,
                    )
                    .await?;
                    self.cache.invalidate_memory(&self.tenant_id).await;
                    self.cache
                        .invalidate_session(&self.tenant_id, &self.session_id)
                        .await;
                }
                "replace" | "remove" => {
                    // 第一刀：追加一条说明性写入，权威仍在库。
                    db::insert_memory(
                        &self.pool,
                        &self.tenant_id,
                        Some(&self.session_id),
                        "tenant",
                        &format!("[{op}] {entry}"),
                    )
                    .await?;
                    self.cache.invalidate_memory(&self.tenant_id).await;
                }
                _ => anyhow::bail!("unknown memory op: {op}"),
            }
            let rows = db::list_memories(&self.pool, &self.tenant_id, Some(&self.session_id)).await?;
            let live: String = rows
                .into_iter()
                .map(|(_, _, c)| c)
                .collect::<Vec<_>>()
                .join("");
            return Ok(Some(format!(
                "memory updated (postgres). Takes effect in prompt next session.\n{live}"
            )));
        }
        if name == "memory_search" {
            let q = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(5) as i64;
            let hits = db::search_memories(&self.pool, &self.tenant_id, q, limit.min(20)).await?;
            if hits.is_empty() {
                return Ok(Some("no matching memories".into()));
            }
            let lines: Vec<String> = hits
                .into_iter()
                .map(|(scope, c)| format!("[{scope}] {c}"))
                .collect();
            return Ok(Some(lines.join("\n---\n")));
        }
        if name == "session_search" {
            let q = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(5) as i64;
            let hits = db::search_sessions(&self.pool, &self.tenant_id, q, limit.min(20)).await?;
            if hits.is_empty() {
                return Ok(Some("no matching sessions".into()));
            }
            let lines: Vec<String> = hits
                .into_iter()
                .map(|(sid, c)| format!("[session {sid}] {c}"))
                .collect();
            return Ok(Some(lines.join("\n---\n")));
        }
        Ok(None)
    }

    fn recall_status(&self) -> Option<RecallStatus> {
        None
    }
}
