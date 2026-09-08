// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Benchmarks checked upstream event decoding.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use otel_arrow_dfe_upstream_ebpf_profiler_backend::abi::{TRACE_PREFIX_SIZE, decode_trace};

fn event_decode(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("decode_native_trace");
    for frames in [1_u16, 32, 128, 512] {
        let mut bytes = vec![0_u8; TRACE_PREFIX_SIZE + usize::from(frames) * 16];
        bytes[700..702].copy_from_slice(&(frames * 2).to_le_bytes());
        bytes[702..704].copy_from_slice(&frames.to_le_bytes());
        for frame in 0..usize::from(frames) {
            let offset = TRACE_PREFIX_SIZE + frame * 16;
            let header = (3_u64 << 60) | (2_u64 << 52) | frame as u64;
            bytes[offset..offset + 8].copy_from_slice(&header.to_le_bytes());
            bytes[offset + 8..offset + 16].copy_from_slice(&0xfeed_u64.to_le_bytes());
        }
        let _group = group.throughput(Throughput::Elements(u64::from(frames)));
        let _benchmark = group.bench_with_input(
            BenchmarkId::from_parameter(frames),
            &bytes,
            |bencher, bytes| {
                bencher.iter(|| {
                    decode_trace(std::hint::black_box(bytes), 512).expect("valid benchmark trace")
                })
            },
        );
    }
    group.finish();
}

criterion_group!(benches, event_decode);
criterion_main!(benches);
