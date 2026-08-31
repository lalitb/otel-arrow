// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Fixed-cardinality metrics and health-event suppression for filelog.
//!
//! The blocking worker owns source state, while the async receiver owns the
//! registered metric set. A fixed array of atomics transfers counter deltas
//! and current gauges without another queue, a lock, or telemetry-driven
//! blocking. Counter slots are drained with `swap(0)`: a deferred metrics
//! report retains values in the hot metric set, while later worker updates
//! accumulate independently in the bridge.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use otap_df_telemetry::instrument::{Counter, Gauge, Mmsc};
use otap_df_telemetry::metrics::{MetricSet, MetricSetSnapshot};
use otap_df_telemetry_macros::metric_set;

const HEALTH_EVENT_INTERVAL: Duration = Duration::from_secs(30);

/// Complete fixed-cardinality metric set for the Phase 1 filelog receiver.
#[metric_set(name = "receiver.filelog")]
#[derive(Debug, Default, Clone)]
pub struct FilelogReceiverMetrics {
    /// Receiver starts.
    #[metric(name = "lifecycle.starts", unit = "{start}")]
    pub starts: Counter<u64>,
    /// Clean drain transitions.
    #[metric(name = "lifecycle.drains", unit = "{drain}")]
    pub drains: Counter<u64>,
    /// Forced shutdown transitions.
    #[metric(name = "lifecycle.shutdowns", unit = "{shutdown}")]
    pub shutdowns: Counter<u64>,
    /// Terminal receiver failures.
    #[metric(name = "lifecycle.failures", unit = "{failure}")]
    pub terminal_failures: Counter<u64>,
    /// Detailed health events withheld by the fixed-category limiter.
    #[metric(name = "health_events.suppressed", unit = "{event}")]
    pub health_events_suppressed: Counter<u64>,

    /// Records successfully handed downstream, including resend attempts.
    #[metric(name = "records.emitted", unit = "{record}")]
    pub records_emitted: Counter<u64>,
    /// Source bytes represented by successfully handed-off records.
    #[metric(name = "bytes.source.emitted", unit = "By")]
    pub source_bytes_emitted: Counter<u64>,
    /// Logical OTAP bytes successfully handed downstream.
    #[metric(name = "bytes.logical.emitted", unit = "By")]
    pub logical_bytes_emitted: Counter<u64>,
    /// Initial successful downstream batch sends. Resends use the separate
    /// `batches.resent` semantic.
    #[metric(name = "batches.emitted", unit = "{batch}")]
    pub batches_emitted: Counter<u64>,
    /// Matching downstream Acks.
    #[metric(name = "batches.acked", unit = "{batch}")]
    pub batches_acked: Counter<u64>,
    /// Matching downstream Nacks.
    #[metric(name = "batches.nacked", unit = "{batch}")]
    pub batches_nacked: Counter<u64>,
    /// Pre-publication attempts that found no accepted downstream route
    /// (no default output port, or a route that was or became closed).
    /// Distinct from `batches.nacked`.
    #[metric(name = "batches.no_route", unit = "{batch}")]
    pub batches_no_route: Counter<u64>,
    /// Successful resend handoffs.
    #[metric(name = "batches.resent", unit = "{batch}")]
    pub batches_resent: Counter<u64>,
    /// Retry attempts scheduled after an aggregate Nack or `NoRoute`.
    #[metric(name = "retries.attempted", unit = "{retry}")]
    pub retry_attempts: Counter<u64>,
    /// Retained batches that exhausted their send-attempt budget.
    #[metric(name = "retries.exhausted", unit = "{batch}")]
    pub retry_exhausted: Counter<u64>,
    /// Configured retry backoff observations.
    #[metric(name = "retry.backoff.duration", unit = "ns")]
    pub retry_backoff_duration_ns: Mmsc,
    /// Batches durably advanced under explicit drop-and-continue policy.
    #[metric(name = "batches.explicit_loss", unit = "{batch}")]
    pub batches_explicit_loss: Counter<u64>,
    /// Records durably skipped after explicit drop-and-continue exhaustion.
    #[metric(name = "records.dropped_on_nack", unit = "{record}")]
    pub records_dropped_on_nack: Counter<u64>,
    /// Fully framed records retained across one predecessor in-flight batch.
    #[metric(name = "carry_over.records", unit = "{record}")]
    pub carry_over_records: Counter<u64>,
    /// Completion contexts with an invalid shape.
    #[metric(name = "completions.malformed", unit = "{completion}")]
    pub malformed_completions: Counter<u64>,
    /// Completion contexts for a different batch or attempt.
    #[metric(name = "completions.stale", unit = "{completion}")]
    pub stale_completions: Counter<u64>,
    /// Repeated current completions after the state already advanced.
    #[metric(name = "completions.duplicate", unit = "{completion}")]
    pub duplicate_completions: Counter<u64>,
    /// Downstream-full pause durations.
    #[metric(name = "backpressure.pause.duration", unit = "ns")]
    pub backpressure_pause_duration_ns: Mmsc,

    /// WAL bytes appended over the receiver lifetime.
    #[metric(name = "checkpoint.wal.bytes", unit = "By")]
    pub checkpoint_wal_bytes: Counter<u64>,
    /// WAL transactions appended.
    #[metric(name = "checkpoint.transactions", unit = "{transaction}")]
    pub checkpoint_transactions: Counter<u64>,
    /// WAL sync operations completed.
    #[metric(name = "checkpoint.syncs", unit = "{sync}")]
    pub checkpoint_syncs: Counter<u64>,
    /// Checkpoint operation failures.
    #[metric(name = "checkpoint.failures", unit = "{failure}")]
    pub checkpoint_failures: Counter<u64>,
    /// Total WAL append latency; divide by persist operations for the mean.
    #[metric(name = "checkpoint.persist.duration.total", unit = "ns")]
    pub checkpoint_persist_duration_ns: Counter<u64>,
    /// WAL append latency observation count.
    #[metric(name = "checkpoint.persist.operations", unit = "{operation}")]
    pub checkpoint_persist_operations: Counter<u64>,
    /// Total WAL sync latency; divide by sync operations for the mean.
    #[metric(name = "checkpoint.sync.duration.total", unit = "ns")]
    pub checkpoint_sync_duration_ns: Counter<u64>,
    /// WAL sync latency observation count.
    #[metric(name = "checkpoint.sync.operations", unit = "{operation}")]
    pub checkpoint_sync_operations: Counter<u64>,
    /// Total delay from the first unsynced Ack transaction to successful sync.
    #[metric(name = "checkpoint.sync.delay.total", unit = "ns")]
    pub checkpoint_sync_delay_ns: Counter<u64>,
    /// Successful sync-delay observations.
    #[metric(name = "checkpoint.sync.delay.operations", unit = "{operation}")]
    pub checkpoint_sync_delay_operations: Counter<u64>,
    /// Total checkpoint namespace recovery duration.
    #[metric(name = "checkpoint.recovery.duration.total", unit = "ns")]
    pub checkpoint_recovery_duration_ns: Counter<u64>,
    /// Checkpoint namespace recovery attempts observed.
    #[metric(name = "checkpoint.recovery.operations", unit = "{operation}")]
    pub checkpoint_recovery_operations: Counter<u64>,
    /// Total compaction latency; divide by compactions for the mean.
    #[metric(name = "checkpoint.compaction.duration.total", unit = "ns")]
    pub checkpoint_compaction_duration_ns: Counter<u64>,
    /// Completed checkpoint compactions.
    #[metric(name = "checkpoint.compactions", unit = "{compaction}")]
    pub checkpoint_compactions: Counter<u64>,
    /// Retired generations removed.
    #[metric(name = "checkpoint.cleanup.generations", unit = "{generation}")]
    pub checkpoint_cleanup_generations: Counter<u64>,
    /// Retired-generation cleanup failures.
    #[metric(name = "checkpoint.cleanup.failures", unit = "{failure}")]
    pub checkpoint_cleanup_failures: Counter<u64>,
    /// Current live WAL size.
    #[metric(name = "checkpoint.wal.size", unit = "By")]
    pub checkpoint_wal_size: Gauge<u64>,
    /// Current complete transactions in the live WAL.
    #[metric(name = "checkpoint.wal.transactions", unit = "{transaction}")]
    pub checkpoint_wal_transactions: Gauge<u64>,
    /// Checkpoint records removed by receiver-owned retention.
    #[metric(name = "checkpoint.records.removed", unit = "{record}")]
    pub checkpoint_records_removed: Counter<u64>,

