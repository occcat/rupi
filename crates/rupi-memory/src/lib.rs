//! Hermes-style hierarchical memory.
//!
//! Layers:
//! - Hot: `MEMORY.md` + `USER.md` frozen into the system prompt at session start
//! - Core vs extended: `[core]` prefix always injected; others retrieved via `search`
//! - Evidence: SQLite FTS5 session search (`session_search`)
//! - Procedural: skills (see `rupi-skills`)

mod security;
mod session_search;
mod snapshot;
mod store;
mod tool;

pub use security::scan_memory_entry;
pub use session_search::{SessionSearchHit, SessionSearchIndex};
pub use snapshot::{render_memory_block, MemorySnapshot};
pub use store::{
    MemoryEntry, MemoryLimits, MemoryStore, StoreKind, CORE_PREFIX, ENTRY_DELIM, MEMORY_CHAR_LIMIT,
    USER_CHAR_LIMIT,
};
pub use tool::{memory_tool_definition, MemoryAction, MemoryTool};
