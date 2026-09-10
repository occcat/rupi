//! Pi-compatible coding agent harness.

pub mod config;
pub mod context_files;
pub mod harness;
pub mod tools;

pub use config::{AgentSettings, ConfigPaths};
pub use harness::{Harness, HarnessOptions, PrintOutcome};
pub use tools::{builtin_tool_names, create_coding_tools};

pub const UPSTREAM: &str = "@earendil-works/pi-coding-agent@0.85.1";
