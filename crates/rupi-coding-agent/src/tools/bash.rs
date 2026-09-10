use async_trait::async_trait;
use rupi_agent_core::{AgentTool, AgentToolResult};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::timeout;

pub struct BashTool {
    pub cwd: PathBuf,
    pub timeout_ms: u64,
}

#[async_trait]
impl AgentTool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        "Run a bash command in the workspace. Output is truncated if large."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "timeout_ms": {"type": "integer"}
            },
            "required": ["command"]
        })
    }
    async fn execute(&self, _id: &str, args: Value) -> AgentToolResult {
        let command = args["command"].as_str().unwrap_or("");
        if command.trim().is_empty() {
            return AgentToolResult::err("empty command");
        }
        let t = args["timeout_ms"]
            .as_u64()
            .unwrap_or(self.timeout_ms)
            .min(300_000);
        let mut child = match Command::new("bash")
            .arg("-lc")
            .arg(command)
            .current_dir(&self.cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => return AgentToolResult::err(e.to_string()),
        };
        let fut = async {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            if let Some(mut out) = child.stdout.take() {
                let _ = out.read_to_end(&mut stdout).await;
            }
            if let Some(mut err) = child.stderr.take() {
                let _ = err.read_to_end(&mut stderr).await;
            }
            let status = child.wait().await;
            (stdout, stderr, status)
        };
        match timeout(Duration::from_millis(t), fut).await {
            Ok((stdout, stderr, status)) => {
                let code = status.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
                let mut text = String::new();
                text.push_str(&truncate_bytes(&stdout, 32_000));
                if !stderr.is_empty() {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str("stderr:\n");
                    text.push_str(&truncate_bytes(&stderr, 16_000));
                }
                text.push_str(&format!("\nexit {code}"));
                if code == 0 {
                    AgentToolResult::ok(text)
                } else {
                    AgentToolResult::err(text)
                }
            }
            Err(_) => {
                let _ = child.kill().await;
                AgentToolResult::err(format!("timed out after {t}ms"))
            }
        }
    }
}

fn truncate_bytes(bytes: &[u8], max: usize) -> String {
    let s = String::from_utf8_lossy(bytes);
    if s.len() <= max {
        s.into_owned()
    } else {
        format!("{}…\n[truncated {} bytes]", &s[..max], s.len().saturating_sub(max))
    }
}
