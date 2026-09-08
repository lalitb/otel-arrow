// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Benchmarks bounded repeated-stack aggregation.

use criterion::{Criterion, criterion_group, criterion_main};
use otel_arrow_dfe_upstream_ebpf_profiler_backend::abi::{
    DecodedFrame, FrameFlags, FrameKind, RawTrace,
};
use otel_arrow_dfe_upstream_ebpf_profiler_backend::process::ProcessIdentity;
use otel_arrow_dfe_upstream_ebpf_profiler_backend::unwind::{
    ExecutableLayout, ExecutableSegment, FramePointerUnwindPlan,
};
use otel_arrow_dfe_upstream_ebpf_profiler_backend::{BoundedAggregator, ResourceLimits};

fn aggregation(criterion: &mut Criterion) {
    let trace = RawTrace {
        pid: 1,
        tid: 1,
        ktime_ns: 1,
        comm: b"bench".to_vec(),
        apm_transaction_id: [0; 8],
        apm_trace_id: [0; 16],
        custom_labels: Vec::new(),
        origin: 1,
        value: 1,
        cpu_id: 0,
        kernel_frames: Vec::new(),
        user_frames: vec![DecodedFrame {
            kind: FrameKind::Native,
            flags: FrameFlags::default(),
            data: 0x1234,
            variables: vec![0xfeed],
        }],
    };
    let process = ProcessIdentity {
        pid: 1,
        start_time_ticks: 1,
        executable: Some("/bench".to_owned()),
    };
    let _benchmark = criterion.bench_function("aggregate_repeated_stack", |bencher| {
        bencher.iter_batched(
            || BoundedAggregator::new(ResourceLimits::default()),
            |mut aggregator| {
                for _ in 0..1024 {
                    aggregator
                        .record(trace.clone(), process.clone())
                        .expect("benchmark limits");
                }
                aggregator.finalize()
            },
            criterion::BatchSize::SmallInput,
        )
    });
}

fn metadata(criterion: &mut Criterion) {
    let layout = ExecutableLayout {
        segments: (0..64)
            .map(|index| ExecutableSegment {
                file_offset: index * 8192,
                file_size: 4096,
                virtual_address: 0x400000 + index * 8192,
                memory_size: 4096,
                executable: true,
            })
            .collect(),
    };
    let _benchmark = criterion.bench_function("executable_mapping_lookup", |bencher| {
        bencher.iter(|| {
            layout.virtual_address_for_file_offset(std::hint::black_box(63 * 8192 + 32), 4096)
        })
    });
    let _benchmark = criterion.bench_function("encode_native_unwind_metadata", |bencher| {
        bencher.iter(|| {
            FramePointerUnwindPlan::from_layout(std::hint::black_box(&layout), 256, 64, 64 * 1024)
                .expect("benchmark metadata capacity")
        })
    });
}

criterion_group!(benches, aggregation, metadata);
criterion_main!(benches);
