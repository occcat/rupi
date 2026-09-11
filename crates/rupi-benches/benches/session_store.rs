use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use rupi_memory::SessionStore;

fn scratch(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "rupi-bench-ss-{tag}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn bench_session_store(c: &mut Criterion) {
    let mut g = c.benchmark_group("session_store");
    g.sample_size(20);

    g.bench_function("open_new", |b| {
        b.iter_batched(
            || scratch("open"),
            |dir| {
                let _ = SessionStore::open(&dir).unwrap();
                let _ = std::fs::remove_dir_all(&dir);
            },
            BatchSize::SmallInput,
        );
    });

    let existing = scratch("exist");
    let _ = SessionStore::open(&existing).unwrap();
    g.bench_function("open_existing", |b| {
        b.iter(|| SessionStore::open(&existing).unwrap());
    });

    g.bench_function("add_message_full_100", |b| {
        b.iter_batched(
            || {
                let dir = scratch("add");
                let store = SessionStore::open(&dir).unwrap();
                let sid = store.create_session("bench").unwrap();
                (dir, store, sid)
            },
            |(dir, store, sid)| {
                for i in 0..100 {
                    store
                        .add_message_full(
                            &uuid::Uuid::new_v4().to_string(),
                            &sid,
                            "user",
                            &format!("message {i} about tea and rust"),
                            None,
                        )
                        .unwrap();
                }
                let _ = std::fs::remove_dir_all(dir);
            },
            BatchSize::SmallInput,
        );
    });

    let filled = scratch("filled");
    let store = SessionStore::open(&filled).unwrap();
    let sid = store.create_session("bench").unwrap();
    for i in 0..200 {
        store
            .add_message(&sid, "user", &format!("row {i} searchable tea rust"))
            .unwrap();
    }
    g.bench_function("session_records_200", |b| {
        b.iter(|| store.session_records(&sid, 500).unwrap());
    });
    g.bench_function("fts_search_tea", |b| {
        b.iter(|| store.search("tea", 10).unwrap());
    });
    g.finish();
}

criterion_group!(benches, bench_session_store);
criterion_main!(benches);
