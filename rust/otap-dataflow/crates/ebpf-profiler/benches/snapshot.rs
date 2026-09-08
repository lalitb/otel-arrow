// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Bounded finalization, ownership handoff, window reset, and queue pressure.

mod support;

use std::{
    sync::mpsc::{TrySendError, sync_channel},
    time::{Duration, SystemTime},
};

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use otel_arrow_dfe_ebpf_profiler::ProfileSnapshot;

fn snapshot() -> ProfileSnapshot {
    support::populated(1000)
        .finish(SystemTime::UNIX_EPOCH + Duration::from_secs(1))
        .expect("snapshot is valid")
}

fn bench_snapshot(criterion: &mut Criterion) {
    let _finalize = criterion.bench_function("finalize_1000_samples", |bencher| {
        bencher.iter_batched(
            || support::populated(1000),
            |window| {
                let snapshot = window
                    .finish(SystemTime::UNIX_EPOCH + Duration::from_secs(1))
                    .expect("snapshot finalizes successfully");
                assert_eq!(snapshot.samples.len(), 1000);
                std::hint::black_box(snapshot)
            },
            BatchSize::SmallInput,
        );
    });
    let _handoff = criterion.bench_function("snapshot_ownership_handoff", |bencher| {
        let (sender, receiver) = sync_channel(1);
        bencher.iter_batched(
            snapshot,
            |snapshot| {
                sender
                    .try_send(snapshot)
                    .expect("one queue slot is available");
                std::hint::black_box(receiver.try_recv().expect("ownership transfers"))
            },
            BatchSize::SmallInput,
        );
    });
    let _reset = criterion.bench_function("empty_window_reset", |bencher| {
        bencher.iter(|| std::hint::black_box(support::window()));
    });
    let _pressure = criterion.bench_function("slow_consumer_bounded_rejection", |bencher| {
        let (sender, _receiver) = sync_channel(1);
        sender
            .try_send(snapshot())
            .expect("initial queue slot is available");
        bencher.iter_batched(
            snapshot,
            |snapshot| {
                assert!(matches!(
                    sender.try_send(snapshot),
                    Err(TrySendError::Full(_))
                ));
            },
            BatchSize::SmallInput,
        );
    });
}

criterion_group!(benches, bench_snapshot);
criterion_main!(benches);
