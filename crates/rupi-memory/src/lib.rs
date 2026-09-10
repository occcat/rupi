//! Hermes-style persistent memory: MEMORY.md / USER.md, memory tool, FTS session recall.

mod fts;
mod scan;
mod store;
mod tool;

pub use fts::SessionIndex;
pub use store::{MemoryStore, MemoryTarget, MEMORY_CHAR_LIMIT, USER_CHAR_LIMIT};
pub use tool::{MemoryTool, SessionSearchTool};
