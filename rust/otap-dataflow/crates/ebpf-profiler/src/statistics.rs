// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Low-cardinality collection and capacity statistics.

/// Fixed reasons for dropping samples. Counters are sample counts, not window
/// counts or omitted optional metadata. Kernel loss is reported separately from
/// userspace loss because helper failures and perf lost records can overlap.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u8)]
pub enum DropReason {
    /// Kernel perf buffer overflow.
    KernelBufferFull,
    /// Worker batch capacity was reached.
    RawEventCapacity,
    /// Aggregation key capacity was reached.
    AggregationCapacity,
    /// User-stack table capacity was reached.
    UserStackCapacity,
    /// Kernel-stack table capacity was reached.
    KernelStackCapacity,
    /// Process table capacity was reached.
    ProcessCapacity,
    /// Thread table capacity was reached.
    ThreadCapacity,
    /// Mapping table capacity was reached.
    MappingCapacity,
    /// Location table capacity was reached.
    LocationCapacity,
    /// Function or symbol table capacity was reached.
    SymbolCapacity,
    /// Snapshot size or row capacity was reached.
    SnapshotCapacity,
    /// Completed-snapshot queue was full.
    SnapshotQueueFull,
    /// Snapshot consumer disconnected.
    ConsumerDisconnected,
    /// Shutdown deadline forced data loss.
    ShutdownDeadline,
    /// A malformed kernel event was rejected.
    MalformedEvent,
    /// Required process or thread identity could not be established.
    MetadataUnavailable,
    /// The process generation is newer than the buffered sample.
    StaleProcess,
    /// An event source failed before the sample could be processed.
    SourceFailure,
    /// A shard window could not enter the bounded finalization queue.
    ShardQueueFull,
    /// A sample count could not be represented without overflow.
    CounterOverflow,
}

impl DropReason {
    /// Number of stable drop-reason slots.
    pub const COUNT: usize = Self::CounterOverflow as usize + 1;
}

