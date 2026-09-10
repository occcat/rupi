//! Pi-compatible coding agent harness.

pub mod config;
pub mod context_files;
pub mod harness;
pub mod mcp;
pub mod tools;

pub use config::{AgentSettings, ConfigPaths};
pub use harness::{Harness, HarnessOptions, PrintOutcome, ReplOutcome};
pub use tools::{builtin_tool_names, create_coding_tools, create_read_only_tools};

pub const UPSTREAM: &str = "@earendil-works/pi-coding-agent@0.85.1";
