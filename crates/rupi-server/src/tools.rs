//! 云工具表：read/write/edit/glob/grep/bash 只经 [`rupi_runtime::Executor`]。
//! 控制面进程不 `sh -c`。think 留在控制面。

use async_trait::async_trait;
use rupi_core::{CancelFlag, ToolDefinition};
use rupi_runtime::{
    ExecRequest, Executor, FsEditRequest, FsReadRequest, FsWriteRequest, GlobRequest, GrepRequest,
    WorkspaceHandle,
};
use rupi_tools::{Tool, ToolOutput, ToolRegistry};
use std::sync::Arc;

pub fn cloud_tools(exec: Arc<dyn Executor>, handle: WorkspaceHandle) -> ToolRegistry {
    let mut r = ToolRegistry::new();
    r.register(Arc::new(ExecFsTool {
        exec: exec.clone(),
        handle: handle.clone(),
        kind: FsKind::Read,
    }));
    r.register(Arc::new(ExecFsTool {
        exec: exec.clone(),
        handle: handle.clone(),
        kind: FsKind::Write,
    }));
    r.register(Arc::new(ExecFsTool {
        exec: exec.clone(),
        handle: handle.clone(),
        kind: FsKind::Edit,
    }));
    r.register(Arc::new(ExecFsTool {
        exec: exec.clone(),
        handle: handle.clone(),
        kind: FsKind::Glob,
    }));
    r.register(Arc::new(ExecFsTool {
        exec: exec.clone(),
        handle: handle.clone(),
        kind: FsKind::Grep,
    }));
    r.register(Arc::new(ExecBashTool {
        exec,
        handle,
    }));
    r.register(Arc::new(rupi_tools::ThinkTool));
    r
}

enum FsKind {
    Read,
    Write,
    Edit,
    Glob,
    Grep,
}

struct ExecFsTool {
    exec: Arc<dyn Executor>,
    handle: WorkspaceHandle,
    kind: FsKind,
}

#[async_trait]
impl Tool for ExecFsTool {
    fn definition(&self) -> ToolDefinition {
        match self.kind {
            FsKind::Read => ToolDefinition {
                name: "read".into(),
                description: "Read a file in the remote workspace".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "offset": {"type": "integer"},
                        "limit": {"type": "integer"}
                    },
                    "required": ["path"]
                }),
                prompt_snippet: Some("read(path, offset?, limit?): read workspace file".into()),
            },
            FsKind::Write => ToolDefinition {
                name: "write".into(),
                description: "Write a file in the remote workspace".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "content": {"type": "string"}
                    },
                    "required": ["path", "content"]
                }),
                prompt_snippet: Some("write(path, content): create/overwrite remote file".into()),
            },
            FsKind::Edit => ToolDefinition {
                name: "edit".into(),
                description: "Edit a file in the remote workspace".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "old_string": {"type": "string"},
                        "new_string": {"type": "string"},
                        "replace_all": {"type": "boolean"}
                    },
                    "required": ["path"]
                }),
                prompt_snippet: Some("edit(path, old_string, new_string): patch remote file".into()),
            },
            FsKind::Glob => ToolDefinition {
                name: "glob".into(),
                description: "List files in the remote workspace".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string"},
                        "path": {"type": "string"}
                    },
                    "required": ["pattern"]
                }),
                prompt_snippet: Some("glob(pattern, path?): list remote files".into()),
            },
            FsKind::Grep => ToolDefinition {
                name: "grep".into(),
                description: "Search remote workspace contents".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string"},
                        "path": {"type": "string"},
                        "include": {"type": "string"}
                    },
                    "required": ["pattern"]
                }),
                prompt_snippet: Some("grep(pattern, path?): search remote files".into()),
            },
        }
    }

    async fn execute(&self, arguments: serde_json::Value) -> anyhow::Result<ToolOutput> {
        let t = match self.kind {
            FsKind::Read => {
                self.exec
                    .fs_read(
                        &self.handle,
                        FsReadRequest {
                            path: arg_str(&arguments, "path"),
                            offset: arguments.get("offset").and_then(|v| v.as_u64()),
                            limit: arguments.get("limit").and_then(|v| v.as_u64()),
                        },
                    )
                    .await?
            }
            FsKind::Write => {
                self.exec
                    .fs_write(
                        &self.handle,
                        FsWriteRequest {
                            path: arg_str(&arguments, "path"),
                            content: arg_str(&arguments, "content"),
                        },
                    )
                    .await?
            }
            FsKind::Edit => {
                self.exec
                    .fs_edit(
                        &self.handle,
                        FsEditRequest {
                            path: arg_str(&arguments, "path"),
                            arguments,
                        },
                    )
                    .await?
            }
            FsKind::Glob => {
                self.exec
                    .glob(
                        &self.handle,
                        GlobRequest {
                            pattern: arg_str(&arguments, "pattern"),
                            path: arguments
                                .get("path")
                                .and_then(|v| v.as_str())
                                .map(str::to_owned),
                        },
                    )
                    .await?
            }
            FsKind::Grep => {
                self.exec
                    .grep(
                        &self.handle,
                        GrepRequest {
                            pattern: arg_str(&arguments, "pattern"),
                            path: arguments
                                .get("path")
                                .and_then(|v| v.as_str())
                                .map(str::to_owned),
                            include: arguments
                                .get("include")
                                .and_then(|v| v.as_str())
                                .map(str::to_owned),
                            max_results: arguments.get("max_results").and_then(|v| v.as_u64()),
                        },
                    )
                    .await?
            }
        };
        Ok(if t.is_error {
            ToolOutput::err(t.content)
        } else {
            ToolOutput::ok(t.content)
        })
    }

    async fn execute_with_cancel(
        &self,
        arguments: serde_json::Value,
        _cancel: &CancelFlag,
    ) -> anyhow::Result<ToolOutput> {
        self.execute(arguments).await
    }
}

struct ExecBashTool {
    exec: Arc<dyn Executor>,
    handle: WorkspaceHandle,
}

#[async_trait]
impl Tool for ExecBashTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "bash".into(),
            description: "Run a shell command on the remote executor (not the API host)".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "timeout_secs": {"type": "integer"}
                },
                "required": ["command"]
            }),
            prompt_snippet: Some("bash(command): run on remote executor".into()),
        }
    }

    async fn execute(&self, arguments: serde_json::Value) -> anyhow::Result<ToolOutput> {
        let command = arg_str(&arguments, "command");
        if command.is_empty() {
            return Ok(ToolOutput::err("command required"));
        }
        let timeout_secs = arguments.get("timeout_secs").and_then(|v| v.as_u64());
        let r = self
            .exec
            .exec(
                &self.handle,
                ExecRequest {
                    command,
                    timeout_secs,
                },
            )
            .await?;
        let mut s = r.stdout;
        if !r.stderr.is_empty() {
            if !s.is_empty() {
                s.push('\n');
            }
            s.push_str(&r.stderr);
        }
        if r.exit_code != 0 {
            s.push_str(&format!("\n[exit {}]", r.exit_code));
            Ok(ToolOutput::err(s))
        } else {
            Ok(ToolOutput::ok(s))
        }
    }
}

fn arg_str(v: &serde_json::Value, k: &str) -> String {
    v.get(k)
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string()
}
