// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Bounded deterministic sample aggregation.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use crate::abi::{DecodedFrame, FrameKind, RawTrace};
use crate::limits::ResourceLimits;
use crate::process::ProcessIdentity;
use crate::snapshot::{ProfileFrame, ProfileSample, ProfileSnapshot};
use crate::statistics::{DropReason, LossCounters, SnapshotStatistics};

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct AggregationKey {
    process: ProcessIdentity,
    tid: u32,
    comm: Vec<u8>,
    kernel_frames: Vec<u64>,
    user_frames: Vec<ProfileFrame>,
    labels: Vec<(Vec<u8>, Vec<u8>)>,
}

#[derive(Clone, Copy, Debug)]
struct AggregateValue {
    count: u64,
    value_sum: u64,
    first_ktime_ns: u64,
    last_ktime_ns: u64,
}

/// Aggregates raw samples under explicit cardinality and snapshot-size limits.
#[derive(Debug)]
pub struct BoundedAggregator {
    limits: ResourceLimits,
    entries: BTreeMap<AggregationKey, AggregateValue>,
    user_stacks: BTreeSet<Vec<ProfileFrame>>,
    kernel_stacks: BTreeSet<Vec<u64>>,
    threads: BTreeSet<(u32, u64, u32)>,
    logical_bytes: usize,
    sequence: u64,
    raw_events: u64,
    accepted_samples: u64,
    losses: LossCounters,
}

impl BoundedAggregator {
    /// Creates an empty bounded reporting window.
    #[must_use]
    pub fn new(limits: ResourceLimits) -> Self {
        Self {
            entries: BTreeMap::new(),
            user_stacks: BTreeSet::new(),
            kernel_stacks: BTreeSet::new(),
            threads: BTreeSet::new(),
            logical_bytes: 0,
            sequence: 0,
            raw_events: 0,
            accepted_samples: 0,
            losses: LossCounters::default(),
            limits,
        }
    }

    /// Records one checked trace or rejects it with a fixed loss reason.
    pub fn record(&mut self, trace: RawTrace, process: ProcessIdentity) -> Result<(), DropReason> {
        self.raw_events = self.raw_events.saturating_add(1);
        let total_frames = trace
            .kernel_frames
            .len()
            .saturating_add(trace.user_frames.len());
        if total_frames > self.limits.max_frames_per_trace {
            return self.reject(DropReason::TooManyFrames);
        }
        if process.pid != trace.pid {
            return self.reject(DropReason::ProcessGenerationMismatch);
        }
        let frame_words =
            trace
                .user_frames
                .iter()
                .fold(trace.kernel_frames.len(), |count, frame| {
                    count
                        .saturating_add(1)
                        .saturating_add(frame.variables.len())
                });
        if frame_words > crate::abi::MAX_FRAME_WORDS
            || trace
                .user_frames
                .iter()
                .any(|frame| frame.variables.len() > 14)
            || trace.comm.len() > 16
            || trace.custom_labels.len() > crate::abi::MAX_CUSTOM_LABELS
            || trace
                .custom_labels
                .iter()
                .any(|label| label.key.len() > 16 || label.value.len() > 48)
            || process
                .executable
                .as_ref()
                .is_some_and(|path| path.len() > crate::process::MAX_EXECUTABLE_PATH_BYTES)
        {
            return self.reject(DropReason::MalformedRecord);
        }

        let user_frames: Vec<_> = trace.user_frames.iter().map(profile_frame).collect();
        let new_user_stack = !self.user_stacks.contains(&user_frames);
        let new_kernel_stack = !self.kernel_stacks.contains(&trace.kernel_frames);
        let thread = (process.pid, process.start_time_ticks, trace.tid);
        let new_thread = !self.threads.contains(&thread);
        if new_user_stack && self.user_stacks.len() >= self.limits.max_unique_user_stacks {
            return self.reject(DropReason::UserStackCapacity);
        }
        if new_kernel_stack && self.kernel_stacks.len() >= self.limits.max_unique_kernel_stacks {
            return self.reject(DropReason::KernelStackCapacity);
        }
        if new_thread && self.threads.len() >= self.limits.max_threads {
            return self.reject(DropReason::ThreadCapacity);
        }

        let mut labels: Vec<_> = trace
            .custom_labels
            .into_iter()
            .map(|label| (label.key, label.value))
            .collect();
        labels.sort();
        let key = AggregationKey {
            process,
            tid: trace.tid,
            comm: trace.comm,
            kernel_frames: trace.kernel_frames,
            user_frames,
            labels,
        };
        let new_key = !self.entries.contains_key(&key);
        if new_key && self.entries.len() >= self.limits.max_aggregation_keys {
            return self.reject(DropReason::AggregationCapacity);
        }

        let mut additional = if new_key {
            logical_sample_bytes(&key).saturating_add(64)
        } else {
            0
        };
        if new_user_stack {
            additional = additional
                .saturating_add(frame_bytes(&key.user_frames))
                .saturating_add(88);
        }
        if new_kernel_stack {
            additional = additional
                .saturating_add(key.kernel_frames.len().saturating_mul(8))
                .saturating_add(88);
        }
        if new_thread {
            additional = additional.saturating_add(96);
        }
        if additional
            > self
                .limits
                .max_aggregation_logical_bytes
                .saturating_sub(self.logical_bytes)
        {
            return self.reject(DropReason::AggregationByteCapacity);
        }
        self.logical_bytes += additional;
        if new_user_stack {
            let _inserted = self.user_stacks.insert(key.user_frames.clone());
        }
        if new_kernel_stack {
            let _inserted = self.kernel_stacks.insert(key.kernel_frames.clone());
        }
        if new_thread {
            let _inserted = self.threads.insert(thread);
        }
        let _aggregate = self
            .entries
            .entry(key)
            .and_modify(|aggregate| {
                aggregate.count = aggregate.count.saturating_add(1);
                aggregate.value_sum = aggregate.value_sum.saturating_add(trace.value);
                aggregate.first_ktime_ns = aggregate.first_ktime_ns.min(trace.ktime_ns);
                aggregate.last_ktime_ns = aggregate.last_ktime_ns.max(trace.ktime_ns);
            })
            .or_insert(AggregateValue {
                count: 1,
                value_sum: trace.value,
                first_ktime_ns: trace.ktime_ns,
                last_ktime_ns: trace.ktime_ns,
            });
        self.accepted_samples = self.accepted_samples.saturating_add(1);
        Ok(())
    }

