// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Owned transport-neutral profile snapshots.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::abi::{FrameFlags, FrameKind};
use crate::process::ProcessIdentity;
use crate::statistics::{DropReason, LossCounters, SnapshotStatistics};

/// One owned frame in a profile snapshot.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct ProfileFrame {
    /// Frame marker supplied by the kernel.
    pub kind: FrameKind,
    /// Upstream frame flags.
    pub flags: FrameFlags,
    /// Executable ID for native frames when present.
    pub executable_id: Option<u64>,
    /// Relative address, source line, or error value from the frame header.
    pub address_or_line: u64,
    /// Additional type-specific words retained for forward compatibility.
    pub variables: Vec<u64>,
}

/// One aggregated sample with fully owned state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProfileSample {
    /// PID plus start-time identity that protects against PID reuse.
    pub process: ProcessIdentity,
    /// Thread ID observed by the kernel.
    pub tid: u32,
    /// Task command bytes.
    pub comm: Vec<u8>,
    /// Ordered kernel instruction pointers.
    pub kernel_frames: Vec<u64>,
    /// Ordered checked user frames.
    pub user_frames: Vec<ProfileFrame>,
    /// Sorted custom label bytes.
    pub labels: Vec<(Vec<u8>, Vec<u8>)>,
    /// Number of matching samples.
    pub count: u64,
    /// Saturating sum of probe-defined values.
    pub value_sum: u64,
    /// Earliest kernel timestamp in the aggregate.
    pub first_ktime_ns: u64,
    /// Latest kernel timestamp in the aggregate.
    pub last_ktime_ns: u64,
}

/// A completed profile window with no loader- or transport-specific handles.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProfileSnapshot {
    /// Monotonic userspace generation number.
    pub sequence: u64,
    /// Earliest kernel timestamp represented in this snapshot.
    pub start_ktime_ns: u64,
    /// Latest kernel timestamp represented in this snapshot.
    pub end_ktime_ns: u64,
    /// Deterministically ordered aggregated samples.
    pub samples: Vec<ProfileSample>,
    /// Estimated owned logical bytes.
    pub logical_bytes: usize,
    /// Accepted and discarded event statistics.
    pub statistics: SnapshotStatistics,
}

/// Bounded handoff queue for completed snapshots.
#[derive(Debug)]
pub struct PendingSnapshots {
    capacity: usize,
    queue: VecDeque<ProfileSnapshot>,
    losses: LossCounters,
}

impl PendingSnapshots {
    /// Creates a queue that retains at most `capacity` generations.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: VecDeque::with_capacity(capacity),
            losses: LossCounters::default(),
        }
    }

    /// Enqueues a snapshot, rejecting the new generation when the consumer is full.
    pub fn try_push(&mut self, snapshot: ProfileSnapshot) -> Result<(), ProfileSnapshot> {
        if self.queue.len() == self.capacity {
            self.losses.add(DropReason::PendingSnapshotCapacity, 1);
            self.losses.add(
                DropReason::PendingSnapshotSamples,
                snapshot
                    .samples
                    .iter()
                    .fold(0_u64, |count, sample| count.saturating_add(sample.count)),
            );
            for (reason, count) in &snapshot.statistics.losses {
                self.losses.add(*reason, *count);
            }
            return Err(snapshot);
        }
        self.queue.push_back(snapshot);
        Ok(())
    }

    /// Removes the oldest completed generation.
    pub fn pop(&mut self) -> Option<ProfileSnapshot> {
        self.queue.pop_front()
    }

    /// Returns the number of pending generations.
    #[must_use]
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Returns whether no generation is pending.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Returns handoff loss counters.
    #[must_use]
    pub fn losses(&self) -> &LossCounters {
        &self.losses
    }

    /// Removes and returns handoff loss counters.
    pub fn take_losses(&mut self) -> LossCounters {
        std::mem::take(&mut self.losses)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(sequence: u64) -> ProfileSnapshot {
        ProfileSnapshot {
            sequence,
            start_ktime_ns: 0,
            end_ktime_ns: 0,
            samples: Vec::new(),
            logical_bytes: 0,
            statistics: SnapshotStatistics::default(),
        }
    }

    /// Scenario: A reporting interval fires while the only pending generation
    /// has not been consumed.
    /// Guarantees: The older generation is preserved, the newer one is
    /// deterministically rejected, and a fixed loss reason is incremented.
    #[test]
    fn full_handoff_rejects_new_generation() {
        let mut pending = PendingSnapshots::new(1);
        pending.try_push(snapshot(1)).expect("first fits");
        let rejected = pending.try_push(snapshot(2)).expect_err("second is full");
        assert_eq!(rejected.sequence, 2);
        assert_eq!(pending.pop().expect("old generation").sequence, 1);
        assert_eq!(pending.losses().get(DropReason::PendingSnapshotCapacity), 1);
    }

    /// Scenario: A full consumer queue rejects a generation containing prior kernel losses and seven samples.
    /// Guarantees: Dropped generations do not erase kernel-loss evidence or sample multiplicity.
    #[test]
    fn rejected_generation_preserves_loss_evidence() {
        let mut pending = PendingSnapshots::new(1);
        pending.try_push(snapshot(1)).expect("first generation");
        let mut next = snapshot(2);
        next.statistics.losses.push((DropReason::RingBufferLost, 3));
        next.samples.push(ProfileSample {
            process: ProcessIdentity {
                pid: 1,
                start_time_ticks: 1,
                executable: None,
            },
            tid: 1,
            comm: Vec::new(),
            kernel_frames: Vec::new(),
            user_frames: Vec::new(),
            labels: Vec::new(),
            count: 7,
            value_sum: 7,
            first_ktime_ns: 1,
            last_ktime_ns: 7,
        });
        let _rejected = pending.try_push(next).expect_err("queue full");
        assert_eq!(pending.losses().get(DropReason::PendingSnapshotSamples), 7);
        assert_eq!(pending.losses().get(DropReason::RingBufferLost), 3);
        assert_eq!(
            pending
                .take_losses()
                .get(DropReason::PendingSnapshotCapacity),
            1
        );
        assert!(pending.losses().non_zero().is_empty());
    }
}
