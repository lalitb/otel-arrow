// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Bounded, round-robin Aya perf-buffer event source.

use std::{
    ops::ControlFlow,
    time::{Duration, Instant},
};

use aya::maps::{
    MapData,
    perf::{PerfEvent, PerfEventArrayBuffer},
};

use crate::{
    ABI_EVENT_SIZE, EventBatch, EventSource, ProfilerError, RawSample, Result, decode_event,
};

pub(crate) struct AyaEventSource {
    buffers: Vec<PerfEventArrayBuffer<MapData>>,
    next_buffer: usize,
}

impl AyaEventSource {
    pub(crate) fn new(buffers: Vec<PerfEventArrayBuffer<MapData>>) -> Self {
        Self {
            buffers,
            next_buffer: 0,
        }
    }
}

impl std::fmt::Debug for AyaEventSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AyaEventSource")
            .field("buffers", &self.buffers.len())
            .finish()
    }
}

trait Buffer {
    fn readable(&self) -> bool;
    fn drain(&mut self, max_records: usize, batch: &mut EventBatch) -> usize;
}

impl Buffer for PerfEventArrayBuffer<MapData> {
    fn readable(&self) -> bool {
        Self::readable(self)
    }

    fn drain(&mut self, max_records: usize, batch: &mut EventBatch) -> usize {
        let flow = self.try_fold(0, |count, event| {
            consume(event, batch);
            let count = count + 1;
            if count == max_records {
                ControlFlow::Break(count)
            } else {
                ControlFlow::Continue(count)
            }
        });
        match flow {
            ControlFlow::Break(count) | ControlFlow::Continue(count) => count,
        }
    }
}

fn consume(event: PerfEvent<'_>, batch: &mut EventBatch) {
    match event {
        PerfEvent::Sample { head, tail } => match decode_perf_sample(head, tail) {
            Ok(sample) => batch.samples.push(sample),
            Err(_) => batch.malformed_events = batch.malformed_events.saturating_add(1),
        },
        PerfEvent::Lost { count } => batch.lost_events = batch.lost_events.saturating_add(count),
    }
}

fn decode_perf_sample(head: &[u8], tail: &[u8]) -> Result<RawSample> {
    let length = head
        .len()
        .checked_add(tail.len())
        .ok_or_else(|| ProfilerError::AbiMismatch("perf payload length overflow".to_owned()))?;
    if !(ABI_EVENT_SIZE..=ABI_EVENT_SIZE + 7).contains(&length) {
        return Err(ProfilerError::AbiMismatch(
            "invalid perf payload length".to_owned(),
        ));
    }
    // Aya includes up to seven bytes of transport padding, not part of the ABI.
    if head.len() >= ABI_EVENT_SIZE {
        return decode_event(&head[..ABI_EVENT_SIZE]);
    }
    let mut bytes = [0_u8; ABI_EVENT_SIZE];
    bytes[..head.len()].copy_from_slice(head);
    bytes[head.len()..].copy_from_slice(&tail[..ABI_EVENT_SIZE - head.len()]);
    decode_event(&bytes)
}

fn drain_round_robin(
    buffers: &mut [impl Buffer],
    next: &mut usize,
    max_records: usize,
    batch: &mut EventBatch,
) {
    if buffers.is_empty() {
        return;
    }
    let quota = max_records.div_ceil(buffers.len());
    let mut remaining = max_records;
    for _ in 0..buffers.len() {
        let index = *next;
        *next = (*next + 1) % buffers.len();
        remaining -= buffers[index].drain(quota.min(remaining), batch);
        if remaining == 0 {
            break;
        }
    }
    batch.has_more = buffers.iter().any(Buffer::readable);
}

impl EventSource for AyaEventSource {
    fn read_batch(
        &mut self,
        timeout: Duration,
        max_samples: usize,
        batch: &mut EventBatch,
    ) -> Result<()> {
        batch.clear();
        if max_samples == 0 {
            return Err(ProfilerError::invalid("max_samples", "must be non-zero"));
        }
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        while !self.buffers.iter().any(Buffer::readable) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            std::thread::sleep(Duration::from_millis(1).min(remaining));
        }
        drain_round_robin(&mut self.buffers, &mut self.next_buffer, max_samples, batch);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;
    use crate::{EventFlags, MAX_ABI_STACK_DEPTH, encode_event};

    fn event() -> Vec<u8> {
        encode_event(&RawSample {
            pid: 3,
            tid: 4,
            cpu: 5,
            timestamp_ns: 9,
            flags: EventFlags::empty(),
            user_stack_error: 0,
            kernel_stack_error: 0,
            user_depth: 0,
            kernel_depth: 0,
            user_frames: [0; MAX_ABI_STACK_DEPTH],
            kernel_frames: [0; MAX_ABI_STACK_DEPTH],
        })
    }

    /// Scenario: Perf payloads include alignment padding at every possible ring split.
    /// Guarantees: Wrapped and contiguous records decode identically with no overread.
    #[test]
    fn padding_and_all_wrapped_splits_decode() {
        let raw = event();
        let expected = decode_event(&raw).expect("fixture");
        for padding in 0..=7 {
            let mut bytes = raw.clone();
            bytes.extend(std::iter::repeat_n(0xa5, padding));
            for split in 0..=bytes.len() {
                assert_eq!(
                    decode_perf_sample(&bytes[..split], &bytes[split..]).expect("perf event"),
                    expected
                );
            }
        }
        assert!(decode_perf_sample(&raw[..raw.len() - 1], &[]).is_err());
        assert!(decode_perf_sample(&raw, &[0; 8]).is_err());
        assert!(decode_event(&[raw.as_slice(), &[0; 4]].concat()).is_err());
    }

    struct FakeBuffer(VecDeque<Vec<u8>>);

    impl Buffer for FakeBuffer {
        fn readable(&self) -> bool {
            !self.0.is_empty()
        }
        fn drain(&mut self, max_records: usize, batch: &mut EventBatch) -> usize {
            let count = max_records.min(self.0.len());
            for _ in 0..count {
                let bytes = self.0.pop_front().expect("buffered record");
                consume(
                    PerfEvent::Sample {
                        head: &bytes,
                        tail: &[],
                    },
                    batch,
                );
            }
            count
        }
    }

    /// Scenario: A busy malformed CPU ring competes with a valid ring at budget one.
    /// Guarantees: Malformed records consume budget, a later CPU is not starved,
    /// and unread events remain buffered instead of becoming capacity drops.
    #[test]
    fn record_budget_and_cpu_fairness() {
        let mut buffers = [
            FakeBuffer(vec![vec![0]; 5].into()),
            FakeBuffer(vec![event(); 5].into()),
        ];
        let mut cursor = 0;
        let mut batch = EventBatch::default();
        drain_round_robin(&mut buffers, &mut cursor, 1, &mut batch);
        assert_eq!(batch.malformed_events, 1);
        assert!(batch.has_more);
        batch.clear();
        drain_round_robin(&mut buffers, &mut cursor, 1, &mut batch);
        assert_eq!(batch.samples.len(), 1);
        assert_eq!(batch.capacity_drops, 0);
        assert_eq!(buffers[0].0.len(), 4);
        assert_eq!(buffers[1].0.len(), 4);
    }
}
