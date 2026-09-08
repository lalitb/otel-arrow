// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Fixed-cardinality loss and error accounting.

use serde::{Deserialize, Serialize};

/// Fixed reasons for discarded or rejected data.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum DropReason {
    /// Kernel perf buffer reported lost records.
    PerfBufferLost,
    /// Kernel ring-buffer output failed.
    RingBufferLost,
    /// A raw event queue was full.
    RawQueueFull,
    /// A raw record was malformed.
    MalformedRecord,
    /// A trace exceeded its frame bound.
    TooManyFrames,
    /// Process capacity was reached.
    ProcessCapacity,
    /// Mapping capacity was reached.
    MappingCapacity,
    /// Executable capacity was reached.
    ExecutableCapacity,
    /// Native unwind byte capacity was reached.
    UnwindByteCapacity,
    /// Stack-delta capacity was reached.
    StackDeltaCapacity,
    /// User-stack cardinality was reached.
    UserStackCapacity,
    /// Kernel-stack cardinality was reached.
    KernelStackCapacity,
    /// Aggregation-key capacity was reached.
    AggregationCapacity,
    /// Snapshot sample capacity was reached.
    SnapshotSampleCapacity,
    /// Snapshot logical-byte capacity was reached.
    SnapshotByteCapacity,
    /// Pending snapshot queue was full.
    PendingSnapshotCapacity,
    /// Shutdown deadline prevented a drain.
    ShutdownDeadline,
    /// A selected CPU could not be attached in best-effort mode.
    CpuAttachment,
    /// Thread cardinality was reached.
    ThreadCapacity,
    /// Aggregation logical-byte capacity was reached.
    AggregationByteCapacity,
    /// Process metadata was unavailable or could not be synchronized.
    ProcessMetadataUnavailable,
    /// A trace cannot be attributed to the confirmed process generation.
    ProcessGenerationMismatch,
    /// A perf notification selector is not supported by this revision.
    UnknownNotification,
    /// Number of samples in rejected pending generations, not generation count.
    PendingSnapshotSamples,
    /// The capture-window deadline prevented processing already-decoded records.
    CaptureDeadline,
}

impl DropReason {
    const COUNT: usize = 25;

    const fn index(self) -> usize {
        self as usize
    }

    const ALL: [Self; Self::COUNT] = [
        Self::PerfBufferLost,
        Self::RingBufferLost,
        Self::RawQueueFull,
        Self::MalformedRecord,
        Self::TooManyFrames,
        Self::ProcessCapacity,
        Self::MappingCapacity,
        Self::ExecutableCapacity,
        Self::UnwindByteCapacity,
        Self::StackDeltaCapacity,
        Self::UserStackCapacity,
        Self::KernelStackCapacity,
        Self::AggregationCapacity,
        Self::SnapshotSampleCapacity,
        Self::SnapshotByteCapacity,
        Self::PendingSnapshotCapacity,
        Self::ShutdownDeadline,
        Self::CpuAttachment,
        Self::ThreadCapacity,
        Self::AggregationByteCapacity,
        Self::ProcessMetadataUnavailable,
        Self::ProcessGenerationMismatch,
        Self::UnknownNotification,
        Self::PendingSnapshotSamples,
        Self::CaptureDeadline,
    ];
}

/// Fixed-size counters indexed by [`DropReason`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LossCounters {
    counts: [u64; DropReason::COUNT],
}

impl Default for LossCounters {
    fn default() -> Self {
        Self {
            counts: [0; DropReason::COUNT],
        }
    }
}

impl LossCounters {
    /// Adds a count using saturating arithmetic.
    pub fn add(&mut self, reason: DropReason, count: u64) {
        let slot = &mut self.counts[reason.index()];
        *slot = slot.saturating_add(count);
    }

    /// Returns the current count for one reason.
    #[must_use]
    pub fn get(&self, reason: DropReason) -> u64 {
        self.counts[reason.index()]
    }

    /// Returns deterministic non-zero reason/count pairs.
    #[must_use]
    pub fn non_zero(&self) -> Vec<(DropReason, u64)> {
        DropReason::ALL
            .into_iter()
            .filter_map(|reason| {
                let count = self.get(reason);
                (count != 0).then_some((reason, count))
            })
            .collect()
    }

    /// Merges counters using saturating arithmetic.
    pub fn merge(&mut self, other: &Self) {
        for reason in DropReason::ALL {
            self.add(reason, other.get(reason));
        }
    }
}

/// A bounded set of detailed diagnostic messages.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ErrorDetails {
    maximum: usize,
    details: Vec<String>,
    rejected: u64,
}

impl ErrorDetails {
    /// Creates a detail store with a fixed message count.
    #[must_use]
    pub fn new(maximum: usize) -> Self {
        Self {
            maximum,
            details: Vec::with_capacity(maximum),
            rejected: 0,
        }
    }

    /// Stores a bounded message or counts its rejection.
    pub fn push(&mut self, mut detail: String) {
        const MAX_DETAIL_BYTES: usize = 512;
        crate::error::truncate_detail(&mut detail, MAX_DETAIL_BYTES);
        if self.details.len() < self.maximum {
            self.details.push(detail);
        } else {
            self.rejected = self.rejected.saturating_add(1);
        }
    }

    /// Returns stored details.
    #[must_use]
    pub fn details(&self) -> &[String] {
        &self.details
    }

    /// Returns the number of details rejected after capacity was reached.
    #[must_use]
    pub fn rejected(&self) -> u64 {
        self.rejected
    }
}

/// Owned statistics included in each profile snapshot.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SnapshotStatistics {
    /// Count of raw records accepted from the kernel.
    pub raw_events: u64,
    /// Count of decoded samples accepted into aggregation.
    pub accepted_samples: u64,
    /// Fixed-cardinality loss counters.
    pub losses: Vec<(DropReason, u64)>,
    /// Shutdown expired before buffers were proven empty or the window fully finalized.
    ///
    /// The number of records still in the closed kernel buffers is unknown;
    /// losses for already-owned userspace records remain exact.
    pub shutdown_incomplete: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: Loss counts approach integer overflow during a long-running process.
    /// Guarantees: Accounting saturates instead of wrapping and never reports a
    /// smaller cumulative loss after another event.
    #[test]
    fn loss_counters_saturate() {
        let mut counters = LossCounters::default();
        counters.add(DropReason::RawQueueFull, u64::MAX);
        counters.add(DropReason::RawQueueFull, 1);
        assert_eq!(counters.get(DropReason::RawQueueFull), u64::MAX);
    }

    /// Scenario: More diagnostic strings arrive than configured capacity.
    /// Guarantees: Stored memory remains bounded while rejected detail is
    /// represented by a counter.
    #[test]
    fn error_details_stop_at_capacity() {
        let mut details = ErrorDetails::new(1);
        details.push("first".to_owned());
        details.push("second".to_owned());
        assert_eq!(details.details(), ["first"]);
        assert_eq!(details.rejected(), 1);
    }

    /// Scenario: A non-ASCII process path crosses the stored diagnostic byte boundary.
    /// Guarantees: Bounded diagnostic storage preserves valid UTF-8 without panicking.
    #[test]
    fn error_details_accept_unicode_paths() {
        let mut details = ErrorDetails::new(1);
        details.push("\u{20ac}".repeat(200));
        assert_eq!(details.details()[0].len(), 510);
        assert!(details.details()[0].capacity() <= 512);
    }
}
