//! MCP client: JSON-RPC 2.0 over stdio (newline-delimited) and streamable HTTP.

mod client;
mod config;
mod protocol;
mod tools;

pub use client::{McpClient, McpManager, McpServerHandle};
pub use config::{load_mcp_config, merge_mcp_files, McpServerConfig, McpServersFile};
pub use protocol::{JsonRpcError, JsonRpcId, JsonRpcRequest, JsonRpcResponse};
pub use tools::mcp_tools_from_manager;
