use std::sync::Arc;

use async_trait::async_trait;
use rupi_agent_core::{run_subagent, AgentTool, AgentToolResult, SubagentConfig, ToolSet};
use rupi_ai::{Model, ProviderClient};
use serde_json::{json, Value};

pub struct SubagentTool {
    pub model: Model,
    pub provider: Arc<dyn ProviderClient>,
    pub tools: ToolSet,
}

#[async_trait]
impl AgentTool for SubagentTool {
    fn name(&self) -> &str {
        "subagent"
    }
    fn description(&self) -> &str {
        "Spawn an isolated sub-agent with read-only tools (read/grep/find/ls). \
         Use for research or exploration that should not mutate the workspace. \
         Returns only a summary to the parent."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task": {"type": "string"},
                "label": {"type": "string"}
            },
            "required": ["task"]
        })
    }
    async fn execute(&self, _id: &str, args: Value) -> AgentToolResult {
        let task = args["task"].as_str().unwrap_or("").trim();
        if task.is_empty() {
            return AgentToolResult::err("task required");
        }
        let label = args["label"].as_str().unwrap_or("explore").to_string();
        match run_subagent(
            self.model.clone(),
            self.provider.clone(),
            self.tools.clone(),
            task,
            SubagentConfig {
                label: label.clone(),
                system_prompt: "You are a read-only research sub-agent. Investigate with read/grep/find/ls. Do not attempt to modify files. Return a concise summary.".into(),
                isolated: true,
            },
        )
        .await
        {
            Ok(r) => AgentToolResult::ok(format!("[{}] {}", r.label, r.final_text)),
            Err(e) => AgentToolResult::err(e.to_string()),
        }
    }
}
