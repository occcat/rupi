//! MCP 2025-03-26 client: JSON-RPC, stdio, streamable HTTP, tool bridge.

mod bridge;
mod client;
mod protocol;
mod stdio;
mod http;

pub use bridge::{McpToolBridge, PrefixedTool};
pub use client::{
    load_mcp_configs, InMemoryTransport, McpClient, McpManager, ServerConfig, Transport, TransportKind,
};
pub use protocol::McpError;
pub use protocol::{
    CallToolResult, JsonRpcError, JsonRpcId, JsonRpcRequest, JsonRpcResponse, ToolAnnotation,
    ToolSpec, PROTOCOL_FALLBACK, PROTOCOL_VERSION,
};
pub use stdio::StdioTransport;
pub use http::HttpTransport;