    /// Finalizes and resets the current window.
    pub fn finalize(&mut self) -> ProfileSnapshot {
        self.finalize_before(None)
    }

    /// Finalizes only while the drain budget permits, counting all discarded samples.
    ///
    /// Releasing already-owned bounded storage remains mandatory after the
    /// deadline; this is not a hard real-time allocator/scheduler guarantee.
    pub fn finalize_until(&mut self, deadline: Instant) -> ProfileSnapshot {
        self.finalize_before(Some(deadline))
    }

    fn finalize_before(&mut self, deadline: Option<Instant>) -> ProfileSnapshot {
        let capacity = if deadline.is_some_and(|end| Instant::now() >= end) {
            0
        } else {
            self.entries.len().min(self.limits.max_samples_per_snapshot)
        };
        let mut samples = Vec::with_capacity(capacity);
        let mut logical_bytes = 0_usize;
        let mut start = u64::MAX;
        let mut end = 0_u64;
        let mut expired = false;

        for (key, value) in std::mem::take(&mut self.entries) {
            if expired || deadline.is_some_and(|end| Instant::now() >= end) {
                self.losses.add(DropReason::ShutdownDeadline, value.count);
                expired = true;
                continue;
            }
            if samples.len() == self.limits.max_samples_per_snapshot {
                self.losses
                    .add(DropReason::SnapshotSampleCapacity, value.count);
                continue;
            }
            let sample_bytes = logical_sample_bytes(&key);
            if logical_bytes.saturating_add(sample_bytes) > self.limits.max_snapshot_logical_bytes {
                self.losses
                    .add(DropReason::SnapshotByteCapacity, value.count);
                continue;
            }
            logical_bytes += sample_bytes;
            start = start.min(value.first_ktime_ns);
            end = end.max(value.last_ktime_ns);
            samples.push(ProfileSample {
                process: key.process,
                tid: key.tid,
                comm: key.comm,
                kernel_frames: key.kernel_frames,
                user_frames: key.user_frames,
                labels: key.labels,
                count: value.count,
                value_sum: value.value_sum,
                first_ktime_ns: value.first_ktime_ns,
                last_ktime_ns: value.last_ktime_ns,
            });
        }
        self.user_stacks.clear();
        self.kernel_stacks.clear();
        self.threads.clear();
        self.logical_bytes = 0;
        self.sequence = self.sequence.saturating_add(1);
        let statistics = SnapshotStatistics {
            raw_events: std::mem::take(&mut self.raw_events),
            accepted_samples: std::mem::take(&mut self.accepted_samples),
            losses: self.losses.non_zero(),
            shutdown_incomplete: expired,
        };
        self.losses = LossCounters::default();
        ProfileSnapshot {
            sequence: self.sequence,
            start_ktime_ns: if start == u64::MAX { 0 } else { start },
            end_ktime_ns: end,
            samples,
            logical_bytes,
            statistics,
        }
    }