/// Window counters and occupancy gauges. The profiler handle sums disjoint
/// window counters and retains gauge peaks; gauges are not a host process census.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProfilerStatistics {
    /// Sampling periods requested from the kernel.
    pub sampling_periods_attempted: u64,
    /// Raw events received from kernel or synthetic sources.
    pub raw_events_received: u64,
    /// Samples accepted into aggregation.
    pub samples_accepted: u64,
    /// Samples merged into an existing aggregation key.
    pub samples_aggregated: u64,
    /// Samples emitted in completed snapshots.
    pub samples_in_snapshots: u64,
    /// Kernel-reported lost perf events.
    pub kernel_lost_events: u64,
    /// BPF output-helper failures. This can overlap `kernel_lost_events`;
    /// consumers must not add those two counters.
    pub kernel_output_failures: u64,
    /// Sampling periods spent in the idle task and intentionally not exported.
    pub idle_samples_skipped: u64,
    /// Final non-idle BPF attempts not observed as userspace records. This
    /// includes output/transport loss, including loss without a later perf
    /// notification. It is unavailable for sources without kernel counters.
    pub unobserved_samples: Option<u64>,
    /// User-stack collection failures.
    pub user_stack_failures: u64,
    /// Kernel-stack collection failures.
    pub kernel_stack_failures: u64,
    /// Stacks truncated by the ABI or configured limit.
    pub truncated_stacks: u64,
    /// Process metadata lookup failures.
    pub process_lookup_failures: u64,
    /// Mapping lookup failures.
    pub mapping_lookup_failures: u64,
    /// Symbol lookup failures.
    pub symbol_lookup_failures: u64,
    /// Process rows retained in a window (peak on the cumulative handle).
    pub active_processes: u64,
    /// Thread rows retained in a window (peak on the cumulative handle).
    pub active_threads: u64,
    /// Unique user stacks in a window (peak on the cumulative handle).
    pub unique_user_stacks: u64,
    /// Unique kernel stacks in a window (peak on the cumulative handle).
    pub unique_kernel_stacks: u64,
    /// Mapping rows in a window (peak on the cumulative handle).
    pub mapping_cache_occupancy: u64,
    /// Cached symbol entries in a window (peak on the cumulative handle).
    pub symbol_cache_occupancy: u64,
    /// Bounded cache evictions.
    pub cache_evictions: u64,
    /// Symbol lookups served by cached file indexes.
    pub symbol_cache_hits: u64,
    /// Capacity rejections across all fixed tables.
    pub capacity_rejections: u64,
    /// Snapshots created.
    pub snapshots_created: u64,
    /// Snapshots dropped before consumption.
    pub snapshots_dropped: u64,
    /// Logical bytes in a snapshot (peak on the cumulative handle).
    pub snapshot_logical_bytes: u64,
    /// Reporting finalization duration (peak on the cumulative handle).
    pub reporting_duration_ns: u64,
    /// Maximum observed BOOTTIME event-to-aggregation delay, or unavailable
    /// when an injected/non-Linux clock cannot provide the kernel time domain.
    pub event_to_aggregation_ns_max: Option<u64>,
    /// Samples discarded during bounded shutdown.
    pub shutdown_losses: u64,
    /// Shutdown calls that exhausted their wait deadline (not sample loss).
    pub shutdown_timeouts: u64,
    /// Selected CPUs that could not be attached in best-effort mode.
    pub unavailable_cpus: u64,
    /// Successfully attached CPUs.
    pub attached_cpus: u64,
    /// Workers whose optional CPU-affinity request failed.
    pub affinity_failures: u64,
    /// Event-source errors observed by workers.
    pub source_errors: u64,
    /// Optional metadata fields omitted or truncated.
    pub metadata_omissions: u64,
    /// Diagnostic messages omitted after reaching the error-detail bound.
    pub error_details_dropped: u64,
    /// Worker drain calls, including empty wakeups.
    pub worker_wakeups: u64,
    /// Fixed-index counters matching [`DropReason`] discriminants.
    pub dropped_by_reason: [u64; DropReason::COUNT],
}

impl ProfilerStatistics {
    /// Increments one fixed drop-reason counter and total capacity state when
    /// applicable.
    pub fn record_drop(&mut self, reason: DropReason, count: u64) {
        let index = reason as usize;
        self.dropped_by_reason[index] = self.dropped_by_reason[index].saturating_add(count);
        if matches!(
            reason,
            DropReason::RawEventCapacity
                | DropReason::AggregationCapacity
                | DropReason::UserStackCapacity
                | DropReason::KernelStackCapacity
                | DropReason::ProcessCapacity
                | DropReason::ThreadCapacity
                | DropReason::MappingCapacity
                | DropReason::LocationCapacity
                | DropReason::SymbolCapacity
                | DropReason::SnapshotCapacity
        ) {
            self.capacity_rejections = self.capacity_rejections.saturating_add(count);
        }
        if reason == DropReason::ShutdownDeadline {
            self.shutdown_losses = self.shutdown_losses.saturating_add(count);
        }
    }

