use rupi_memory::{MemoryStore, SessionIndex};

#[test]
fn fts_indexes_and_searches_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let idx = SessionIndex::open(&dir.path().join("state.db")).unwrap();
    idx.upsert_session("s1", "demo", "/tmp", "2026-01-01").unwrap();
    idx.index_message("s1", "user", "the staging ssh port is 2222", "t0")
        .unwrap();
    idx.index_message("s1", "assistant", "noted, using port 2222", "t1")
        .unwrap();
    let hits = idx.search("2222", 8).unwrap();
    assert!(!hits.is_empty());
    assert!(hits[0].content.contains("2222"));
}

#[test]
fn memory_prompt_block_includes_usage() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::open(dir.path()).unwrap();
    store
        .add(
            rupi_memory::MemoryTarget::User,
            "User prefers concise answers",
        )
        .unwrap();
    let block = store.prompt_block();
    assert!(block.contains("USER PROFILE"));
    assert!(block.contains("concise"));
}
