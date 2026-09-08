// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Bounded ownership transfer from kernel callback memory.

use std::collections::VecDeque;
use std::time::Instant;

use crate::abi::{RawTrace, TRACE_MAX_SIZE, decode_trace};
use crate::statistics::{DropReason, LossCounters};

/// Queue that copies ephemeral kernel records into bounded owned storage.
#[derive(Debug)]
pub struct BoundedEventReader {
    capacity: usize,
    max_frames: usize,
    queue: VecDeque<Vec<u8>>,
    losses: LossCounters,
}

impl BoundedEventReader {
    /// Creates an event queue with fixed record and frame capacities.
    #[must_use]
    pub fn new(capacity: usize, max_frames: usize) -> Self {
        Self {
            capacity,
            max_frames,
            queue: VecDeque::with_capacity(capacity),
            losses: LossCounters::default(),
        }
    }

    /// Copies one callback record or discards it immediately when bounded state is full.
    pub fn push_raw(&mut self, bytes: &[u8]) -> bool {
        if bytes.len() > TRACE_MAX_SIZE + 7 {
            self.losses.add(DropReason::MalformedRecord, 1);
            return false;
        }
        if self.queue.len() == self.capacity {
            self.losses.add(DropReason::RawQueueFull, 1);
            return false;
        }
        self.queue.push_back(bytes.to_vec());
        true
    }

    /// Decodes at most `maximum` queued events and leaves the rest for a later drain.
    pub fn drain(&mut self, maximum: usize) -> Vec<RawTrace> {
        self.drain_before(maximum, None)
    }

    /// Decodes only within the time budget, leaving unprocessed records owned by the queue.
    pub fn drain_until(&mut self, maximum: usize, deadline: Instant) -> Vec<RawTrace> {
        self.drain_before(maximum, Some(deadline))
    }

    fn drain_before(&mut self, maximum: usize, deadline: Option<Instant>) -> Vec<RawTrace> {
        if deadline.is_some_and(|end| Instant::now() >= end) {
            return Vec::new();
        }
        let count = maximum.min(self.queue.len());
        let mut traces = Vec::with_capacity(count);
        for _ in 0..count {
            if deadline.is_some_and(|end| Instant::now() >= end) {
                break;
            }
            let Some(bytes) = self.queue.pop_front() else {
                break;
            };
            match decode_trace(&bytes, self.max_frames) {
                Ok(trace) => traces.push(trace),
                Err(crate::abi::AbiError::FrameCapacity { .. }) => {
                    self.losses.add(DropReason::TooManyFrames, 1);
                }
                Err(_) => self.losses.add(DropReason::MalformedRecord, 1),
            }
        }
        traces
    }

    /// Returns queued owned records.
    #[must_use]
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Returns whether no record is queued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Returns accumulated event-reader losses.
    #[must_use]
    pub fn losses(&self) -> &LossCounters {
        &self.losses
    }

    /// Removes and returns accumulated losses.
    pub fn take_losses(&mut self) -> LossCounters {
        std::mem::take(&mut self.losses)
    }

    /// Discards all queued records with exact event loss accounting.
    pub fn discard(&mut self, reason: DropReason) {
        self.losses.add(reason, self.queue.len() as u64);
        self.queue.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: A decode deadline expires before queued records can be processed.
    /// Guarantees: Undecoded records remain available for exact shutdown-loss accounting.
    #[test]
    fn decode_deadline_preserves_pending_records() {
        let mut reader = BoundedEventReader::new(2, 4);
        assert!(reader.push_raw(&[0; 8]));
        assert!(reader.push_raw(&[0; 8]));
        assert!(reader.drain_until(2, Instant::now()).is_empty());
        assert_eq!(reader.len(), 2);
        reader.discard(DropReason::ShutdownDeadline);
        assert_eq!(reader.take_losses().get(DropReason::ShutdownDeadline), 2);
    }

    /// Scenario: Kernel callbacks outpace a queue with one record slot.
    /// Guarantees: The second callback is discarded without blocking and queue
    /// memory remains fixed.
    #[test]
    fn full_queue_drops_without_growth() {
        let mut reader = BoundedEventReader::new(1, 4);
        assert!(reader.push_raw(&[0; 8]));
        assert!(!reader.push_raw(&[0; 8]));
        assert_eq!(reader.len(), 1);
        assert_eq!(reader.losses().get(DropReason::RawQueueFull), 1);
    }

    /// Scenario: A malformed record is followed by future drain iterations.
    /// Guarantees: The bad record is removed, counted, and cannot permanently
    /// block queue progress.
    #[test]
    fn malformed_record_is_consumed_and_counted() {
        let mut reader = BoundedEventReader::new(2, 4);
        assert!(reader.push_raw(&[0; 8]));
        assert!(reader.drain(1).is_empty());
        assert!(reader.is_empty());
        assert_eq!(reader.losses().get(DropReason::MalformedRecord), 1);
    }

    /// Scenario: Shutdown expires with two owned records still queued.
    /// Guarantees: Both records are released and counted exactly once.
    #[test]
    fn deadline_discard_counts_records() {
        let mut reader = BoundedEventReader::new(2, 4);
        assert!(reader.push_raw(&[0; 8]));
        assert!(reader.push_raw(&[0; 8]));
        reader.discard(DropReason::ShutdownDeadline);
        reader.discard(DropReason::ShutdownDeadline);
        assert!(reader.is_empty());
        assert_eq!(reader.losses().get(DropReason::ShutdownDeadline), 2);
    }
}
