use criterion::{criterion_group, criterion_main, Criterion};
use rupi_agent::AgentLoop;
use rupi_core::{Message, Role, SessionTree};
use rupi_llm::MockProvider;
use rupi_memory::{MemoryManager, MemoryStore};

fn padded_session(n: usize) -> SessionTree {
    let mut session = SessionTree::new();
    for i in 0..n {
        session.push(Message::text(
            Role::User,
            format!("turn {i}: {}", "context ".repeat(40)),
        ));
    }
    session
}

fn bench_compaction(c: &mut Criterion) {
    let mut g = c.benchmark_group("compaction");
    g.sample_size(20);

    g.bench_function("history_chars_80", |b| {
        let session = padded_session(80);
        b.iter(|| session.history_chars());
    });

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let home = std::env::temp_dir().join(format!("rupi-bench-mem-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&home);
    g.bench_function("force_compress_mock_80", |b| {
        b.to_async(&rt).iter(|| async {
            let mut session = padded_session(80);
            let agent = AgentLoop::new(3).with_compression(200, 8);
            let provider = MockProvider::new(vec![MockProvider::text_response("SUMMARY")]);
            let mem = MemoryManager::new(MemoryStore::new(home.clone()));
            agent.force_compress(&provider, &mut session, &mem).await;
            session.summary.is_some()
        });
    });
    g.finish();
}

criterion_group!(benches, bench_compaction);
criterion_main!(benches);