    /// Include matches observed by discovery.
    #[metric(name = "files.discovered", unit = "{file}")]
    pub files_discovered: Counter<u64>,
    /// Eligible regular-file observations.
    #[metric(name = "files.eligible", unit = "{file}")]
    pub files_eligible: Counter<u64>,
    /// New candidate transitions.
    #[metric(name = "discovery.observed", unit = "{file}")]
    pub discovery_observed: Counter<u64>,
    /// Candidate metadata transitions.
    #[metric(name = "discovery.updated", unit = "{file}")]
    pub discovery_updated: Counter<u64>,
    /// Candidate removal transitions.
    #[metric(name = "discovery.removed", unit = "{file}")]
    pub discovery_removed: Counter<u64>,
    /// Completed reconciliation scans.
    #[metric(name = "discovery.scans", unit = "{scan}")]
    pub discovery_scans: Counter<u64>,
    /// Recoverable scan errors.
    #[metric(name = "discovery.scan.errors", unit = "{error}")]
    pub discovery_scan_errors: Counter<u64>,
    /// Total reconciliation scan latency.
    #[metric(name = "discovery.scan.duration.total", unit = "ns")]
    pub discovery_scan_duration_ns: Counter<u64>,
    /// Candidate observations not retained due to bounded admission.
    #[metric(name = "candidates.overflowed", unit = "{candidate}")]
    pub candidate_overflow: Counter<u64>,
    /// Reconciliation passes that observed candidate overflow.
    #[metric(name = "candidates.overflow.scans", unit = "{scan}")]
    pub candidate_overflow_scans: Counter<u64>,
    /// Total admission wait of emitted retained candidates.
    #[metric(name = "candidates.admission.delay.total", unit = "ns")]
    pub candidate_admission_delay_ns: Counter<u64>,
    /// Retained candidates admitted after an observed wait.
    #[metric(name = "candidates.admissions", unit = "{candidate}")]
    pub candidate_admissions: Counter<u64>,
    /// Events that encountered a full tracked table.
    #[metric(name = "files.tracked.saturation", unit = "{event}")]
    pub tracked_saturation: Counter<u64>,
    /// Events that encountered no evictable descriptor slot.
    #[metric(name = "files.descriptor.saturation", unit = "{event}")]
    pub descriptor_saturation: Counter<u64>,
    /// Confirmed least-recently-served descriptor evictions.
    #[metric(name = "files.descriptor.evictions", unit = "{eviction}")]
    pub descriptor_evictions: Counter<u64>,
    /// Failed descriptor reopens after a reader had opened successfully.
    #[metric(name = "files.descriptor.reopen_failures", unit = "{failure}")]
    pub descriptor_reopen_failures: Counter<u64>,
    /// Startup warnings where the receiver-owned descriptor budget exceeds
    /// 80 percent of the process soft limit.
    #[metric(name = "descriptor.budget.warnings", unit = "{warning}")]
    pub descriptor_budget_warnings: Counter<u64>,
    /// Source, probe, or traversal operations scheduled for bounded
    /// environmental retry.
    #[metric(name = "environmental.reprobes", unit = "{reprobe}")]
    pub environmental_reprobes: Counter<u64>,
    /// Current durable tracked-record population.
    #[metric(name = "files.tracked", unit = "{file}")]
    pub files_tracked: Gauge<u64>,
    /// Current retained pending-candidate population.
    #[metric(name = "files.pending", unit = "{file}")]
    pub files_pending: Gauge<u64>,
    /// Current resident source descriptor population.
    #[metric(name = "files.open", unit = "{file}")]
    pub files_open: Gauge<u64>,
    /// Current readers blocked on descriptor capacity.
    #[metric(name = "files.descriptor_blocked", unit = "{file}")]
    pub files_descriptor_blocked: Gauge<u64>,
    /// Current removed readers waiting for finalization.
    #[metric(name = "files.removed_waiting", unit = "{file}")]
    pub files_removed_waiting: Gauge<u64>,
    /// Current durable quarantined-record population.
    #[metric(name = "files.quarantined", unit = "{file}")]
    pub files_quarantined: Gauge<u64>,
    /// Current retained candidate oldest age.
    #[metric(name = "candidates.oldest.age", unit = "ns")]
    pub candidate_oldest_age_ns: Gauge<u64>,
    /// Current age of a continuing overflow condition.
    #[metric(name = "candidates.overflow.persistence", unit = "ns")]
    pub candidate_overflow_persistence_ns: Gauge<u64>,
    /// Whether the durable tracked table is currently saturated.
    #[metric(name = "files.tracked.saturated", unit = "{boolean}")]
    pub tracked_saturated: Gauge<u64>,
    /// Whether descriptor capacity is currently saturated.
    #[metric(name = "files.descriptor.saturated", unit = "{boolean}")]
    pub descriptor_saturated: Gauge<u64>,
    /// Current removed readers retaining late-write-capable descriptors.
    #[metric(name = "rotation.pinned_handles", unit = "{handle}")]
    pub pinned_rotated_handles: Gauge<u64>,
    /// Age of the oldest removed reader retaining a descriptor.
    #[metric(name = "rotation.pinned_oldest.age", unit = "ns")]
    pub pinned_rotated_oldest_age_ns: Gauge<u64>,
    /// Descriptor saturation observations while rotated handles were pinned.
    #[metric(name = "rotation.pinned.saturation", unit = "{event}")]
    pub pinned_rotation_saturation: Counter<u64>,

