// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Event ABI decoding microbenchmarks.

use criterion::{Criterion, criterion_group, criterion_main};
use otel_arrow_dfe_ebpf_profiler::{
    EventFlags, MAX_ABI_STACK_DEPTH, RawSample, decode_event, encode_event,
};

fn bench_event_decode(criterion: &mut Criterion) {
    let mut user_frames = [0; MAX_ABI_STACK_DEPTH];
    for (index, frame) in user_frames.iter_mut().take(32).enumerate() {
        *frame = 0x1000 + (index as u64 * 16);
    }
    let event = encode_event(&RawSample {
        pid: 1,
        tid: 1,
        cpu: 0,
        timestamp_ns: 1,
        flags: EventFlags::empty(),
        user_stack_error: 0,
        kernel_stack_error: 0,
        user_depth: 32,
        kernel_depth: 0,
        user_frames,
        kernel_frames: [0; MAX_ABI_STACK_DEPTH],
    });
    let _benchmark = criterion.bench_function("decode_32_frame_event", |bencher| {
        bencher.iter(|| decode_event(std::hint::black_box(&event)));
    });
}

criterion_group!(benches, bench_event_decode);
criterion_main!(benches);
