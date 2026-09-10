use std::sync::Arc;

use rupi_ai::{Message, Model, ProviderClient};

use crate::agent::Agent;
use crate::tools::ToolSet;
use crate::types::AgentResult;

#[derive(Debug, Clone)]
pub struct SubagentConfig {
    pub label: String,
    pub system_prompt: String,
    pub isolated: bool,
}

#[derive(Debug, Clone)]
pub struct SubagentResult {
    pub label: String,
    pub messages: Vec<Message>,
    pub final_text: String,
}

/// Run a nested agent with a restricted tool set. Isolated sub-agents do not
/// share the parent transcript; they only return a summary.
pub async fn run_subagent(
    model: Model,
    provider: Arc<dyn ProviderClient>,
    tools: ToolSet,
    task: &str,
    config: SubagentConfig,
) -> AgentResult<SubagentResult> {
    let mut agent = Agent::new(config.system_prompt, model).with_provider(provider);
    agent.set_tools(tools);
    let new_msgs = agent.prompt(task).await?;
    let final_text = new_msgs
        .iter()
        .rev()
        .find_map(|m| match m {
            Message::Assistant { content, .. } => {
                let t = rupi_ai::content_text(content);
                if t.is_empty() {
                    None
                } else {
                    Some(t)
                }
            }
            _ => None,
        })
        .unwrap_or_default();
    Ok(SubagentResult {
        label: config.label,
        messages: if config.isolated {
            vec![Message::assistant_text(&final_text)]
        } else {
            new_msgs
        },
        final_text,
    })
}