    /// Durable file registrations.
    #[metric(name = "identity.registrations", unit = "{file}")]
    pub identity_registrations: Counter<u64>,
    /// Recovery mismatches that selected a new logical identity.
    #[metric(name = "identity.resets", unit = "{file}")]
    pub identity_resets: Counter<u64>,
    /// Unsafe recovery evidence observations.
    #[metric(name = "identity.recovery_mismatches", unit = "{file}")]
    pub identity_recovery_mismatches: Counter<u64>,
    /// Exact-locator recovery matches.
    #[metric(name = "identity.matches.exact_locator", unit = "{file}")]
    pub identity_exact_matches: Counter<u64>,

    /// Same-locator path changes associated with move/create rotation.
    #[metric(name = "rotation.move_create", unit = "{rotation}")]
    pub rotation_move_create: Counter<u64>,
    /// New identities registered as a recognized move/create replacement
    /// for a rebounding distinguished matched-path binding.
    #[metric(name = "rotation.recognized_replacement", unit = "{rotation}")]
    pub rotation_recognized_replacement: Counter<u64>,
    /// Removed-file lifecycle finalizations.
    #[metric(name = "rotation.finalizations", unit = "{file}")]
    pub rotation_finalizations: Counter<u64>,
    /// Source data observed after rotation inactivity began.
    #[metric(name = "rotation.late_writes", unit = "{write}")]
    pub rotation_late_writes: Counter<u64>,
    /// Removed readers whose descriptor was unavailable for late-write capture.
    #[metric(name = "rotation.descriptor_unavailable", unit = "{file}")]
    pub rotation_descriptor_unavailable: Counter<u64>,
    /// Observable copy-truncate transitions.
    #[metric(name = "rotation.copytruncate.detected", unit = "{rotation}")]
    pub copytruncate_detected: Counter<u64>,
    /// Copy-truncate transitions handled by durable fail quarantine.
    #[metric(name = "rotation.copytruncate.fail", unit = "{rotation}")]
    pub copytruncate_fail: Counter<u64>,
    /// Copy-truncate transitions handled by durable read-new reset.
    #[metric(name = "rotation.copytruncate.read_new", unit = "{rotation}")]
    pub copytruncate_read_new: Counter<u64>,

    /// Records emitted after replacement decoding.
    #[metric(name = "decode.replace.records", unit = "{record}")]
    pub decode_replace_records: Counter<u64>,
    /// Malformed units replaced.
    #[metric(name = "decode.replace.units", unit = "{unit}")]
    pub decode_replace_units: Counter<u64>,
    /// Records emitted as preserved raw evidence.
    #[metric(name = "decode.preserve_raw.records", unit = "{record}")]
    pub decode_preserve_raw_records: Counter<u64>,
    /// Malformed units preserved as raw evidence.
    #[metric(name = "decode.preserve_raw.units", unit = "{unit}")]
    pub decode_preserve_raw_units: Counter<u64>,
    /// Decode-fail policy outcomes.
    #[metric(name = "decode.failures", unit = "{failure}")]
    pub decode_failures: Counter<u64>,
    /// Truncated logical records.
    #[metric(name = "records.truncated", unit = "{record}")]
    pub records_truncated: Counter<u64>,
    /// Source bytes discarded by explicit truncate policy.
    #[metric(name = "bytes.discarded", unit = "By")]
    pub source_bytes_discarded: Counter<u64>,
    /// Emitted split fragments.
    #[metric(name = "records.split_fragments", unit = "{fragment}")]
    pub split_fragments: Counter<u64>,
    /// Logical records whose first split fragment was emitted.
    #[metric(name = "records.split", unit = "{record}")]
    pub records_split: Counter<u64>,
    /// Recoverable partial source bytes present at drain.
    #[metric(name = "partial.bytes.pending_drain", unit = "By")]
    pub partial_bytes_pending_drain: Counter<u64>,
    /// Records emitted only because permanent rotation EOF established a
    /// terminal boundary.
    #[metric(name = "terminal.unterminated.records", unit = "{record}")]
    pub terminal_unterminated_records: Counter<u64>,
    /// Current recoverable partial source bytes.
    #[metric(name = "partial.bytes.pending", unit = "By")]
    pub partial_bytes_pending: Gauge<u64>,
    /// Start-pattern lines emitted through newline fallback.
    #[metric(name = "multiline.pattern_fallback", unit = "{line}")]
    pub pattern_fallback: Counter<u64>,
    /// Flushes caused by the multiline line bound.
    #[metric(name = "flush.max_lines", unit = "{flush}")]
    pub flush_max_lines: Counter<u64>,
    /// Flushes caused by the EOF-gated idle timeout.
    #[metric(name = "flush.timeout", unit = "{flush}")]
    pub flush_timeout: Counter<u64>,
    /// Flushes before a bounded oversized physical line.
    #[metric(name = "flush.oversize_line_boundary", unit = "{flush}")]
    pub flush_oversize_line_boundary: Counter<u64>,
    /// Rotation-triggered partial flushes.
    #[metric(name = "flush.rotation", unit = "{flush}")]
    pub flush_rotation: Counter<u64>,
    /// Drain-triggered partial flushes.
    #[metric(name = "flush.drain", unit = "{flush}")]
    pub flush_drain: Counter<u64>,
    /// Checkpoint records rejected for an incompatible framing profile.
    #[metric(name = "framing.profile.incompatible", unit = "{file}")]
    pub framing_profile_incompatible: Counter<u64>,
    /// Native advisory paths whose bounded evidence stores only a suffix.
    #[metric(name = "path.advisory.truncated", unit = "{path}")]
    pub advisory_path_truncated: Counter<u64>,
    /// Positioned source-read turns attempted.
    #[metric(name = "reader.turns", unit = "{turn}")]
    pub read_turns: Counter<u64>,
    /// Ordinary EOF deadlines promoted for another source probe.
    #[metric(name = "reader.eof.reprobes", unit = "{reprobe}")]
    pub eof_reprobes: Counter<u64>,

    /// Decode-policy quarantines.
    #[metric(name = "quarantine.decode", unit = "{file}")]
    pub quarantine_decode: Counter<u64>,
    /// Truncation-policy quarantines.
    #[metric(name = "quarantine.truncate", unit = "{file}")]
    pub quarantine_truncate: Counter<u64>,
    /// Recovery-mismatch quarantines.
    #[metric(name = "quarantine.recovery_mismatch", unit = "{file}")]
    pub quarantine_recovery_mismatch: Counter<u64>,
    /// Distribution-defined or otherwise unclassified quarantines.
    #[metric(name = "quarantine.other", unit = "{file}")]
    pub quarantine_other: Counter<u64>,
    /// Explicit reset-to-beginning recovery actions.
    #[metric(name = "quarantine.recovery.reset_to_beginning", unit = "{action}")]
    pub quarantine_reset_beginning: Counter<u64>,
    /// Explicit reset-to-end recovery actions.
    #[metric(name = "quarantine.recovery.reset_to_end", unit = "{action}")]
    pub quarantine_reset_end: Counter<u64>,
    /// Explicit keep-failed recovery actions.
    #[metric(name = "quarantine.recovery.keep_failed", unit = "{action}")]
    pub quarantine_keep_failed: Counter<u64>,
    /// Explicit administrative quarantine removals.
    #[metric(name = "quarantine.recovery.remove", unit = "{action}")]
    pub quarantine_remove: Counter<u64>,

