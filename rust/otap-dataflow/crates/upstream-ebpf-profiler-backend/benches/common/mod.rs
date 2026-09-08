// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

use otel_arrow_dfe_upstream_ebpf_profiler_backend::{
    BoundedAggregator, ResourceLimits,
    abi::{DecodedFrame, FrameFlags, FrameKind, RawTrace},
    process::ProcessIdentity,
};

pub fn populated_window(keys: usize) -> BoundedAggregator {
    let mut aggregator = BoundedAggregator::new(ResourceLimits::default());
    for index in 0..keys {
        let trace = RawTrace {
            pid: 1,
            tid: 1,
            ktime_ns: index as u64,
            comm: b"bench".to_vec(),
            apm_transaction_id: [0; 8],
            apm_trace_id: [0; 16],
            custom_labels: Vec::new(),
            origin: 1,
            value: 1,
            cpu_id: 0,
            kernel_frames: Vec::new(),
            user_frames: (0..32)
                .map(|frame| DecodedFrame {
                    kind: FrameKind::Native,
                    flags: FrameFlags::default(),
                    data: (index * 32 + frame) as u64,
                    variables: vec![0xfeed],
                })
                .collect(),
        };
        aggregator
            .record(
                trace,
                ProcessIdentity {
                    pid: 1,
                    start_time_ticks: 1,
                    executable: Some("/bench".to_owned()),
                },
            )
            .expect("benchmark capacity");
    }
    aggregator
}
