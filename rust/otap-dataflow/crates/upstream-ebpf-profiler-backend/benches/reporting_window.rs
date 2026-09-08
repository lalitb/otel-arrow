// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Benchmarks non-empty reporting-window finalization and owned snapshot handoff.

mod common;
use criterion::{Criterion, criterion_group, criterion_main};
use otel_arrow_dfe_upstream_ebpf_profiler_backend::snapshot::PendingSnapshots;

fn reporting_window(criterion: &mut Criterion) {
    let _benchmark = criterion.bench_function("finalize_256_stacks", |bencher| {
        bencher.iter_batched(
            || common::populated_window(256),
            |mut aggregator| aggregator.finalize(),
            criterion::BatchSize::LargeInput,
        )
    });
    let _benchmark = criterion.bench_function("snapshot_owned_handoff", |bencher| {
        bencher.iter_batched(
            || common::populated_window(256).finalize(),
            |snapshot| {
                let mut pending = PendingSnapshots::new(2);
                pending.try_push(snapshot).expect("capacity");
                pending.pop()
            },
            criterion::BatchSize::LargeInput,
        )
    });
}

criterion_group!(benches, reporting_window);
criterion_main!(benches);
