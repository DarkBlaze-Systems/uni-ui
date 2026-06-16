//! Batch spring integration: scalar baseline vs the `uni-simd` SIMD kernel.
//!
//! A running UI tree animates thousands of properties at once — position, size,
//! opacity, color — each its own damped oscillator sharing a preset's constants.
//! This bench drives a pool of ~10k springs one symplectic-Euler step at a time
//! and pits the scalar reference against the runtime-dispatched SIMD path, so the
//! speedup the SIMD offload buys is a measured number, not a hope.
//!
//! Run with `cargo bench -p uni-spring --features std`.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use uni_spring::{Spring, SpringPool};

const DT: f32 = 1.0 / 240.0;

/// Build a pool of `n` springs, each released from 0 toward a spread of targets,
/// then pre-roll a few steps so it is mid-flight (the realistic, non-trivial
/// state — not everything sitting at rest).
fn primed_pool(n: usize) -> SpringPool {
    let mut pool = SpringPool::filled(Spring::spatial(), n, 0.0);
    for i in 0..n {
        pool.set_target(i, (i as f32) * 0.001 - 5.0);
    }
    for _ in 0..8 {
        pool.step(DT);
    }
    pool
}

fn bench_integrate(c: &mut Criterion) {
    let mut group = c.benchmark_group("spring_batch_step");
    for &n in &[10_000usize] {
        group.throughput(Throughput::Elements(n as u64));

        group.bench_with_input(BenchmarkId::new("scalar", n), &n, |bencher, &n| {
            let mut pool = primed_pool(n);
            bencher.iter(|| pool.step_scalar(black_box(DT)));
        });

        group.bench_with_input(BenchmarkId::new("simd", n), &n, |bencher, &n| {
            let mut pool = primed_pool(n);
            bencher.iter(|| pool.step(black_box(DT)));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_integrate);
criterion_main!(benches);