    /// Total checkpoint namespace-lock acquisition wait.
    #[metric(name = "ownership.namespace_lock.wait.duration.total", unit = "ns")]
    pub namespace_lock_wait_ns: Counter<u64>,
    /// Namespace-lock acquisitions observed.
    #[metric(name = "ownership.namespace_lock.waits", unit = "{wait}")]
    pub namespace_lock_waits: Counter<u64>,
    /// Failed immediate lock attempts before acquisition.
    #[metric(name = "ownership.namespace_lock.contentions", unit = "{contention}")]
    pub namespace_lock_contentions: Counter<u64>,
    /// Namespace-lock acquisition failures.
    #[metric(name = "ownership.namespace_lock.failures", unit = "{failure}")]
    pub namespace_lock_failures: Counter<u64>,
    /// Total runtime-lease acquisition wait.
    #[metric(name = "ownership.runtime_lease.wait.duration.total", unit = "ns")]
    pub runtime_lease_wait_ns: Counter<u64>,
    /// Runtime-lease waits observed.
    #[metric(name = "ownership.runtime_lease.waits", unit = "{wait}")]
    pub runtime_lease_waits: Counter<u64>,
    /// Runtime-lease contentions.
    #[metric(name = "ownership.runtime_lease.contentions", unit = "{contention}")]
    pub runtime_lease_contentions: Counter<u64>,
    /// Runtime-lease acquisition or registry failures.
    #[metric(name = "ownership.runtime_lease.failures", unit = "{failure}")]
    pub runtime_lease_failures: Counter<u64>,
    /// Source bytes returned by positioned reads, including replay.
    #[metric(name = "source.bytes.read", unit = "By")]
    pub source_bytes_read: Counter<u64>,
}

/// Fixed worker-owned counter slots transferred to the async metric owner.
#[derive(Clone, Copy, Debug)]
#[repr(usize)]
pub(super) enum WorkerCounter {
    HealthSuppressed,
    CheckpointWalBytes,
    CheckpointTransactions,
    CheckpointSyncs,
    CheckpointFailures,
    CheckpointPersistDurationNs,
    CheckpointPersistOperations,
    CheckpointSyncDurationNs,
    CheckpointSyncOperations,
    CheckpointSyncDelayNs,
    CheckpointSyncDelayOperations,
    CheckpointRecoveryDurationNs,
    CheckpointRecoveryOperations,
    CheckpointCompactionDurationNs,
    CheckpointCompactions,
    CheckpointCleanupGenerations,
    CheckpointCleanupFailures,
    FilesDiscovered,
    FilesEligible,
    DiscoveryObserved,
    DiscoveryUpdated,
    DiscoveryRemoved,
    DiscoveryScans,
    DiscoveryScanErrors,
    DiscoveryScanDurationNs,
    CandidateOverflow,
    CandidateOverflowScans,
    CandidateAdmissionDelayNs,
    CandidateAdmissions,
    TrackedSaturation,
    DescriptorSaturation,
    DescriptorEvictions,
    DescriptorReopenFailures,
    PinnedRotationSaturation,
    DescriptorBudgetWarnings,
    EnvironmentalReprobes,
    IdentityRegistrations,
    IdentityResets,
    IdentityRecoveryMismatches,
    IdentityExactMatches,
    RotationMoveCreate,
    RotationRecognizedReplacement,
    RotationFinalizations,
    RotationLateWrites,
    RotationDescriptorUnavailable,
    CopytruncateDetected,
    CopytruncateFail,
    CopytruncateReadNew,
    DecodeReplaceRecords,
    DecodeReplaceUnits,
    DecodePreserveRawRecords,
    DecodePreserveRawUnits,
    DecodeFailures,
    RecordsTruncated,
    SourceBytesDiscarded,
    SplitFragments,
    RecordsSplit,
    CarryOverRecords,
    PartialBytesPendingDrain,
    TerminalUnterminatedRecords,
    PatternFallback,
    FlushMaxLines,
    FlushTimeout,
    FlushOversizeLineBoundary,
    FlushRotation,
    FlushDrain,
    FramingProfileIncompatible,
    AdvisoryPathTruncated,
    ReadTurns,
    EofReprobes,
    CheckpointRecordsRemoved,
    QuarantineDecode,
    QuarantineTruncate,
    QuarantineRecoveryMismatch,
    QuarantineOther,
    QuarantineResetBeginning,
    QuarantineResetEnd,
    QuarantineKeepFailed,
    QuarantineRemove,
    NamespaceLockWaitNs,
    NamespaceLockWaits,
    NamespaceLockContentions,
    NamespaceLockFailures,
    RuntimeLeaseWaitNs,
    RuntimeLeaseWaits,
    RuntimeLeaseContentions,
    RuntimeLeaseFailures,
    SourceBytesRead,
}

impl WorkerCounter {
    const COUNT: usize = Self::SourceBytesRead as usize + 1;
}

/// Fixed current-value slots transferred from the worker.
#[derive(Clone, Copy, Debug)]
#[repr(usize)]
pub(super) enum WorkerGauge {
    CheckpointWalSize,
    CheckpointWalTransactions,
    FilesTracked,
    FilesPending,
    FilesOpen,
    FilesDescriptorBlocked,
    FilesRemovedWaiting,
    FilesQuarantined,
    CandidateOldestAgeNs,
    CandidateOverflowPersistenceNs,
    TrackedSaturated,
    DescriptorSaturated,
    PinnedRotatedHandles,
    PinnedRotatedOldestAgeNs,
    PartialBytesPending,
}

impl WorkerGauge {
    const COUNT: usize = Self::PartialBytesPending as usize + 1;
}

/// Fixed-size, nonblocking cross-thread telemetry bridge.
#[derive(Debug)]
pub(super) struct WorkerTelemetryBridge {
    counters: [AtomicU64; WorkerCounter::COUNT],
    gauges: [AtomicU64; WorkerGauge::COUNT],
    #[cfg(test)]
    peak_partial_bytes_pending: AtomicU64,
}

impl Default for WorkerTelemetryBridge {
    fn default() -> Self {
        Self {
            counters: [const { AtomicU64::new(0) }; WorkerCounter::COUNT],
            gauges: [const { AtomicU64::new(0) }; WorkerGauge::COUNT],
            #[cfg(test)]
            peak_partial_bytes_pending: AtomicU64::new(0),
        }
    }
}

