use aft_tokenizer::count_tokens;
use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use std::hint::black_box;

fn bench_128kb_tokenization(c: &mut Criterion) {
    let fixture = include_str!(
        "../../../benchmarks/compression-tokens/fixtures/deploy-container/journalctl.txt"
    );
    let mut input = String::with_capacity(128 * 1024 + fixture.len());
    while input.len() < 128 * 1024 {
        input.push_str(fixture);
    }
    input.truncate(128 * 1024);

    let mut group = c.benchmark_group("claude_tokenizer");
    group.throughput(Throughput::Bytes(input.len() as u64));
    group.bench_function("count_128kb", |b| {
        b.iter(|| count_tokens(black_box(&input)))
    });
    group.finish();
}

criterion_group!(benches, bench_128kb_tokenization);
criterion_main!(benches);
