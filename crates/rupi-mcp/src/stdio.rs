use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

use crate::client::Transport;
use crate::protocol::{JsonRpcRequest, JsonRpcResponse, McpError, McpResult};

pub struct StdioTransport {
    child: Child,
    stdin: tokio::process::ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
}

impl StdioTransport {
    pub async fn spawn(command: &str, args: &[String], env: &[(String, String)]) -> McpResult<Self> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().map_err(|e| McpError::Transport(e.to_string()))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Transport("missing stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Transport("missing stdout".into()))?;
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        })
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

#[async_trait::async_trait]
impl Transport for StdioTransport {
    async fn request(&mut self, req: JsonRpcRequest) -> McpResult<JsonRpcResponse> {
        let mut line = serde_json::to_string(&req).map_err(|e| McpError::Protocol(e.to_string()))?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| McpError::Transport(e.to_string()))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| McpError::Transport(e.to_string()))?;
        let mut response = String::new();
        self.stdout
            .read_line(&mut response)
            .await
            .map_err(|e| McpError::Transport(e.to_string()))?;
        serde_json::from_str(response.trim())
            .map_err(|e| McpError::Protocol(format!("invalid json-rpc: {e}: {response}")))
    }

    async fn notify(&mut self, req: JsonRpcRequest) -> McpResult<()> {
        let mut line = serde_json::to_string(&req).map_err(|e| McpError::Protocol(e.to_string()))?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| McpError::Transport(e.to_string()))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| McpError::Transport(e.to_string()))?;
        Ok(())
    }

    async fn close(&mut self) -> McpResult<()> {
        let _ = self.child.kill().await;
        Ok(())
    }
}
