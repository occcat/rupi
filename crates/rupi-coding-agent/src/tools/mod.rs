mod bash;
mod edit;
mod find;
mod grep;
mod ls;
mod read;
mod write;

use std::path::PathBuf;
use std::sync::Arc;

use rupi_agent_core::{PathGuard, ToolSet};

pub use bash::BashTool;
pub use edit::EditTool;
pub use find::FindTool;
pub use grep::GrepTool;
pub use ls::LsTool;
pub use read::ReadTool;
pub use write::WriteTool;

pub fn builtin_tool_names() -> &'static [&'static str] {
    &["read", "bash", "edit", "write", "grep", "find", "ls"]
}

pub fn create_coding_tools(cwd: PathBuf, names: &[String], sandbox: bool) -> ToolSet {
    let guard = PathGuard {
        cwd: cwd.clone(),
        allow_outside: !sandbox,
    };
    let mut set = ToolSet::new();
    let want = |n: &str| names.is_empty() || names.iter().any(|x| x == n);
    if want("read") {
        set.register(Arc::new(ReadTool {
            cwd: cwd.clone(),
            guard: guard.clone(),
        }));
    }
    if want("write") {
        set.register(Arc::new(WriteTool {
            cwd: cwd.clone(),
            guard: guard.clone(),
        }));
    }
    if want("edit") {
        set.register(Arc::new(EditTool {
            cwd: cwd.clone(),
            guard: guard.clone(),
        }));
    }
    if want("bash") {
        set.register(Arc::new(BashTool {
            cwd: cwd.clone(),
            timeout_ms: 60_000,
        }));
    }
    if want("grep") {
        set.register(Arc::new(GrepTool {
            cwd: cwd.clone(),
            guard: guard.clone(),
        }));
    }
    if want("find") {
        set.register(Arc::new(FindTool {
            cwd: cwd.clone(),
            guard: guard.clone(),
        }));
    }
    if want("ls") {
        set.register(Arc::new(LsTool {
            cwd: cwd.clone(),
            guard: guard.clone(),
        }));
    }
    set
}