    /// Adds externally observed loss, such as perf-buffer overflow.
    pub fn add_loss(&mut self, reason: DropReason, count: u64) {
        self.losses.add(reason, count);
    }

    fn reject<T>(&mut self, reason: DropReason) -> Result<T, DropReason> {
        self.losses.add(reason, 1);
        Err(reason)
    }
}

fn profile_frame(frame: &DecodedFrame) -> ProfileFrame {
    ProfileFrame {
        kind: frame.kind,
        flags: frame.flags,
        executable_id: (frame.kind == FrameKind::Native)
            .then(|| frame.variables.first().copied())
            .flatten(),
        address_or_line: frame.data,
        variables: frame.variables.clone(),
    }
}

fn logical_sample_bytes(key: &AggregationKey) -> usize {
    size_of::<ProfileSample>()
        .saturating_add(key.process.executable.as_ref().map_or(0, String::len))
        .saturating_add(key.comm.len())
        .saturating_add(key.kernel_frames.len().saturating_mul(8))
        .saturating_add(frame_bytes(&key.user_frames))
        .saturating_add(
            key.labels
                .len()
                .saturating_mul(size_of::<(Vec<u8>, Vec<u8>)>()),
        )
        .saturating_add(key.labels.iter().fold(0_usize, |bytes, (key, value)| {
            bytes.saturating_add(key.len()).saturating_add(value.len())
        }))
}