impl WorkerTelemetryBridge {
    /// Adds one worker counter delta with documented saturation.
    pub(super) fn add(&self, counter: WorkerCounter, value: u64) {
        if value == 0 {
            return;
        }
        let slot = &self.counters[counter as usize];
        let _ = slot.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            Some(current.saturating_add(value))
        });
    }

    /// Publishes one current gauge.
    pub(super) fn set(&self, gauge: WorkerGauge, value: u64) {
        self.gauges[gauge as usize].store(value, Ordering::Relaxed);
        #[cfg(test)]
        if matches!(gauge, WorkerGauge::PartialBytesPending) {
            let _ = self
                .peak_partial_bytes_pending
                .fetch_max(value, Ordering::Relaxed);
        }
    }

    /// Drains every counter exactly once and refreshes every current gauge.
    pub(super) fn drain_into(&self, metrics: &mut MetricSet<FilelogReceiverMetrics>) {
        macro_rules! drain {
            ($counter:ident, $field:ident) => {
                add_counter_saturating(
                    &mut metrics.$field,
                    self.counters[WorkerCounter::$counter as usize].swap(0, Ordering::AcqRel),
                );
            };
        }
        drain!(HealthSuppressed, health_events_suppressed);
        drain!(CheckpointWalBytes, checkpoint_wal_bytes);
        drain!(CheckpointTransactions, checkpoint_transactions);
        drain!(CheckpointSyncs, checkpoint_syncs);
        drain!(CheckpointFailures, checkpoint_failures);
        drain!(CheckpointPersistDurationNs, checkpoint_persist_duration_ns);
        drain!(CheckpointPersistOperations, checkpoint_persist_operations);
        drain!(CheckpointSyncDurationNs, checkpoint_sync_duration_ns);
        drain!(CheckpointSyncOperations, checkpoint_sync_operations);
        drain!(CheckpointSyncDelayNs, checkpoint_sync_delay_ns);
        drain!(
            CheckpointSyncDelayOperations,
            checkpoint_sync_delay_operations
        );
        drain!(
            CheckpointRecoveryDurationNs,
            checkpoint_recovery_duration_ns
        );
        drain!(CheckpointRecoveryOperations, checkpoint_recovery_operations);
        drain!(
            CheckpointCompactionDurationNs,
            checkpoint_compaction_duration_ns
        );
        drain!(CheckpointCompactions, checkpoint_compactions);
        drain!(CheckpointCleanupGenerations, checkpoint_cleanup_generations);
        drain!(CheckpointCleanupFailures, checkpoint_cleanup_failures);
        drain!(FilesDiscovered, files_discovered);
        drain!(FilesEligible, files_eligible);
        drain!(DiscoveryObserved, discovery_observed);
        drain!(DiscoveryUpdated, discovery_updated);
        drain!(DiscoveryRemoved, discovery_removed);
        drain!(DiscoveryScans, discovery_scans);
        drain!(DiscoveryScanErrors, discovery_scan_errors);
        drain!(DiscoveryScanDurationNs, discovery_scan_duration_ns);
        drain!(CandidateOverflow, candidate_overflow);
        drain!(CandidateOverflowScans, candidate_overflow_scans);
        drain!(CandidateAdmissionDelayNs, candidate_admission_delay_ns);
        drain!(CandidateAdmissions, candidate_admissions);
        drain!(TrackedSaturation, tracked_saturation);
        drain!(DescriptorSaturation, descriptor_saturation);
        drain!(DescriptorEvictions, descriptor_evictions);
        drain!(DescriptorReopenFailures, descriptor_reopen_failures);
        drain!(PinnedRotationSaturation, pinned_rotation_saturation);
        drain!(DescriptorBudgetWarnings, descriptor_budget_warnings);
        drain!(EnvironmentalReprobes, environmental_reprobes);
        drain!(IdentityRegistrations, identity_registrations);
        drain!(IdentityResets, identity_resets);
        drain!(IdentityRecoveryMismatches, identity_recovery_mismatches);
        drain!(IdentityExactMatches, identity_exact_matches);
        drain!(RotationMoveCreate, rotation_move_create);
        drain!(
            RotationRecognizedReplacement,
            rotation_recognized_replacement
        );
        drain!(RotationFinalizations, rotation_finalizations);
        drain!(RotationLateWrites, rotation_late_writes);
        drain!(
            RotationDescriptorUnavailable,
            rotation_descriptor_unavailable
        );
        drain!(CopytruncateDetected, copytruncate_detected);
        drain!(CopytruncateFail, copytruncate_fail);
        drain!(CopytruncateReadNew, copytruncate_read_new);
        drain!(DecodeReplaceRecords, decode_replace_records);
        drain!(DecodeReplaceUnits, decode_replace_units);
        drain!(DecodePreserveRawRecords, decode_preserve_raw_records);
        drain!(DecodePreserveRawUnits, decode_preserve_raw_units);
        drain!(DecodeFailures, decode_failures);
        drain!(RecordsTruncated, records_truncated);
        drain!(SourceBytesDiscarded, source_bytes_discarded);
        drain!(SplitFragments, split_fragments);
        drain!(RecordsSplit, records_split);
        drain!(CarryOverRecords, carry_over_records);
        drain!(PartialBytesPendingDrain, partial_bytes_pending_drain);
        drain!(TerminalUnterminatedRecords, terminal_unterminated_records);
        drain!(PatternFallback, pattern_fallback);
        drain!(FlushMaxLines, flush_max_lines);
        drain!(FlushTimeout, flush_timeout);
        drain!(FlushOversizeLineBoundary, flush_oversize_line_boundary);
        drain!(FlushRotation, flush_rotation);
        drain!(FlushDrain, flush_drain);
        drain!(FramingProfileIncompatible, framing_profile_incompatible);
        drain!(AdvisoryPathTruncated, advisory_path_truncated);
        drain!(ReadTurns, read_turns);
        drain!(EofReprobes, eof_reprobes);
        drain!(CheckpointRecordsRemoved, checkpoint_records_removed);
        drain!(QuarantineDecode, quarantine_decode);
        drain!(QuarantineTruncate, quarantine_truncate);
        drain!(QuarantineRecoveryMismatch, quarantine_recovery_mismatch);
        drain!(QuarantineOther, quarantine_other);
        drain!(QuarantineResetBeginning, quarantine_reset_beginning);
        drain!(QuarantineResetEnd, quarantine_reset_end);
        drain!(QuarantineKeepFailed, quarantine_keep_failed);
        drain!(QuarantineRemove, quarantine_remove);
        drain!(NamespaceLockWaitNs, namespace_lock_wait_ns);
        drain!(NamespaceLockWaits, namespace_lock_waits);
        drain!(NamespaceLockContentions, namespace_lock_contentions);
        drain!(NamespaceLockFailures, namespace_lock_failures);
        drain!(RuntimeLeaseWaitNs, runtime_lease_wait_ns);
        drain!(RuntimeLeaseWaits, runtime_lease_waits);
        drain!(RuntimeLeaseContentions, runtime_lease_contentions);
        drain!(RuntimeLeaseFailures, runtime_lease_failures);
        drain!(SourceBytesRead, source_bytes_read);

        macro_rules! gauge {
            ($gauge:ident, $field:ident) => {
                metrics
                    .$field
                    .set(self.gauges[WorkerGauge::$gauge as usize].load(Ordering::Acquire));
            };
        }
        gauge!(CheckpointWalSize, checkpoint_wal_size);
        gauge!(CheckpointWalTransactions, checkpoint_wal_transactions);
        gauge!(FilesTracked, files_tracked);
        gauge!(FilesPending, files_pending);
        gauge!(FilesOpen, files_open);
        gauge!(FilesDescriptorBlocked, files_descriptor_blocked);
        gauge!(FilesRemovedWaiting, files_removed_waiting);
        gauge!(FilesQuarantined, files_quarantined);
        gauge!(CandidateOldestAgeNs, candidate_oldest_age_ns);
        gauge!(
            CandidateOverflowPersistenceNs,
            candidate_overflow_persistence_ns
        );
        gauge!(TrackedSaturated, tracked_saturated);
        gauge!(DescriptorSaturated, descriptor_saturated);
        gauge!(PinnedRotatedHandles, pinned_rotated_handles);
        gauge!(PinnedRotatedOldestAgeNs, pinned_rotated_oldest_age_ns);
        gauge!(PartialBytesPending, partial_bytes_pending);
    }

    #[cfg(test)]
    pub(super) fn take_counter_for_test(&self, counter: WorkerCounter) -> u64 {
        self.counters[counter as usize].swap(0, Ordering::AcqRel)
    }

    #[cfg(test)]
    pub(super) fn counter_for_test(&self, counter: WorkerCounter) -> u64 {
        self.counters[counter as usize].load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(super) fn gauge_for_test(&self, gauge: WorkerGauge) -> u64 {
        self.gauges[gauge as usize].load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(super) fn peak_partial_bytes_pending_for_test(&self) -> u64 {
        self.peak_partial_bytes_pending.load(Ordering::Acquire)
    }
}

/// Adds a delta without allowing telemetry arithmetic to panic or wrap.
pub(super) fn add_counter_saturating(counter: &mut Counter<u64>, value: u64) {
    let next = counter.get().saturating_add(value);
    counter.reset();
    counter.add(next);
}

/// Converts a duration to a bounded telemetry value.
#[must_use]
pub(super) fn duration_ns(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

/// Takes terminal snapshots after importing every final worker delta.
pub(super) fn terminal_snapshots(
    metrics: &mut Option<MetricSet<FilelogReceiverMetrics>>,
    bridge: &WorkerTelemetryBridge,
) -> Vec<MetricSetSnapshot> {
    let Some(metrics) = metrics.as_mut() else {
        return Vec::new();
    };
    bridge.drain_into(metrics);
    metrics.terminal_snapshots()
}

/// Fixed health-event categories. No source-derived key can allocate a slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(usize)]
pub(super) enum HealthEventCategory {
    Backpressure,
    Retry,
    NoRoute,
    ExplicitLoss,
    Completion,
    DrainTimeout,
    Cleanup,
    Terminal,
    Scan,
    CandidateOverflow,
    Saturation,
    CheckpointCommit,
    CheckpointMaintenance,
    Lease,
    PatternFallback,
    Decode,
    Truncation,
    Quarantine,
    Rotation,
    PinnedRotation,
    AdvisoryPath,
    Compatibility,
    Partial,
    Source,
}

impl HealthEventCategory {
    const COUNT: usize = Self::Source as usize + 1;

    /// Stable bounded category value used by suppression summaries.
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Backpressure => "backpressure",
            Self::Retry => "retry",
            Self::NoRoute => "no_route",
            Self::ExplicitLoss => "explicit_loss",
            Self::Completion => "completion",
            Self::DrainTimeout => "drain_timeout",
            Self::Cleanup => "cleanup",
            Self::Terminal => "terminal",
            Self::Scan => "scan",
            Self::CandidateOverflow => "candidate_overflow",
            Self::Saturation => "saturation",
            Self::CheckpointCommit => "checkpoint_commit",
            Self::CheckpointMaintenance => "checkpoint_maintenance",
            Self::Lease => "runtime_lease",
            Self::PatternFallback => "pattern_fallback",
            Self::Decode => "decode",
            Self::Truncation => "truncation",
            Self::Quarantine => "quarantine",
            Self::Rotation => "rotation",
            Self::PinnedRotation => "pinned_rotation",
            Self::AdvisoryPath => "advisory_path",
            Self::Compatibility => "compatibility",
            Self::Partial => "partial",
            Self::Source => "source",
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct HealthEventSlot {
    next_allowed: Option<Instant>,
    suppressed: u64,
    suppress_forever: bool,
}

/// Per-receiver, fixed-array event limiter with deterministic clock input.
#[derive(Debug)]
pub(super) struct HealthEventLimiter {
    interval: Duration,
    slots: [HealthEventSlot; HealthEventCategory::COUNT],
}

impl Default for HealthEventLimiter {
    fn default() -> Self {
        Self::new(HEALTH_EVENT_INTERVAL)
    }
}

impl HealthEventLimiter {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) const fn new(interval: Duration) -> Self {
        Self {
            interval,
            slots: [HealthEventSlot {
                next_allowed: None,
                suppressed: 0,
                suppress_forever: false,
            }; HealthEventCategory::COUNT],
        }
    }

    /// Returns the number suppressed since the previous emitted event.
    ///
    /// `None` means this occurrence is suppressed. Suppression counts
    /// saturate, so an event storm cannot wrap accounting.
    pub(super) fn admit(&mut self, category: HealthEventCategory, now: Instant) -> Option<u64> {
        let slot = &mut self.slots[category as usize];
        if slot.suppress_forever || slot.next_allowed.is_some_and(|deadline| now < deadline) {
            slot.suppressed = slot.suppressed.saturating_add(1);
            return None;
        }
        let suppressed = std::mem::take(&mut slot.suppressed);
        match now.checked_add(self.interval) {
            Some(deadline) => slot.next_allowed = Some(deadline),
            None => slot.suppress_forever = true,
        }
        Some(suppressed)
    }

    #[cfg(test)]
    fn suppressed(&self, category: HealthEventCategory) -> u64 {
        self.slots[category as usize].suppressed
    }
}

#[cfg(test)]
mod tests {
    use otap_df_engine::context::ControllerContext;
    use otap_df_telemetry::metrics::MetricSetHandler;
    use otap_df_telemetry::registry::TelemetryRegistryHandle;
    use otap_df_telemetry::reporter::MetricsReporter;

    use super::*;

    fn metric_value<'a>(
        snapshot: &'a MetricSetSnapshot,
        name: &str,
    ) -> &'a otap_df_telemetry::metrics::MetricValue {
        let index = snapshot
            .descriptor()
            .metrics
            .iter()
            .position(|field| field.name == name)
            .expect("metric field exists");
        &snapshot.get_metrics()[index]
    }

    fn registered() -> (TelemetryRegistryHandle, MetricSet<FilelogReceiverMetrics>) {
        let registry = TelemetryRegistryHandle::new();
        let controller = ControllerContext::new(registry.clone());
        let pipeline = controller.pipeline_context_with("group".into(), "pipeline".into(), 0, 1, 0);
        (registry, FilelogReceiverMetrics::register(&pipeline))
    }

    /// Scenario: the current provisional filelog metric set is registered in
    /// a pipeline context.
    /// Guarantees: the implementation uses `receiver.filelog` consistently
    /// without declaring that exact name a stable public contract.
    #[test]
    fn metric_set_registers_current_provisional_name() {
        let (_registry, metrics) = registered();
        assert_eq!(metrics.snapshot().descriptor().name, "receiver.filelog");
    }

    /// Scenario: worker counters are drained repeatedly around new updates.
    /// Guarantees: each delta transfers exactly once while gauges retain their
    /// current value across snapshots.
    #[test]
    fn worker_bridge_transfers_deltas_once_and_refreshes_gauges() {
        let bridge = WorkerTelemetryBridge::default();
        let (_registry, mut metrics) = registered();
        bridge.add(WorkerCounter::FilesEligible, 3);
        bridge.add(WorkerCounter::CarryOverRecords, 1);
        bridge.add(WorkerCounter::DescriptorBudgetWarnings, 1);
        bridge.add(WorkerCounter::EnvironmentalReprobes, 2);
        bridge.add(WorkerCounter::CheckpointSyncDelayNs, 11);
        bridge.add(WorkerCounter::CheckpointSyncDelayOperations, 1);
        bridge.add(WorkerCounter::CheckpointRecoveryDurationNs, 12);
        bridge.add(WorkerCounter::CheckpointRecoveryOperations, 1);
        bridge.add(WorkerCounter::DescriptorEvictions, 2);
        bridge.add(WorkerCounter::DescriptorReopenFailures, 3);
        bridge.add(WorkerCounter::PinnedRotationSaturation, 4);
        bridge.add(WorkerCounter::ReadTurns, 5);
        bridge.add(WorkerCounter::EofReprobes, 6);
        bridge.add(WorkerCounter::FramingProfileIncompatible, 7);
        bridge.add(WorkerCounter::AdvisoryPathTruncated, 8);
        bridge.add(WorkerCounter::CheckpointRecordsRemoved, 9);
        bridge.add(WorkerCounter::RecordsSplit, 10);
        bridge.set(WorkerGauge::FilesOpen, 2);
        bridge.set(WorkerGauge::CheckpointWalTransactions, 13);
        bridge.set(WorkerGauge::PinnedRotatedHandles, 2);
        bridge.set(WorkerGauge::PinnedRotatedOldestAgeNs, 14);
        bridge.drain_into(&mut metrics);
        assert_eq!(metrics.files_eligible.get(), 3);
        assert_eq!(metrics.carry_over_records.get(), 1);
        assert_eq!(metrics.descriptor_budget_warnings.get(), 1);
        assert_eq!(metrics.environmental_reprobes.get(), 2);
        assert_eq!(metrics.checkpoint_sync_delay_ns.get(), 11);
        assert_eq!(metrics.checkpoint_sync_delay_operations.get(), 1);
        assert_eq!(metrics.checkpoint_recovery_duration_ns.get(), 12);
        assert_eq!(metrics.checkpoint_recovery_operations.get(), 1);
        assert_eq!(metrics.descriptor_evictions.get(), 2);
        assert_eq!(metrics.descriptor_reopen_failures.get(), 3);
        assert_eq!(metrics.pinned_rotation_saturation.get(), 4);
        assert_eq!(metrics.read_turns.get(), 5);
        assert_eq!(metrics.eof_reprobes.get(), 6);
        assert_eq!(metrics.framing_profile_incompatible.get(), 7);
        assert_eq!(metrics.advisory_path_truncated.get(), 8);
        assert_eq!(metrics.checkpoint_records_removed.get(), 9);
        assert_eq!(metrics.records_split.get(), 10);
        assert_eq!(metrics.files_open.get(), 2);
        assert_eq!(metrics.checkpoint_wal_transactions.get(), 13);
        assert_eq!(metrics.pinned_rotated_handles.get(), 2);
        assert_eq!(metrics.pinned_rotated_oldest_age_ns.get(), 14);

        metrics.clear_values();
        bridge.drain_into(&mut metrics);
        assert_eq!(metrics.files_eligible.get(), 0);
        assert_eq!(metrics.carry_over_records.get(), 0);
        assert_eq!(metrics.descriptor_budget_warnings.get(), 0);
        assert_eq!(metrics.environmental_reprobes.get(), 0);
        assert_eq!(metrics.checkpoint_sync_delay_ns.get(), 0);
        assert_eq!(metrics.descriptor_evictions.get(), 0);
        assert_eq!(metrics.read_turns.get(), 0);
        assert_eq!(metrics.checkpoint_records_removed.get(), 0);
        assert_eq!(metrics.files_open.get(), 2);
        assert_eq!(metrics.checkpoint_wal_transactions.get(), 13);
        assert_eq!(metrics.pinned_rotated_handles.get(), 2);
        assert_eq!(metrics.pinned_rotated_oldest_age_ns.get(), 14);

        bridge.add(WorkerCounter::FilesEligible, 4);
        bridge.drain_into(&mut metrics);
        assert_eq!(metrics.files_eligible.get(), 4);
    }

    /// Scenario: diagnostic counters receive `u64::MAX` followed by another
    /// update while a current gauge is replaced from `u64::MAX` to zero.
    /// Guarantees: counters saturate without panic or wrap, while gauges store
    /// exact current values rather than applying counter-style saturation.
    #[test]
    fn counters_saturate_but_current_gauges_replace_exactly() {
        let bridge = WorkerTelemetryBridge::default();
        bridge.add(WorkerCounter::ReadTurns, u64::MAX);
        bridge.add(WorkerCounter::ReadTurns, 1);
        assert_eq!(bridge.counter_for_test(WorkerCounter::ReadTurns), u64::MAX);

        bridge.set(WorkerGauge::PinnedRotatedHandles, u64::MAX);
        assert_eq!(
            bridge.gauge_for_test(WorkerGauge::PinnedRotatedHandles),
            u64::MAX
        );
        bridge.set(WorkerGauge::PinnedRotatedHandles, 0);
        assert_eq!(bridge.gauge_for_test(WorkerGauge::PinnedRotatedHandles), 0);

        let mut metric = Counter::<u64>::default();
        add_counter_saturating(&mut metric, u64::MAX);
        add_counter_saturating(&mut metric, 1);
        assert_eq!(metric.get(), u64::MAX);
    }

    /// Scenario: a metrics reporter is full while worker deltas continue.
    /// Guarantees: deferred reporting keeps the hot-set delta, and a later
    /// refresh adds new worker values without loss or double counting.
    #[test]
    fn reporter_backpressure_does_not_lose_worker_counters() {
        let bridge = WorkerTelemetryBridge::default();
        let (_registry, mut metrics) = registered();
        let (receiver, mut reporter) = MetricsReporter::create_new_and_receiver(1);

        bridge.add(WorkerCounter::FilesEligible, 2);
        bridge.drain_into(&mut metrics);
        reporter.report(&mut metrics).unwrap();
        bridge.add(WorkerCounter::FilesEligible, 3);
        bridge.drain_into(&mut metrics);
        assert_eq!(metrics.files_eligible.get(), 3);
        reporter.report(&mut metrics).unwrap();
        assert_eq!(metrics.files_eligible.get(), 3);

        let first = receiver.recv().unwrap();
        assert_eq!(
            metric_value(&first, "files.eligible"),
            &otap_df_telemetry::metrics::MetricValue::U64(2)
        );
        reporter.report(&mut metrics).unwrap();
        let second = receiver.recv().unwrap();
        assert_eq!(
            metric_value(&second, "files.eligible"),
            &otap_df_telemetry::metrics::MetricValue::U64(3)
        );
        assert_eq!(metrics.files_eligible.get(), 0);
    }

    /// Scenario: the telemetry reporter receiver disconnects while source
    /// counters continue to arrive through the fixed bridge.
    /// Guarantees: reporting failure leaves the hot delta intact and never
    /// blocks or feeds failure back into file reading and Ack handling.
    #[test]
    fn reporter_disconnect_retains_metrics_without_blocking_source() {
        let bridge = WorkerTelemetryBridge::default();
        let (_registry, mut metrics) = registered();
        let (receiver, mut reporter) = MetricsReporter::create_new_and_receiver(1);
        drop(receiver);

        bridge.add(WorkerCounter::FilesEligible, 2);
        bridge.drain_into(&mut metrics);
        assert!(reporter.report(&mut metrics).is_err());
        assert_eq!(metrics.files_eligible.get(), 2);
        bridge.add(WorkerCounter::FilesEligible, 3);
        bridge.drain_into(&mut metrics);
        assert_eq!(metrics.files_eligible.get(), 5);
    }

    /// Scenario: one health category repeats before and after its interval,
    /// while another category is used independently.
    /// Guarantees: categories have fixed independent slots, suppression
    /// saturates observably, and the refill returns the exact suppressed count.
    #[test]
    fn health_limiter_suppresses_and_refills_without_sleep() {
        let mut limiter = HealthEventLimiter::new(Duration::from_secs(10));
        let start = Instant::now();
        assert_eq!(
            limiter.admit(HealthEventCategory::CheckpointMaintenance, start),
            Some(0)
        );
        assert_eq!(
            limiter.admit(HealthEventCategory::CheckpointMaintenance, start),
            None
        );
        assert_eq!(
            limiter.suppressed(HealthEventCategory::CheckpointMaintenance),
            1
        );
        assert_eq!(limiter.admit(HealthEventCategory::Scan, start), Some(0));
        assert_eq!(
            limiter.admit(HealthEventCategory::PinnedRotation, start),
            Some(0)
        );
        assert_eq!(
            limiter.admit(HealthEventCategory::AdvisoryPath, start),
            Some(0)
        );
        assert_eq!(
            limiter.admit(
                HealthEventCategory::CheckpointMaintenance,
                start + Duration::from_secs(10)
            ),
            Some(1)
        );
        assert_eq!(
            limiter.suppressed(HealthEventCategory::CheckpointMaintenance),
            0
        );
    }

    /// Scenario: a terminal snapshot follows one ordinary bridge drain and
    /// then is requested a second time.
    /// Guarantees: only final deltas appear in the terminal handoff, gauges
    /// are current, and terminal ownership transfer cannot report twice.
    #[test]
    fn terminal_snapshot_drains_final_worker_values_once() {
        let bridge = WorkerTelemetryBridge::default();
        let (_registry, metrics) = registered();
        let mut metrics = Some(metrics);
        bridge.add(WorkerCounter::CopytruncateDetected, 1);
        bridge.add(WorkerCounter::ReadTurns, 4);
        bridge.set(WorkerGauge::FilesQuarantined, 2);
        bridge.set(WorkerGauge::CheckpointWalTransactions, 3);
        bridge.set(WorkerGauge::PinnedRotatedHandles, 1);
        bridge.set(WorkerGauge::PinnedRotatedOldestAgeNs, 99);

        let snapshots = terminal_snapshots(&mut metrics, &bridge);
        assert_eq!(snapshots.len(), 1);
        assert_eq!(
            metric_value(&snapshots[0], "rotation.copytruncate.detected"),
            &otap_df_telemetry::metrics::MetricValue::U64(1)
        );
        assert_eq!(
            metric_value(&snapshots[0], "reader.turns"),
            &otap_df_telemetry::metrics::MetricValue::U64(4)
        );
        assert_eq!(
            metric_value(&snapshots[0], "files.quarantined"),
            &otap_df_telemetry::metrics::MetricValue::U64(2)
        );
        assert_eq!(
            metric_value(&snapshots[0], "checkpoint.wal.transactions"),
            &otap_df_telemetry::metrics::MetricValue::U64(3)
        );
        assert_eq!(
            metric_value(&snapshots[0], "rotation.pinned_handles"),
            &otap_df_telemetry::metrics::MetricValue::U64(1)
        );
        let second = terminal_snapshots(&mut metrics, &bridge);
        assert_eq!(
            metric_value(&second[0], "rotation.copytruncate.detected"),
            &otap_df_telemetry::metrics::MetricValue::U64(0)
        );
    }

    /// Scenario: the complete filelog metric descriptor is inspected for
    /// dynamic source context.
    /// Guarantees: the fixed-cardinality set has no measurement dimensions
    /// and no metric schema position accepts source identity or arbitrary
    /// source-controlled values.
    #[test]
    fn metric_schema_has_no_source_identity_dimensions() {
        let (_registry, metrics) = registered();
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.measurement_attributes().count(), 0);
        for field in snapshot.descriptor().metrics {
            for forbidden in [
                "file.id",
                "checkpoint.id",
                "source.path",
                "regex.pattern",
                "error.message",
            ] {
                assert!(
                    !field.name.contains(forbidden),
                    "{} contains forbidden dynamic identity marker {forbidden}",
                    field.name
                );
            }
        }
    }

    /// Scenario: the metric descriptor is inspected for every Stage 8O
    /// receiver-owned semantic added to the provisional catalog.
    /// Guarantees: checkpoint, reader, rotation, framing, path, retention,
    /// and split-record semantics each have one fixed-cardinality instrument.
    #[test]
    fn metric_schema_contains_bounded_stage_8o_semantics() {
        let (_registry, metrics) = registered();
        let snapshot = metrics.snapshot();
        let names: std::collections::HashSet<_> = snapshot
            .descriptor()
            .metrics
            .iter()
            .map(|field| field.name)
            .collect();
        for expected in [
            "records.dropped_on_nack",
            "checkpoint.sync.delay.total",
            "checkpoint.sync.delay.operations",
            "checkpoint.recovery.duration.total",
            "checkpoint.recovery.operations",
            "checkpoint.wal.transactions",
            "checkpoint.records.removed",
            "files.descriptor.evictions",
            "files.descriptor.reopen_failures",
            "rotation.pinned_handles",
            "rotation.pinned_oldest.age",
            "rotation.pinned.saturation",
            "records.split",
            "framing.profile.incompatible",
            "path.advisory.truncated",
            "reader.turns",
            "reader.eof.reprobes",
        ] {
            assert!(names.contains(expected), "missing metric {expected}");
        }
    }
}
