use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use rupi_benches::source_file;
use rupi_tools::{apply_edits, EditSpec};

fn bench_edit(c: &mut Criterion) {
    let src = source_file(2_000);
    let unique = vec![EditSpec {
        old: "const UNIQUE_ANCHOR: u32 = 42;".into(),
        new: "const UNIQUE_ANCHOR: u32 = 99;".into(),
    }];
    let replace_all = vec![EditSpec {
        old: "let x = ".into(),
        new: "let y = ".into(),
    }];
    let multi = vec![
        EditSpec {
            old: "fn item_0() { let x = 0; }".into(),
            new: "fn item_0() { let x = 1; }".into(),
        },
        EditSpec {
            old: "const UNIQUE_ANCHOR: u32 = 42;".into(),
            new: "const UNIQUE_ANCHOR: u32 = 7;".into(),
        },
    ];

    let mut g = c.benchmark_group("edit");
    g.throughput(Throughput::Bytes(src.len() as u64));
    g.sample_size(40);
    g.bench_function("apply_unique", |b| {
        b.iter(|| apply_edits(&src, &unique, false).unwrap());
    });
    g.bench_function("apply_replace_all", |b| {
        b.iter(|| apply_edits(&src, &replace_all, true).unwrap());
    });
    g.bench_function("apply_multi", |b| {
        b.iter(|| apply_edits(&src, &multi, false).unwrap());
    });
    g.finish();
}

criterion_group!(benches, bench_edit);
criterion_main!(benches);
