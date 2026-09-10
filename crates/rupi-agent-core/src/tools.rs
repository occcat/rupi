use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use rupi_ai::{ContentBlock, Message, ToolDefinition};
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct AgentToolResult {
    pub text: String,
    pub is_error: bool,
    pub details: Value,
    pub terminate: bool,
}

impl AgentToolResult {
    pub fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
            details: Value::Null,
            terminate: false,
        }
    }

    pub fn err(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
            details: Value::Null,
            terminate: false,
        }
    }
}

#[async_trait]
pub trait AgentTool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;
    fn label(&self) -> &str {
        self.name()
    }
    fn execution_mode(&self) -> crate::ToolExecutionMode {
        crate::ToolExecutionMode::Parallel
    }
    async fn execute(&self, tool_call_id: &str, args: Value) -> AgentToolResult;

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: self.parameters(),
            label: Some(self.label().to_string()),
        }
    }
}

#[derive(Clone, Default)]
pub struct ToolSet {
    tools: HashMap<String, Arc<dyn AgentTool>>,
}

impl ToolSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Arc<dyn AgentTool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn AgentTool>> {
        self.tools.get(name).cloned()
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|t| t.definition()).collect()
    }

    pub fn names(&self) -> Vec<String> {
        let mut n: Vec<_> = self.tools.keys().cloned().collect();
        n.sort();
        n
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }
}

pub struct ToolExecutor;

impl ToolExecutor {
    pub async fn execute(
        tools: &ToolSet,
        call: &ContentBlock,
    ) -> Message {
        let ContentBlock::ToolCall { id, name, arguments } = call else {
            return Message::tool_result("", "unknown", "invalid tool call block", true);
        };
        match tools.get(name) {
            Some(tool) => {
                let result = tool.execute(id, arguments.clone()).await;
                let mut msg = Message::tool_result(id, name, result.text, result.is_error);
                if let Message::ToolResult {
                    details, terminate, ..
                } = &mut msg
                {
                    *details = Some(result.details);
                    *terminate = Some(result.terminate).filter(|t| *t);
                }
                msg
            }
            None => Message::tool_result(
                id,
                name,
                format!("unknown tool: {name}"),
                true,
            ),
        }
    }
}
