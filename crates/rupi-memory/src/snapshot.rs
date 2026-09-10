use crate::store::{MemoryEntry, MemoryStore, StoreKind};

/// Frozen snapshot captured once at session start (Hermes prefix-cache pattern).
#[derive(Debug, Clone)]
pub struct MemorySnapshot {
    pub memory_core: Vec<MemoryEntry>,
    pub memory_extended: Vec<MemoryEntry>,
    pub user: Vec<MemoryEntry>,
    pub memory_usage: usize,
    pub user_usage: usize,
    pub captured_at: i64,
}

impl MemorySnapshot {
    pub fn capture(store: &MemoryStore) -> Self {
        let mut memory_core = Vec::new();
        let mut memory_extended = Vec::new();
        let has_core = store.entries(StoreKind::Memory).iter().any(|e| e.is_core());
        for e in store.entries(StoreKind::Memory) {
            if !has_core || e.is_core() {
                memory_core.push(e.clone());
            } else {
                memory_extended.push(e.clone());
            }
        }
        Self {
            memory_core,
            memory_extended,
            user: store.entries(StoreKind::User).to_vec(),
            memory_usage: store.usage_chars(StoreKind::Memory),
            user_usage: store.usage_chars(StoreKind::User),
            captured_at: chrono::Utc::now().timestamp_millis(),
        }
    }
}

pub fn render_memory_block(snapshot: &MemorySnapshot) -> String {
    let mut out = String::new();
    out.push_str(&render_store(
        "MEMORY (your personal notes)",
        StoreKind::Memory,
        &snapshot.memory_core,
        snapshot.memory_usage,
        snapshot.memory_extended.len(),
    ));
    out.push('\n');
    out.push_str(&render_store(
        "USER PROFILE",
        StoreKind::User,
        &snapshot.user,
        snapshot.user_usage,
        0,
    ));
    out
}

fn render_store(
    title: &str,
    kind: StoreKind,
    entries: &[MemoryEntry],
    usage: usize,
    extended: usize,
) -> String {
    let limit = kind.limit();
    let pct = if limit == 0 {
        0
    } else {
        (usage * 100) / limit
    };
    let mut header = format!(
        "══════════════════════════════════════════════\n\
         {title} [{pct}% — {usage}/{limit} chars]\n\
         ══════════════════════════════════════════════"
    );
    if extended > 0 {
        header.push_str(&format!(
            "\n(core tier — {extended} extended entries available via search)"
        ));
    }
    let body = entries
        .iter()
        .map(|e| e.display_body())
        .collect::<Vec<_>>()
        .join(&format!("\n{}\n", crate::store::ENTRY_DELIM));
    if body.is_empty() {
        format!("{header}\n(empty)")
    } else {
        format!("{header}\n{body}")
    }
}