    /// Adds counters from another disjoint shard or completed window and retains
    /// peak occupancy gauges. No dynamic label or error string is introduced.
    pub fn merge(&mut self, other: &Self) {
        self.sampling_periods_attempted = self
            .sampling_periods_attempted
            .saturating_add(other.sampling_periods_attempted);
        self.raw_events_received = self
            .raw_events_received
            .saturating_add(other.raw_events_received);
        self.samples_accepted = self.samples_accepted.saturating_add(other.samples_accepted);
        self.samples_aggregated = self
            .samples_aggregated
            .saturating_add(other.samples_aggregated);
        self.samples_in_snapshots = self
            .samples_in_snapshots
            .saturating_add(other.samples_in_snapshots);
        self.kernel_lost_events = self
            .kernel_lost_events
            .saturating_add(other.kernel_lost_events);
        self.kernel_output_failures = self
            .kernel_output_failures
            .saturating_add(other.kernel_output_failures);
        self.idle_samples_skipped = self
            .idle_samples_skipped
            .saturating_add(other.idle_samples_skipped);
        self.unobserved_samples = match (self.unobserved_samples, other.unobserved_samples) {
            (Some(left), Some(right)) => Some(left.saturating_add(right)),
            (left, right) => left.or(right),
        };
        self.user_stack_failures = self
            .user_stack_failures
            .saturating_add(other.user_stack_failures);
        self.kernel_stack_failures = self
            .kernel_stack_failures
            .saturating_add(other.kernel_stack_failures);
        self.truncated_stacks = self.truncated_stacks.saturating_add(other.truncated_stacks);
        self.process_lookup_failures = self
            .process_lookup_failures
            .saturating_add(other.process_lookup_failures);
        self.mapping_lookup_failures = self
            .mapping_lookup_failures
            .saturating_add(other.mapping_lookup_failures);
        self.symbol_lookup_failures = self
            .symbol_lookup_failures
            .saturating_add(other.symbol_lookup_failures);
        self.cache_evictions = self.cache_evictions.saturating_add(other.cache_evictions);
        self.symbol_cache_hits = self
            .symbol_cache_hits
            .saturating_add(other.symbol_cache_hits);
        self.capacity_rejections = self
            .capacity_rejections
            .saturating_add(other.capacity_rejections);
        self.snapshots_created = self
            .snapshots_created
            .saturating_add(other.snapshots_created);
        self.snapshots_dropped = self
            .snapshots_dropped
            .saturating_add(other.snapshots_dropped);
        self.shutdown_losses = self.shutdown_losses.saturating_add(other.shutdown_losses);
        self.shutdown_timeouts = self
            .shutdown_timeouts
            .saturating_add(other.shutdown_timeouts);
        self.unavailable_cpus = self.unavailable_cpus.saturating_add(other.unavailable_cpus);
        self.attached_cpus = self.attached_cpus.max(other.attached_cpus);
        self.affinity_failures = self
            .affinity_failures
            .saturating_add(other.affinity_failures);
        self.source_errors = self.source_errors.saturating_add(other.source_errors);
        self.metadata_omissions = self
            .metadata_omissions
            .saturating_add(other.metadata_omissions);
        self.error_details_dropped = self
            .error_details_dropped
            .saturating_add(other.error_details_dropped);
        self.worker_wakeups = self.worker_wakeups.saturating_add(other.worker_wakeups);
        self.active_processes = self.active_processes.max(other.active_processes);
        self.active_threads = self.active_threads.max(other.active_threads);
        self.unique_user_stacks = self.unique_user_stacks.max(other.unique_user_stacks);
        self.unique_kernel_stacks = self.unique_kernel_stacks.max(other.unique_kernel_stacks);
        self.mapping_cache_occupancy = self
            .mapping_cache_occupancy
            .max(other.mapping_cache_occupancy);
        self.symbol_cache_occupancy = self
            .symbol_cache_occupancy
            .max(other.symbol_cache_occupancy);
        self.snapshot_logical_bytes = self
            .snapshot_logical_bytes
            .max(other.snapshot_logical_bytes);
        self.reporting_duration_ns = self.reporting_duration_ns.max(other.reporting_duration_ns);
        self.event_to_aggregation_ns_max = self
            .event_to_aggregation_ns_max
            .max(other.event_to_aggregation_ns_max);
        for (target, source) in self
            .dropped_by_reason
            .iter_mut()
            .zip(other.dropped_by_reason)
        {
            *target = target.saturating_add(source);
        }
    }

    /// Total userspace sample loss; excludes kernel lost notifications.
    #[must_use]
    pub fn userspace_dropped(&self) -> u64 {
        self.dropped_by_reason[1..]
            .iter()
            .copied()
            .fold(0, u64::saturating_add)
    }
}
