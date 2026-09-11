use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use rupi_benches::{chunked, sse_stream};
use rupi_llm::SseParser;

fn bench_sse(c: &mut Criterion) {
    let bytes = sse_stream(8_000, r#"{"choices":[{"delta":{"content":"hello"}}]}"#);
    let mut g = c.benchmark_group("sse");
    g.throughput(Throughput::Bytes(bytes.len() as u64));
    g.sample_size(40);
    g.bench_function("push_bytes_8k_events_4kib_chunks", |b| {
        b.iter(|| {
            let mut p = SseParser::default();
            let mut n = 0usize;
            for chunk in chunked(&bytes, 4096) {
                n += p.push_bytes(chunk).len();
            }
            n += p.finish().is_some() as usize;
            n
        });
    });
    g.finish();
}

criterion_group!(benches, bench_sse);
criterion_main!(benches);
