use super::{Tool, ToolContext, ToolResult};
use crate::truncate::{format_size, truncate_tail, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

pub struct BashTool;

#[derive(Deserialize)]
struct BashArgs {
    command: String,
    #[serde(default)]
    timeout: Option<f64>,
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Execute bash commands (ls, grep, find, etc.). Optional timeout is in seconds."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Shell command to execute"},
                "timeout": {"type": "number", "description": "Timeout in seconds (optional, no default timeout)"}
            },
            "required": ["command"]
        })
    }

    fn prompt_snippet(&self) -> &str {
        "Execute bash commands (ls, grep, find, etc.)"
    }

    fn prompt_guidelines(&self) -> &[&str] {
        &[
            "Use bash for file operations like ls, rg, find when dedicated tools are unavailable",
            "You can inspect RUPI_* environment variables for current model and session details.",
        ]
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> ToolResult {
        if ctx.aborted() {
            return ToolResult::err("Operation aborted");
        }
        let args: BashArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolResult::err(format!("invalid arguments: {e}")),
        };
        let timeout = args
            .timeout
            .filter(|t| t.is_finite() && *t > 0.0)
            .map(|t| Duration::from_secs_f64(t.min(2_147_483.0)));

        let mut cmd = Command::new("bash");
        cmd.arg("-lc")
            .arg(&args.command)
            .current_dir(&ctx.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        unsafe {
            cmd.pre_exec(|| {
                libc::setpgid(0, 0);
                Ok(())
            });
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return ToolResult::err(format!("failed to spawn bash: {e}")),
        };
        let pid = child.id();

        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let stdout_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = stdout.read_to_end(&mut buf).await;
            buf
        });
        let stderr_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = stderr.read_to_end(&mut buf).await;
            buf
        });

        let wait_result = if let Some(dur) = timeout {
            match tokio::time::timeout(dur, child.wait()).await {
                Ok(r) => r.map_err(|e| e.to_string()),
                Err(_) => {
                    #[cfg(unix)]
                    if let Some(pid) = pid {
                        unsafe {
                            libc::kill(-(pid as i32), libc::SIGKILL);
                        }
                    }
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    let _ = stdout_task.await;
                    let _ = stderr_task.await;
                    return ToolResult::err(format!(
                        "command timed out after {} seconds",
                        dur.as_secs_f64()
                    ));
                }
            }
        } else {
            child.wait().await.map_err(|e| e.to_string())
        };

        let stdout_bytes = stdout_task.await.unwrap_or_default();
        let stderr_bytes = stderr_task.await.unwrap_or_default();
        let stdout = String::from_utf8_lossy(&stdout_bytes).into_owned();
        let stderr = String::from_utf8_lossy(&stderr_bytes).into_owned();

        match wait_result {
            Ok(st) => {
                let mut combined = String::new();
                if !stdout.is_empty() {
                    combined.push_str(&stdout);
                }
                if !stderr.is_empty() {
                    if !combined.is_empty() && !combined.ends_with('\n') {
                        combined.push('\n');
                    }
                    combined.push_str(&stderr);
                }
                let truncation = truncate_tail(&combined, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
                let mut out = truncation.content;
                if truncation.truncated {
                    out.push_str(&format!(
                        "\n\n[truncated: kept last {} / {} lines, {} / {}]",
                        truncation.output_lines,
                        truncation.total_lines,
                        format_size(truncation.output_bytes),
                        format_size(truncation.total_bytes)
                    ));
                }
                if !st.success() {
                    out.push_str(&format!(
                        "\n\nCommand exited with code {}",
                        st.code().unwrap_or(-1)
                    ));
                    ToolResult::err(out).with_details(json!({"exit_code": st.code()}))
                } else if out.is_empty() {
                    ToolResult::ok("(no output)")
                } else {
                    ToolResult::ok(out)
                }
            }
            Err(e) => ToolResult::err(e),
        }
    }
}