fn frame_bytes(frames: &[ProfileFrame]) -> usize {
    frames.iter().fold(
        frames.len().saturating_mul(size_of::<ProfileFrame>()),
        |bytes, frame| bytes.saturating_add(frame.variables.len().saturating_mul(8)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::{FrameFlags, RawTrace};

    /// Scenario: Shutdown's finalization budget has already expired with multiple aggregated samples.
    /// Guarantees: No output samples are materialized; all multiplicities are counted and the window resets.
    #[test]
    fn expired_finalization_counts_every_sample() {
        let mut aggregator = BoundedAggregator::new(ResourceLimits::default());
        for address in [100, 100, 200] {
            aggregator
                .record(trace(address), process())
                .expect("sample");
        }
        let snapshot = aggregator.finalize_until(Instant::now());
        assert!(snapshot.samples.is_empty());
        assert_eq!(snapshot.logical_bytes, 0);
        assert!(snapshot.statistics.shutdown_incomplete);
        assert_eq!(
            snapshot.statistics.losses,
            [(DropReason::ShutdownDeadline, 3)]
        );
        assert!(aggregator.finalize().samples.is_empty());
    }

    fn trace(address: u64) -> RawTrace {
        RawTrace {
            pid: 10,
            tid: 11,
            ktime_ns: address,
            comm: b"work".to_vec(),
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
                data: address,
                variables: vec![7],
            }],
        }
    }

    fn process() -> ProcessIdentity {
        ProcessIdentity {
            pid: 10,
            start_time_ticks: 20,
            executable: Some("/work".to_owned()),
        }
    }

    /// Scenario: Two identical checked traces arrive in one reporting window.
    /// Guarantees: They form one deterministic owned sample with a count of two
    /// and no borrowed event memory.
    #[test]
    fn identical_samples_aggregate() {
        let mut aggregator = BoundedAggregator::new(ResourceLimits::default());
        aggregator.record(trace(100), process()).expect("first");
        aggregator.record(trace(100), process()).expect("second");
        let snapshot = aggregator.finalize();
        assert_eq!(snapshot.samples.len(), 1);
        assert_eq!(snapshot.samples[0].count, 2);
    }

    /// Scenario: A window receives more distinct stack keys than configured.
    /// Guarantees: New keys are rejected at capacity while existing aggregates
    /// remain valid and the reason is observable in the snapshot.
    #[test]
    fn aggregation_capacity_rejects_new_keys() {
        let limits = ResourceLimits {
            max_aggregation_keys: 1,
            ..ResourceLimits::default()
        };
        let mut aggregator = BoundedAggregator::new(limits);
        aggregator.record(trace(100), process()).expect("first");
        assert_eq!(
            aggregator.record(trace(200), process()),
            Err(DropReason::AggregationCapacity)
        );
        let snapshot = aggregator.finalize();
        assert_eq!(snapshot.samples.len(), 1);
        assert_eq!(
            snapshot.statistics.losses,
            vec![(DropReason::AggregationCapacity, 1)]
        );
    }

    /// Scenario: The same input set is aggregated in two fresh windows.
    /// Guarantees: BTree ordering produces byte-for-byte equivalent sample
    /// order independent of hash randomization.
    #[test]
    fn output_order_is_deterministic() {
        let build = || {
            let mut aggregator = BoundedAggregator::new(ResourceLimits::default());
            aggregator
                .record(trace(200), process())
                .expect("second key");
            aggregator.record(trace(100), process()).expect("first key");
            aggregator.finalize().samples
        };
        assert_eq!(build(), build());
    }

    /// Scenario: A window reaches its aggregation byte budget before its key-count budget.
    /// Guarantees: New storage is rejected while an existing key can still accumulate samples.
    #[test]
    fn byte_budget_allows_existing_keys_without_growth() {
        let mut probe = BoundedAggregator::new(ResourceLimits::default());
        probe.record(trace(100), process()).expect("one key");
        let limits = ResourceLimits {
            max_aggregation_logical_bytes: probe.logical_bytes,
            ..ResourceLimits::default()
        };
        let mut aggregator = BoundedAggregator::new(limits);
        aggregator.record(trace(100), process()).expect("first");
        aggregator.record(trace(100), process()).expect("existing");
        assert_eq!(
            aggregator.record(trace(200), process()),
            Err(DropReason::AggregationByteCapacity)
        );
        assert_eq!(aggregator.finalize().samples[0].count, 2);
        assert_eq!(aggregator.logical_bytes, 0);
    }

    /// Scenario: Snapshot limits discard an aggregate representing multiple raw samples.
    /// Guarantees: Loss counters count every discarded sample rather than only the aggregate key.
    #[test]
    fn snapshot_losses_count_sample_multiplicity() {
        for byte_limit in [false, true] {
            let mut limits = ResourceLimits::default();
            if byte_limit {
                limits.max_snapshot_logical_bytes = 1;
            } else {
                limits.max_samples_per_snapshot = 0;
            }
            let mut aggregator = BoundedAggregator::new(limits);
            for _ in 0..7 {
                aggregator.record(trace(100), process()).expect("sample");
            }
            let snapshot = aggregator.finalize();
            assert!(snapshot.samples.is_empty());
            let reason = if byte_limit {
                DropReason::SnapshotByteCapacity
            } else {
                DropReason::SnapshotSampleCapacity
            };
            assert_eq!(snapshot.statistics.losses, [(reason, 7)]);
        }
    }

    /// Scenario: Caller-constructed traces bypass the byte decoder and exceed a wire-format bound.
    /// Guarantees: Oversized strings or variable-word vectors cannot enter bounded aggregation.
    #[test]
    fn constructed_trace_shape_is_revalidated() {
        let mut aggregator = BoundedAggregator::new(ResourceLimits::default());
        let mut malformed = trace(100);
        malformed.user_frames[0].variables = vec![0; 15];
        assert_eq!(
            aggregator.record(malformed, process()),
            Err(DropReason::MalformedRecord)
        );
        let mut malformed = trace(100);
        malformed.comm = vec![b'a'; 17];
        assert_eq!(
            aggregator.record(malformed, process()),
            Err(DropReason::MalformedRecord)
        );
        assert!(aggregator.finalize().samples.is_empty());
    }

    /// Scenario: Two different threads emit the same stack under a one-thread limit.
    /// Guarantees: Thread state remains bounded independently of process and stack counts.
    #[test]
    fn thread_capacity_is_enforced() {
        let mut aggregator = BoundedAggregator::new(ResourceLimits {
            max_threads: 1,
            ..ResourceLimits::default()
        });
        aggregator
            .record(trace(100), process())
            .expect("first thread");
        let mut next = trace(100);
        next.tid = 12;
        assert_eq!(
            aggregator.record(next, process()),
            Err(DropReason::ThreadCapacity)
        );
    }
}
