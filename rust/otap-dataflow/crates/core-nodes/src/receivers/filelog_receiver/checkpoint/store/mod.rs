// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Durable, file-backed checkpoint store for one filelog checkpoint
//! namespace.
//!
//! This is the durable half of the checkpoint subsystem: [`super`] defines
//! and validates the version-1 bytes, and this module owns the files those
//! bytes live in -- the namespace layout, the ownership lock, generation
//! selection, recovery, WAL appends, sync policy, compaction, and
//! retention. It implements `docs/filelog-receiver.md` Appendix B
//! ("Checkpoint storage and recovery model") on top of
//! `docs/filelog-checkpoint-format.md`.
//!
//! # Threading contract
//!
//! **Every function in this module blocks and performs filesystem I/O. It
//! must only be called from the receiver's dedicated read/checkpoint OS
//! thread, never from an async task.** The engine is share-nothing and
//! thread-per-core: blocking a pipeline core on `fsync`, on a `rename`, or
//! on the bounded ownership-lock retry would stall every other node on that
//! core. The API is therefore deliberately synchronous and `&mut self`
//! based, so a store instance is owned by exactly one thread and needs no
//! internal synchronization, no `Arc`, and no lock.
//!
//! # Durability model
//!
//! A namespace directory holds a `CURRENT` marker, the snapshot/WAL pair of
//! one or more generations, and `ownership.lock`:
//!
//! - `CURRENT` names the authoritative generation. It is replaced
//!   atomically (same-directory temporary file, sync, rename, directory
//!   sync) and is never appended to.
//! - The snapshot is the recovery base for its generation; the WAL records
//!   every change made since that snapshot was written.
//! - Compaction writes and syncs a complete new generation *before*
//!   repointing `CURRENT`, and the previous generation stays on disk and
//!   fully recoverable until a later explicit cleanup removes it. Recovery
//!   after a crash at any point therefore selects either the complete old
//!   generation or the complete new one, never a mixture.
//!
//! # Fail-closed posture
//!
//! Only a structurally incomplete final WAL transaction is discarded, and
//! only exactly as `docs/filelog-checkpoint-format.md` defines it. Every
//! other integrity, bounds, ordering, version, or impossible-transition
//! failure is reported as a [`StoreError`]; the store never silently resets
//! durable progress, never guesses across an unknown version, and never
//! substitutes an empty namespace for one it could not read.
//!
//! A caller-supplied transaction is validated against the in-memory table
//! *before* any byte reaches the WAL, so an operation that could not be
//! replayed can never be persisted. The staged table transition is committed
//! only after the append and any required sync succeed. An uncertain write
//! retains fixed-size identity for the exact transaction retry. That retry
//! reopens and validates the WAL from the prior known boundary, repairs only
//! a structurally torn suffix, accepts an exact complete transaction without
//! appending it again, and retries any required sync before committing the
//! staged table transition.
//!
//! # Resource bounds
//!
//! Recovery reads a whole snapshot and a whole WAL into memory before it
//! can decode them, so each needs a size cap. Those caps are not separate
//! knobs a caller can set inconsistently: [`StoreOptions`] carries the
//! receiver's own `checkpoint.compact_after_bytes`,
//! `checkpoint.compact_after_transactions`, `limits.max_tracked_files`, and
//! `identity.fingerprint_bytes`, and
//! [`limits::StoreLimits::derive`] turns them into the worst-case sizes
//! this configuration can legally write:
//!
//! - the snapshot cap is the snapshot header and footer plus
//!   `max_tracked_files` worst-case records (a record whose fingerprint is
//!   the configured maximum, whose advisory path is the format's maximum,
//!   and which carries quarantine evidence);
//! - the WAL cap is the tighter of the complete-WAL byte threshold and the
//!   header plus the transaction threshold times one maximum transaction.
//!
//! Both write paths enforce exactly those caps: an append compacts before its
//! prospective byte or transaction count would exceed a configured threshold,
//! and a compaction whose encoded snapshot exceeds its cap is refused before
//! any byte is written, leaving the current generation authoritative. A
//! namespace can therefore never be left holding an artifact its own
//! configuration cannot read back.
//!
//! The caps travel with the configuration, not with the files: shrinking
//! `max_tracked_files`, `fingerprint_bytes`, `compact_after_bytes`, or
//! `compact_after_transactions` below what a namespace already holds makes
//! the next open fail closed rather than truncate durable state.

pub mod error;
pub mod fault;
pub(super) mod fsio;
pub mod layout;
pub mod limits;
pub mod lock;
mod os_lock;

#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::fs::File;
use std::io::Write as _;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sha2::{Digest as _, Sha256};

use super::apply::{CheckpointTable, StagedOperations};
use super::current_marker::{decode_current_marker, encode_current_marker};
use super::namespace::{CheckpointNamespace, CheckpointNamespaceError};
use super::primitives::{
    FileId, LifecycleState, NAMESPACE_ID_MAX_BYTES, REASON_CODE_RESERVED, TX_FRAME_CRC_BYTES,
    TX_HEADER_BYTES, WAL_MAX_NON_PROGRESS_OPS_PER_TX, WAL_MAX_OPS_PER_TX, WAL_MAX_TX_BODY_BYTES,
    namespace_digest, quarantine_reason_is_reserved,
};
use super::snapshot::{SnapshotRecord, decode_snapshot, decode_snapshot_header, encode_snapshot};
use super::wal::{
    ClassifyOutcome, Operation, QuarantineFile, RegisterFile, RemoveFile, ResetAfterTruncate,
    ResetQuarantineAction, ResetQuarantinedFile, Transaction, TransactionScan, UpdateFingerprint,
    UpdateMetadata, UpdateProgress, WAL_HEADER_LEN, classify_operations, decode_wal_header,
    encode_wal, scan_one_transaction,
};
use crate::receivers::filelog_receiver::config::{
    CheckpointConfig, IdentityConfig, LimitsConfig, RuntimeConfig,
};

use error::StoreError;
use fault::{FaultPlan, FaultPoint};
use fsio::{AtomicInstallMode, AtomicWriteError, AtomicWriteFaults};
use layout::{
    CURRENT_COMPACT_TEMP_FILE_NAME, CURRENT_CREATE_TEMP_FILE_NAME, CURRENT_FILE_NAME,
    INITIAL_GENERATION, OWNERSHIP_LOCK_FILE_NAME, PublicationRole, snapshot_file_name,
    temp_file_name, wal_file_name,
};
use limits::StoreLimits;
use lock::NamespaceLock;

/// Maximum number of bytes read while looking for the fixed-width `CURRENT`
/// marker. The marker is 24 bytes; this bound exists only so that a larger
/// file is rejected before it is buffered, while still letting the marker
/// decoder report the precise structural reason.
pub(super) const MARKER_READ_MAX_BYTES: u64 = 4096;

/// Default interval between ownership-lock acquisition attempts.
const DEFAULT_OWNERSHIP_RETRY_INTERVAL: Duration = Duration::from_millis(50);

/// Configuration for opening a [`CheckpointStore`].
///
/// Every field is an input the receiver's validated configuration already
/// carries. The store's read caps are *derived* from them (see
/// [`Self::limits`]) rather than being separate fields, so a caller cannot
/// combine a write threshold with a smaller read cap and leave a namespace
/// that cannot be reopened.
#[derive(Debug, Clone)]
pub struct StoreOptions {
    /// Namespace directory:
    /// `${engine.state_dir}/filelog/@v1/<encoded checkpoint.id>/`.
    pub namespace_dir: PathBuf,
    /// The exact resolved `checkpoint.id` this namespace belongs to.
    /// Administrative `remove_file` operations are validated against it.
    pub namespace_id: String,
    /// Maximum time an Ack-driven progress transaction may stay unsynced.
    /// Zero syncs every transaction. Widening this only widens the
    /// post-crash duplicate window; it never creates a loss window, because
    /// progress is only ever recorded for already-acknowledged data.
    pub sync_interval: Duration,
    /// Compact before an append would make the complete WAL, including its
    /// fixed header, exceed this many bytes.
    pub compact_after_bytes: u64,
    /// Compact once the live WAL reaches this many transactions.
    pub compact_after_transactions: u32,
    /// Bounded wait for the namespace ownership lock.
    pub ownership_timeout: Duration,
    /// Interval between ownership-lock attempts within that bound.
    pub ownership_retry_interval: Duration,
    /// The receiver's `limits.max_tracked_files`: the largest number of
    /// records a snapshot may have to hold, which sizes the snapshot cap.
    pub max_tracked_files: u32,
    /// The receiver's `identity.fingerprint_bytes`: the widest fingerprint
    /// a record or operation may carry, which sizes the record, operation,
    /// and therefore WAL caps.
    pub fingerprint_bytes: u64,
}

impl StoreOptions {
    /// Options for `namespace_dir` / `namespace_id` with default durability
    /// policy: exactly the Appendix C defaults for every size and timing
    /// knob the store consumes.
    #[must_use]
    pub fn new(namespace_dir: PathBuf, namespace_id: String) -> Self {
        let checkpoint = CheckpointConfig::default();
        let limits = LimitsConfig::default();
        let identity = IdentityConfig::default();
        Self {
            namespace_dir,
            namespace_id,
            sync_interval: checkpoint.sync_interval,
            compact_after_bytes: checkpoint.compact_after_bytes,
            compact_after_transactions: checkpoint.compact_after_transactions,
            ownership_timeout: checkpoint.ownership_timeout,
            ownership_retry_interval: DEFAULT_OWNERSHIP_RETRY_INTERVAL,
            max_tracked_files: limits.max_tracked_files,
            fingerprint_bytes: identity.fingerprint_bytes,
        }
    }

    /// Default store options for a namespace derived below an engine state
    /// directory by the shared version-1 namespace contract. The engine state
    /// directory itself must already exist; the store creates and
    /// parent-syncs only its `filelog/@v1/<id>` descendants.
    pub fn from_state_dir(
        engine_state_dir: impl AsRef<Path>,
        namespace_id: &str,
    ) -> Result<Self, CheckpointNamespaceError> {
        let namespace = CheckpointNamespace::derive(engine_state_dir, namespace_id)?;
        Ok(Self::new(
            namespace.into_directory(),
            namespace_id.to_owned(),
        ))
    }

    /// Options taken from the receiver's validated configuration.
    ///
    /// This takes the whole [`RuntimeConfig`] rather than just its
    /// `checkpoint` section because the durable size bounds span three
    /// sections: `checkpoint.compact_after_bytes` sizes the WAL,
    /// `limits.max_tracked_files` and `identity.fingerprint_bytes` size the
    /// snapshot. It also carries the already-resolved namespace directory
    /// and id, so the store can never be pointed at a directory that
    /// disagrees with the id persisted in administrative removals.
    ///
    /// `RuntimeConfig` validation runs the same formulas
    /// ([`limits::StoreLimits::derive`]), so options built from a validated
    /// configuration always resolve to usable bounds.
    #[must_use]
    pub(crate) fn from_runtime_config(config: &RuntimeConfig) -> Self {
        Self {
            namespace_dir: config.checkpoint_namespace_dir.clone(),
            namespace_id: config.checkpoint_id.clone(),
            sync_interval: config.checkpoint.sync_interval,
            compact_after_bytes: config.checkpoint.compact_after_bytes,
            compact_after_transactions: config.checkpoint.compact_after_transactions,
            ownership_timeout: config.checkpoint.ownership_timeout,
            ownership_retry_interval: DEFAULT_OWNERSHIP_RETRY_INTERVAL,
            max_tracked_files: config.limits.max_tracked_files,
            fingerprint_bytes: config.identity.fingerprint_bytes,
        }
    }

    /// The worst-case durable sizes these options imply, which are also the
    /// caps the store reads and writes against.
    pub fn limits(&self) -> Result<StoreLimits, StoreError> {
        StoreLimits::derive(
            self.compact_after_bytes,
            self.compact_after_transactions,
            self.max_tracked_files,
            self.fingerprint_bytes,
        )
        .map_err(|source| StoreError::ResourceBounds {
            namespace_dir: self.namespace_dir.clone(),
            source,
        })
    }
}

/// What opening the namespace found and did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryReport {
    /// The generation that is now authoritative.
    pub generation: u64,
    /// Whether this open created the namespace's initial generation.
    pub created: bool,
    /// Records recovered from the snapshot before WAL replay.
    pub snapshot_records: usize,
    /// WAL transactions replayed on top of the snapshot.
    pub transactions_replayed: usize,
    /// Bytes discarded from a structurally incomplete final WAL
    /// transaction. Any other WAL damage fails recovery closed instead.
    pub torn_tail_bytes: usize,
    /// Abandoned first-publication or proposed-generation artifacts removed
    /// during recovery.
    pub removed_temp_files: usize,
    /// Generations still on disk that are no longer authoritative. They stay
    /// recoverable until [`CheckpointStore::cleanup_retired_generations`]
    /// removes them.
    pub retired_generations: Vec<u64>,
    /// Time spent recovering after namespace ownership was acquired.
    pub duration_ns: u64,
}

/// The result of appending one transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppendOutcome {
    /// The sequence assigned to the transaction.
    pub sequence: u64,
    /// How many operations it carried.
    pub operations: usize,
    /// How many bytes it added to the WAL.
    pub bytes: u64,
    /// Whether the WAL was synced as part of this append.
    pub synced: bool,
    /// Whether a compaction threshold is now met.
    pub compaction_due: bool,
}

/// Result of a cancellation-aware grouped append.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AtomicGroupAppendOutcome {
    /// Every preflighted atomic group was appended.
    Completed(Vec<AppendOutcome>),
    /// Cancellation stopped the append before its next transaction.
    ///
    /// The listed transaction prefix is already durable according to each
    /// transaction's normal persistence policy.
    Cancelled {
        /// Transactions completed before cancellation became visible.
        completed: Vec<AppendOutcome>,
    },
}

/// A preflighted sequence of transaction-bounded atomic operation groups.
///
/// `next_transaction` advances only after one complete transaction succeeds,
/// so an uncertain transaction retry resumes at the exact encoded chunk
/// instead of rebuilding an already committed prefix.
#[derive(Debug)]
pub(crate) struct AtomicGroupAppendPlan {
    operations: Vec<Operation>,
    transaction_lengths: Vec<usize>,
    next_transaction: usize,
    next_operation: usize,
}

/// Durable and in-memory accounting for one store instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreStats {
    /// The authoritative generation.
    pub generation: u64,
    /// Generations kept on disk pending cleanup.
    pub retired_generations: Vec<u64>,
    /// Records currently tracked.
    pub records: usize,
    /// Live WAL size in bytes, including its header.
    pub wal_bytes: u64,
    /// Transactions in the live WAL.
    pub wal_transactions: u64,
    /// Transactions written but not yet synced.
    pub unsynced_transactions: u64,
    /// Sequence the next transaction will use.
    pub next_sequence: u64,
    /// Number of WAL syncs performed by this store instance.
    pub syncs: u64,
    /// Current quarantined-record population.
    pub quarantined_records: usize,
    /// WAL bytes appended by this store instance, excluding generation headers.
    pub wal_bytes_appended: u64,
    /// WAL transactions appended by this store instance across compactions.
    pub transactions_appended: u64,
    /// Total measured WAL append latency in nanoseconds.
    pub persist_duration_ns: u64,
    /// Number of measured WAL append operations.
    pub persist_operations: u64,
    /// Total measured WAL sync latency in nanoseconds.
    pub sync_duration_ns: u64,
    /// Number of measured WAL sync operations.
    pub sync_operations: u64,
    /// Total delay from the first unsynced interval transaction to successful
    /// WAL sync.
    pub sync_delay_ns: u64,
    /// Successful first-unsynced-to-sync observations.
    pub sync_delay_operations: u64,
    /// Time spent acquiring the checkpoint namespace lock.
    pub namespace_lock_wait_ns: u64,
    /// Failed immediate namespace-lock attempts before acquisition.
    pub namespace_lock_contentions: u64,
    /// Durable reset-to-beginning quarantine actions.
    pub quarantine_reset_beginning: u64,
    /// Durable reset-to-end quarantine actions.
    pub quarantine_reset_end: u64,
    /// Durable keep-failed quarantine actions.
    pub quarantine_keep_failed: u64,
    /// Durable administrative removals targeting quarantined state.
    pub quarantine_removals: u64,
    /// Compactions performed synchronously before an append.
    pub preappend_compactions: u64,
    /// Time spent in successful pre-append cleanup plus compaction.
    pub preappend_compaction_duration_ns: u64,
    /// Retired generations cleaned before a required pre-append compaction.
    pub preappend_cleanup_generations: u64,
    /// Failed retired-cleanup attempts before required pre-append compaction.
    pub preappend_cleanup_failures: u64,
}

/// Whether a transaction must be synced before the call returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncPolicy {
    /// Sync before returning, whatever the configured interval is.
    Immediate,
    /// Sync when the configured interval has elapsed.
    Interval,
}

/// Fixed-size identity and accounting retained after an uncertain append.
///
/// The logical batch already owns the complete operations while the caller
/// retries. Keeping only a digest here avoids retaining a second transaction-
/// sized allocation in the store.
#[derive(Debug, Clone, Copy)]
struct PendingWalAppend {
    boundary: u64,
    wal_transactions_before: u64,
    unsynced_transactions_before: u64,
    sequence: u64,
    transaction_bytes: u64,
    wal_bytes_after: u64,
    wal_transactions_after: u64,
    unsynced_transactions_after: u64,
    next_sequence_after: u64,
    transaction_digest: [u8; 32],
    operation_count: usize,
    policy: SyncPolicy,
    requires_sync: bool,
    repair_sync_required: bool,
    started_sync_delay: bool,
}

impl PendingWalAppend {
    fn new(
        store: &CheckpointStore,
        bytes: &[u8],
        policy: SyncPolicy,
        sequence: u64,
        operation_count: usize,
    ) -> Result<Self, StoreError> {
        let transaction_bytes = u64::try_from(bytes.len())
            .map_err(|_| StoreError::AccountingOverflow { bytes: u64::MAX })?;
        let wal_bytes_after = store.wal_bytes.checked_add(transaction_bytes).ok_or(
            StoreError::AccountingOverflow {
                bytes: store.wal_bytes,
            },
        )?;
        let wal_transactions_after =
            store
                .wal_transactions
                .checked_add(1)
                .ok_or(StoreError::CounterOverflow {
                    counter: "WAL transactions",
                    value: store.wal_transactions,
                })?;
        let unsynced_transactions_after =
            store
                .unsynced_transactions
                .checked_add(1)
                .ok_or(StoreError::CounterOverflow {
                    counter: "unsynced WAL transactions",
                    value: store.unsynced_transactions,
                })?;
        let next_sequence_after = sequence
            .checked_add(1)
            .ok_or(StoreError::SequenceOverflow { sequence })?;
        Ok(Self {
            boundary: store.wal_bytes,
            wal_transactions_before: store.wal_transactions,
            unsynced_transactions_before: store.unsynced_transactions,
            sequence,
            transaction_bytes,
            wal_bytes_after,
            wal_transactions_after,
            unsynced_transactions_after,
            next_sequence_after,
            transaction_digest: Sha256::digest(bytes).into(),
            operation_count,
            policy,
            requires_sync: false,
            repair_sync_required: false,
            started_sync_delay: false,
        })
    }

    fn matches(self, other: Self) -> bool {
        self.boundary == other.boundary
            && self.wal_transactions_before == other.wal_transactions_before
            && self.unsynced_transactions_before == other.unsynced_transactions_before
            && self.sequence == other.sequence
            && self.transaction_bytes == other.transaction_bytes
            && self.wal_bytes_after == other.wal_bytes_after
            && self.wal_transactions_after == other.wal_transactions_after
            && self.unsynced_transactions_after == other.unsynced_transactions_after
            && self.next_sequence_after == other.next_sequence_after
            && self.transaction_digest == other.transaction_digest
            && self.operation_count == other.operation_count
            && self.policy == other.policy
    }
}

enum PendingAppendResolution {
    RetryWrite,
    Complete(AppendOutcome),
}

/// Classifies a transaction's durability requirement from its operations.
///
/// Registration, truncate reset, quarantine, quarantine reset, and removal
/// must be durable before their effect is observable: the receiver must not
/// read a file whose registration could be lost, must not read a
/// replacement stream before the epoch change is durable, must not report a
/// quarantine that could vanish, and must not lose an audited administrative
/// action. Fingerprint and metadata changes affect restart matching and
/// retention, so they are also immediate. Only Ack-driven progress may ride
/// the configured sync interval, widening the duplicate window and never
/// the loss window.
fn sync_policy_for(operations: &[Operation]) -> SyncPolicy {
    for operation in operations {
        match operation {
            Operation::RegisterFile(_)
            | Operation::ResetAfterTruncate(_)
            | Operation::QuarantineFile(_)
            | Operation::ResetQuarantinedFile(_)
            | Operation::RemoveFile(_) => return SyncPolicy::Immediate,
            Operation::UpdateProgress(_) => {}
            Operation::UpdateFingerprint(_) | Operation::UpdateMetadata(_) => {
                return SyncPolicy::Immediate;
            }
        }
    }
    SyncPolicy::Interval
}

fn pack_atomic_groups(
    groups: Vec<Vec<Operation>>,
) -> Result<(Vec<Operation>, Vec<usize>), StoreError> {
    if groups.is_empty() {
        return Err(StoreError::EmptyTransaction);
    }
    for group in &groups {
        reject_reserved_reason_codes(group)?;
    }

    // Every atomic group passed through this path is non-progress (never
    // contains `update_progress`; progress commits always go through the
    // unsplit, class-checked `append` path instead), so packing is bounded
    // by the non-progress class's operation-count maximum and the format's
    // hard 16 MiB transaction-body cap. Both bounds are enforced by byte-
    // and count-aware packing before a single chunk is appended.
    let max_ops = WAL_MAX_NON_PROGRESS_OPS_PER_TX as usize;
    let operation_count = groups.iter().try_fold(0usize, |count, group| {
        count
            .checked_add(group.len())
            .ok_or(StoreError::CounterOverflow {
                counter: "grouped checkpoint operations",
                value: count as u64,
            })
    })?;
    let mut operations = Vec::with_capacity(operation_count);
    let mut transaction_lengths = Vec::new();
    let mut current_len = 0usize;
    let mut current_bytes: u64 = 0;
    for group in groups {
        if group.is_empty() {
            return Err(StoreError::EmptyTransaction);
        }
        let group_bytes = encoded_operations_bytes(&group)?;
        if group.len() > max_ops || group_bytes > WAL_MAX_TX_BODY_BYTES {
            return Err(StoreError::TransactionTooLarge {
                operations: group.len(),
                max: WAL_MAX_NON_PROGRESS_OPS_PER_TX,
            });
        }
        let exceeds_count = current_len + group.len() > max_ops;
        let exceeds_bytes = current_bytes
            .checked_add(group_bytes)
            .is_none_or(|total| total > WAL_MAX_TX_BODY_BYTES);
        if current_len > 0 && (exceeds_count || exceeds_bytes) {
            transaction_lengths.push(current_len);
            current_len = 0;
            current_bytes = 0;
        }
        current_len += group.len();
        current_bytes += group_bytes;
        operations.extend(group);
    }
    if current_len != 0 {
        transaction_lengths.push(current_len);
    }
    Ok((operations, transaction_lengths))
}

/// The total encoded operation-frame bytes `operations` would contribute to
/// a transaction body, computed by actually encoding each operation so a
/// chunking decision never trusts an estimate the codec itself would
/// disagree with.
fn encoded_operations_bytes(operations: &[Operation]) -> Result<u64, StoreError> {
    let mut total: u64 = 0;
    for operation in operations {
        let encoded_len = operation
            .encode()
            .map_err(|source| StoreError::Encode {
                artifact: "WAL operation",
                generation: 0,
                source,
            })?
            .len() as u64;
        total = total
            .checked_add(encoded_len)
            .ok_or(StoreError::CounterOverflow {
                counter: "grouped checkpoint operation bytes",
                value: total,
            })?;
    }
    Ok(total)
}

/// Splits a flat, always-non-progress operation list into chunk lengths,
/// each respecting both `WAL_MAX_NON_PROGRESS_OPS_PER_TX` and the hard
/// `WAL_MAX_TX_BODY_BYTES` cap, computed from each operation's actual
/// encoded size.
fn chunk_non_progress_operations(operations: &[Operation]) -> Result<Vec<usize>, StoreError> {
    let max_ops = WAL_MAX_NON_PROGRESS_OPS_PER_TX as usize;
    let mut lengths = Vec::new();
    let mut current_len = 0usize;
    let mut current_bytes: u64 = 0;
    for operation in operations {
        let op_bytes = encoded_operations_bytes(std::slice::from_ref(operation))?;
        let exceeds_count = current_len + 1 > max_ops;
        let exceeds_bytes = current_bytes
            .checked_add(op_bytes)
            .is_none_or(|total| total > WAL_MAX_TX_BODY_BYTES);
        if current_len > 0 && (exceeds_count || exceeds_bytes) {
            lengths.push(current_len);
            current_len = 0;
            current_bytes = 0;
        }
        current_len += 1;
        current_bytes += op_bytes;
    }
    if current_len > 0 {
        lengths.push(current_len);
    }
    Ok(lengths)
}

/// Refuses `0x0000`, which is reserved for every reason field.
fn reject_reserved_reason_code(field: &'static str, reason_code: u16) -> Result<(), StoreError> {
    if reason_code == REASON_CODE_RESERVED {
        return Err(StoreError::ReservedReasonCode { field, reason_code });
    }
    Ok(())
}

/// Refuses every quarantine reason reserved from version-1 encoder output.
fn reject_reserved_quarantine_reason_code(
    field: &'static str,
    reason_code: u16,
) -> Result<(), StoreError> {
    if quarantine_reason_is_reserved(reason_code) {
        return Err(StoreError::ReservedReasonCode { field, reason_code });
    }
    Ok(())
}

/// Applies [`reject_reserved_reason_code`] to every operation in a
/// transaction.
///
/// Called from [`CheckpointStore::append`], which every public append path
/// funnels through, so no caller-facing entry point -- including the
/// caller-constructed [`CheckpointStore::quarantine_files`],
/// [`CheckpointStore::remove_quarantined_file`], and raw
/// [`CheckpointStore::append`] -- can persist a reserved reason code.
fn reject_reserved_reason_codes(operations: &[Operation]) -> Result<(), StoreError> {
    for operation in operations {
        match operation {
            Operation::QuarantineFile(op) => {
                reject_reserved_quarantine_reason_code(
                    "quarantine_file.reason_code",
                    op.reason_code,
                )?;
            }
            Operation::RemoveFile(op) => {
                reject_reserved_reason_code("remove_file.removal_reason", op.removal_reason)?;
            }
            Operation::RegisterFile(_)
            | Operation::UpdateProgress(_)
            | Operation::ResetAfterTruncate(_)
            | Operation::UpdateFingerprint(_)
            | Operation::UpdateMetadata(_)
            | Operation::ResetQuarantinedFile(_) => {}
        }
    }
    Ok(())
}

/// One namespace's durable checkpoint state.
///
/// See the [module documentation](self) for the threading contract: this
/// type blocks and must live on the dedicated read/checkpoint thread.
#[derive(Debug)]
pub struct CheckpointStore {
    namespace_dir: PathBuf,
    namespace_id: String,
    sync_interval: Duration,
    compact_after_bytes: u64,
    compact_after_transactions: u32,
    max_tracked_files: u32,
    fingerprint_bytes: u64,
    limits: StoreLimits,
    /// Held for the store's lifetime; dropping it releases the namespace.
    _lock: NamespaceLock,
    /// Stable namespace-directory identity retained for every later mutation.
    _namespace: fsio::DirectoryPathBinding,
    table: CheckpointTable,
    generation: u64,
    retired_generations: Vec<u64>,
    wal: File,
    wal_path: PathBuf,
    wal_bytes: u64,
    wal_transactions: u64,
    next_sequence: u64,
    unsynced_transactions: u64,
    last_sync: Instant,
    first_unsynced_at: Option<Instant>,
    syncs: u64,
    wal_bytes_appended: u64,
    transactions_appended: u64,
    persist_duration_ns: u64,
    persist_operations: u64,
    sync_duration_ns: u64,
    sync_operations: u64,
    sync_delay_ns: u64,
    sync_delay_operations: u64,
    quarantine_reset_beginning: u64,
    quarantine_reset_end: u64,
    quarantine_keep_failed: u64,
    quarantine_removals: u64,
    preappend_compactions: u64,
    preappend_compaction_duration_ns: u64,
    preappend_cleanup_generations: u64,
    preappend_cleanup_failures: u64,
    faults: FaultPlan,
    pending_wal_append: Option<PendingWalAppend>,
    unusable: Option<&'static str>,
    recovery: RecoveryReport,
}

impl CheckpointStore {
    /// Opens (creating if necessary) the checkpoint namespace described by
    /// `options`, taking exclusive ownership of it and recovering its
    /// durable state.
    pub fn open(options: StoreOptions) -> Result<Self, StoreError> {
        Self::open_cancellable(options, || false)
            .map(|store| store.expect("non-cancellable checkpoint open cannot be cancelled"))
    }

    /// Opens and recovers a checkpoint namespace while allowing lifecycle
    /// cancellation to abandon lock acquisition and recovery between
    /// durable stages.
    pub(crate) fn open_cancellable(
        options: StoreOptions,
        mut cancelled: impl FnMut() -> bool,
    ) -> Result<Option<Self>, StoreError> {
        Self::open_inner(options, FaultPlan::disabled(), &mut cancelled)
    }

    /// Opens the namespace with `point` armed, so the first time the durable
    /// sequence reaches that boundary it fails. Test-only: production code
    /// cannot construct an armed [`FaultPlan`].
    #[cfg(test)]
    pub(crate) fn open_with_fault(
        options: StoreOptions,
        point: FaultPoint,
    ) -> Result<Self, StoreError> {
        Self::open_inner(options, FaultPlan::armed(point), &mut || false)
            .map(|store| store.expect("non-cancellable checkpoint open cannot be cancelled"))
    }

    /// Opens the namespace with `point` armed after the requested number of
    /// matching boundaries have succeeded. Test-only support for failures
    /// between WAL chunks in one logical batch.
    #[cfg(test)]
    pub(crate) fn open_with_fault_after(
        options: StoreOptions,
        point: FaultPoint,
        matching_occurrences_to_skip: usize,
    ) -> Result<Self, StoreError> {
        Self::open_inner(
            options,
            FaultPlan::armed_after(point, matching_occurrences_to_skip),
            &mut || false,
        )
        .map(|store| store.expect("non-cancellable checkpoint open cannot be cancelled"))
    }

    /// Converts an already locked, read-only administration authority into
    /// the ordinary append-capable store without reacquiring the namespace
    /// lock or reselecting another generation.
    pub(super) fn from_admin_session(
        options: StoreOptions,
        limits: StoreLimits,
        lock: &mut Option<NamespaceLock>,
        generation: u64,
        loaded: &mut Option<LoadedGeneration>,
        retired_generations: Vec<u64>,
        faults: FaultPlan,
    ) -> Result<Self, StoreError> {
        let namespace_binding = fsio::DirectoryPathBinding::open_canonical(
            &options.namespace_dir,
            "bind an append-capable administration namespace",
        )?;
        let wal_path = options.namespace_dir.join(wal_file_name(generation));
        let loaded_ref = loaded
            .as_mut()
            .expect("an append-capable admin transition retains its loaded authority");
        let wal_transactions = u64::try_from(loaded_ref.transactions_replayed).map_err(|_| {
            StoreError::CounterOverflow {
                counter: "WAL transaction",
                value: u64::MAX,
            }
        })?;
        let repaired_torn_tail_bytes = loaded_ref.torn_tail_bytes;
        if repaired_torn_tail_bytes > 0 {
            let mut cancelled = || false;
            fsio::truncate_file_cancellable(&wal_path, loaded_ref.wal_valid_len, &mut cancelled)?
                .expect("a non-cancellable admin WAL repair cannot be cancelled");
            loaded_ref.torn_tail_bytes = 0;
        }
        let mut cancelled = || false;
        let wal = fsio::open_for_append_cancellable(&wal_path, &mut cancelled)?
            .expect("a non-cancellable admin WAL open cannot be cancelled");

        let StoreOptions {
            namespace_dir,
            namespace_id,
            sync_interval,
            compact_after_bytes,
            compact_after_transactions,
            ownership_timeout: _,
            ownership_retry_interval: _,
            max_tracked_files,
            fingerprint_bytes,
        } = options;
        let lock = lock
            .take()
            .expect("an append-capable admin transition retains its namespace lock");
        let loaded = loaded
            .take()
            .expect("an append-capable admin transition retains its loaded authority");
        let recovery = RecoveryReport {
            generation,
            created: false,
            snapshot_records: loaded.snapshot_records,
            transactions_replayed: loaded.transactions_replayed,
            torn_tail_bytes: repaired_torn_tail_bytes,
            removed_temp_files: 0,
            retired_generations: retired_generations.clone(),
            duration_ns: 0,
        };

        Ok(Self {
            namespace_dir,
            namespace_id,
            sync_interval,
            compact_after_bytes,
            compact_after_transactions,
            max_tracked_files,
            fingerprint_bytes,
            limits,
            _lock: lock,
            _namespace: namespace_binding,
            table: loaded.table,
            generation,
            retired_generations,
            wal,
            wal_path,
            wal_bytes: loaded.wal_valid_len,
            wal_transactions,
            next_sequence: loaded.next_sequence,
            unsynced_transactions: 0,
            last_sync: Instant::now(),
            first_unsynced_at: None,
            syncs: 0,
            wal_bytes_appended: 0,
            transactions_appended: 0,
            persist_duration_ns: 0,
            persist_operations: 0,
            sync_duration_ns: 0,
            sync_operations: 0,
            sync_delay_ns: 0,
            sync_delay_operations: 0,
            quarantine_reset_beginning: 0,
            quarantine_reset_end: 0,
            quarantine_keep_failed: 0,
            quarantine_removals: 0,
            preappend_compactions: 0,
            preappend_compaction_duration_ns: 0,
            preappend_cleanup_generations: 0,
            preappend_cleanup_failures: 0,
            faults,
            pending_wal_append: None,
            unusable: None,
            recovery,
        })
    }

    fn open_inner(
        options: StoreOptions,
        mut faults: FaultPlan,
        cancelled: &mut impl FnMut() -> bool,
    ) -> Result<Option<Self>, StoreError> {
        // Resolved before anything is created or locked: options whose
        // worst-case artifacts could not be read back must not be used to
        // take ownership of a namespace at all.
        let limits = options.limits()?;
        let StoreOptions {
            mut namespace_dir,
            namespace_id,
            sync_interval,
            compact_after_bytes,
            compact_after_transactions,
            ownership_timeout,
            ownership_retry_interval,
            max_tracked_files,
            fingerprint_bytes,
        } = options;

        // Checked once, at open, rather than at the first administrative
        // operation: `namespace_id` is persisted verbatim by an
        // administrative `remove_file` and is compared against it on replay,
        // so a namespace this store could never name correctly must not be
        // opened at all.
        if namespace_id.is_empty() {
            return Err(StoreError::InvalidNamespaceId {
                namespace_dir,
                reason: "it is empty",
            });
        }
        if namespace_id.len() > NAMESPACE_ID_MAX_BYTES {
            return Err(StoreError::InvalidNamespaceId {
                namespace_dir,
                reason: "it is longer than the format's namespace_id maximum",
            });
        }
        if cancelled() {
            return Ok(None);
        }
        let Some(prepared_namespace) =
            fsio::create_namespace_dir_cancellable(&namespace_dir, &mut faults, &mut *cancelled)?
        else {
            return Ok(None);
        };
        namespace_dir = prepared_namespace.namespace_path().to_path_buf();
        let Some(lock) = NamespaceLock::acquire_cancellable(
            &namespace_dir,
            ownership_timeout,
            ownership_retry_interval,
            &mut *cancelled,
        )?
        else {
            return Ok(None);
        };
        let recovery_started = Instant::now();
        prepared_namespace.verify("revalidate the namespace chain after lock acquisition")?;
        let marker_path = namespace_dir.join(CURRENT_FILE_NAME);
        prepared_namespace.verify("revalidate the namespace chain before reading authority")?;
        let Some(marker_bytes) = fsio::read_file_bounded_cancellable(
            &marker_path,
            "CURRENT marker",
            MARKER_READ_MAX_BYTES,
            &mut *cancelled,
        )?
        else {
            return Ok(None);
        };
        let marker_present = marker_bytes.is_some();
        let mut removed_temp_files = 0usize;
        let selection = match marker_bytes {
            Some(bytes) => {
                let generation =
                    decode_current_marker(&bytes).map_err(|source| StoreError::Decode {
                        artifact: "CURRENT marker",
                        path: marker_path.clone(),
                        source,
                    })?;
                Selection {
                    generation,
                    created: false,
                }
            }
            None => {
                let Some((selection, removed)) = Self::select_without_marker(
                    &namespace_dir,
                    &namespace_id,
                    &mut faults,
                    &limits,
                    &mut *cancelled,
                )?
                else {
                    return Ok(None);
                };
                removed_temp_files = removed;
                selection
            }
        };
        if cancelled() {
            return Ok(None);
        }
        prepared_namespace.verify("revalidate the namespace chain after authority selection")?;

        let generation = selection.generation;
        let wal_path = namespace_dir.join(wal_file_name(generation));
        let Some(loaded) = Self::load_generation(
            &namespace_dir,
            generation,
            &namespace_id,
            &limits,
            max_tracked_files,
            fingerprint_bytes,
            fsio::ArtifactReadMode::RepairPermissions,
            &mut *cancelled,
        )?
        else {
            return Ok(None);
        };
        if loaded.torn_tail_bytes > 0 {
            if cancelled() {
                return Ok(None);
            }
            faults.check(FaultPoint::BeforeTornTailTruncate)?;
            let Some(()) =
                fsio::truncate_file_cancellable(&wal_path, loaded.wal_valid_len, &mut *cancelled)?
            else {
                return Ok(None);
            };
            faults.check(FaultPoint::AfterTornTailTruncate)?;
        }
        if marker_present {
            let Some(removed) = Self::cleanup_abandoned_publication_artifacts(
                &namespace_dir,
                generation,
                &mut *cancelled,
            )?
            else {
                return Ok(None);
            };
            removed_temp_files += removed;
        }
        if cancelled() {
            return Ok(None);
        }
        prepared_namespace.verify("revalidate the namespace chain after recovery cleanup")?;
        let Some(wal) = fsio::open_for_append_cancellable(&wal_path, &mut *cancelled)? else {
            return Ok(None);
        };

        let Some(generations) = layout::scan_generations(&namespace_dir, &mut *cancelled)? else {
            return Ok(None);
        };
        if generations.keys().any(|found| *found > generation) {
            return Err(StoreError::AuthorityMissingOrAmbiguous {
                dir: namespace_dir,
                reason: "generation artifacts newer than CURRENT remain outside the one cleaned proposal",
            });
        }
        let retired_generations: Vec<u64> = generations
            .into_keys()
            .filter(|found| *found < generation)
            .collect();
        if cancelled() {
            return Ok(None);
        }

        let recovery = RecoveryReport {
            generation,
            created: selection.created,
            snapshot_records: loaded.snapshot_records,
            transactions_replayed: loaded.transactions_replayed,
            torn_tail_bytes: loaded.torn_tail_bytes,
            removed_temp_files,
            retired_generations: retired_generations.clone(),
            duration_ns: duration_nanos(recovery_started.elapsed()),
        };

        Ok(Some(Self {
            namespace_dir,
            namespace_id,
            sync_interval,
            compact_after_bytes,
            compact_after_transactions,
            max_tracked_files,
            fingerprint_bytes,
            limits,
            _lock: lock,
            _namespace: prepared_namespace.into_namespace(),
            table: loaded.table,
            generation,
            retired_generations,
            wal,
            wal_path,
            wal_bytes: loaded.wal_valid_len,
            wal_transactions: u64::try_from(loaded.transactions_replayed).map_err(|_| {
                StoreError::CounterOverflow {
                    counter: "WAL transaction",
                    value: u64::MAX,
                }
            })?,
            next_sequence: loaded.next_sequence,
            unsynced_transactions: 0,
            last_sync: Instant::now(),
            first_unsynced_at: None,
            syncs: 0,
            wal_bytes_appended: 0,
            transactions_appended: 0,
            persist_duration_ns: 0,
            persist_operations: 0,
            sync_duration_ns: 0,
            sync_operations: 0,
            sync_delay_ns: 0,
            sync_delay_operations: 0,
            quarantine_reset_beginning: 0,
            quarantine_reset_end: 0,
            quarantine_keep_failed: 0,
            quarantine_removals: 0,
            preappend_compactions: 0,
            preappend_compaction_duration_ns: 0,
            preappend_cleanup_generations: 0,
            preappend_cleanup_failures: 0,
            faults,
            pending_wal_append: None,
            unusable: None,
            recovery,
        }))
    }

    /// Rejects a configuration that is narrower than the selected durable
    /// state. Limit reductions are migrations, not ordinary reloads.
    fn validate_recovered_configuration(
        table: &CheckpointTable,
        namespace_dir: &Path,
        max_tracked_files: u32,
        fingerprint_bytes: u64,
    ) -> Result<(), StoreError> {
        if table.len() > max_tracked_files as usize {
            return Err(StoreError::RecoveredTrackedFilesExceedMaximum {
                dir: namespace_dir.to_path_buf(),
                tracked: table.len(),
                max: max_tracked_files,
            });
        }
        for (file_id, record) in table.iter() {
            if record.fingerprint.len() as u64 > fingerprint_bytes {
                return Err(StoreError::FingerprintExceedsConfiguredMaximum {
                    context: "recovered fingerprint",
                    file_id: *file_id,
                    len: record.fingerprint.len(),
                    max: fingerprint_bytes,
                });
            }
        }
        Ok(())
    }

    /// Repairs only the exact bounded interrupted-first-publication layout,
    /// then restarts generation-zero creation from an empty locked namespace.
    fn select_without_marker(
        dir: &Path,
        namespace_id: &str,
        faults: &mut FaultPlan,
        limits: &StoreLimits,
        cancelled: &mut impl FnMut() -> bool,
    ) -> Result<Option<(Selection, usize)>, StoreError> {
        let Some(stale) = Self::interrupted_initial_publication_artifacts(dir, &mut *cancelled)?
        else {
            return Ok(None);
        };
        let removed = stale.len();
        let mut artifacts = Vec::with_capacity(removed);
        for path in stale {
            if cancelled() {
                return Ok(None);
            }
            artifacts.push(fsio::CheckpointFilePathBinding::open(
                &path,
                "bind an interrupted first-publication artifact",
            )?);
        }
        for artifact in artifacts {
            artifact.remove("remove an interrupted first-publication artifact")?;
        }
        if removed > 0 {
            fsio::sync_directory(dir)?;
        }
        if let Some(parent) = dir.parent() {
            fsio::sync_directory(if parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            })?;
        }
        if cancelled() {
            return Ok(None);
        }
        if !Self::create_generation_cancellable(
            dir,
            namespace_id,
            INITIAL_GENERATION,
            &[],
            limits,
            faults,
            &mut *cancelled,
        )? {
            return Ok(None);
        }
        Ok(Some((
            Selection {
                generation: INITIAL_GENERATION,
                created: true,
            },
            removed,
        )))
    }

    fn interrupted_initial_publication_artifacts(
        dir: &Path,
        cancelled: &mut impl FnMut() -> bool,
    ) -> Result<Option<Vec<PathBuf>>, StoreError> {
        if cancelled() {
            return Ok(None);
        }
        let allowed = [
            OWNERSHIP_LOCK_FILE_NAME.to_owned(),
            snapshot_file_name(INITIAL_GENERATION),
            wal_file_name(INITIAL_GENERATION),
            temp_file_name(
                &snapshot_file_name(INITIAL_GENERATION),
                PublicationRole::Create,
            ),
            temp_file_name(&wal_file_name(INITIAL_GENERATION), PublicationRole::Create),
            CURRENT_CREATE_TEMP_FILE_NAME.to_owned(),
        ];
        let mut removable = Vec::with_capacity(allowed.len() - 1);
        for entry in std::fs::read_dir(dir).map_err(|source| StoreError::Io {
            operation: "inventory a markerless checkpoint namespace",
            path: dir.to_path_buf(),
            source,
        })? {
            if cancelled() {
                return Ok(None);
            }
            let entry = entry.map_err(|source| StoreError::Io {
                operation: "read a markerless checkpoint namespace entry",
                path: dir.to_path_buf(),
                source,
            })?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                return Err(StoreError::AuthorityMissingOrAmbiguous {
                    dir: dir.to_path_buf(),
                    reason: "a directory entry is not an exact version-1 ASCII artifact name",
                });
            };
            if !allowed.iter().any(|allowed| allowed == &name) {
                return Err(StoreError::AuthorityMissingOrAmbiguous {
                    dir: dir.to_path_buf(),
                    reason: "the artifact set is not an interrupted first publication",
                });
            }
            if name != OWNERSHIP_LOCK_FILE_NAME {
                removable.push(dir.join(name));
            }
        }
        if !dir.join(OWNERSHIP_LOCK_FILE_NAME).is_file() {
            return Err(StoreError::AuthorityMissingOrAmbiguous {
                dir: dir.to_path_buf(),
                reason: "the ownership lock is missing after exclusive acquisition",
            });
        }
        Ok(Some(removable))
    }

    fn cleanup_abandoned_publication_artifacts(
        dir: &Path,
        current_generation: u64,
        cancelled: &mut impl FnMut() -> bool,
    ) -> Result<Option<usize>, StoreError> {
        if cancelled() {
            return Ok(None);
        }
        let namespace = fsio::DirectoryPathBinding::open_canonical(
            dir,
            "bind the checkpoint namespace for publication cleanup",
        )?;
        let marker = fsio::CheckpointFilePathBinding::open(
            &dir.join(CURRENT_FILE_NAME),
            "bind CURRENT for publication cleanup",
        )?;
        Self::require_current_generation(dir, current_generation)?;
        marker.verify("revalidate CURRENT before publication cleanup")?;
        let mut names: HashSet<String> = [
            CURRENT_CREATE_TEMP_FILE_NAME.to_owned(),
            CURRENT_COMPACT_TEMP_FILE_NAME.to_owned(),
            temp_file_name(
                &snapshot_file_name(INITIAL_GENERATION),
                PublicationRole::Create,
            ),
            temp_file_name(&wal_file_name(INITIAL_GENERATION), PublicationRole::Create),
        ]
        .into_iter()
        .collect();
        if let Some(proposed) = current_generation.checked_add(1) {
            names.extend([
                snapshot_file_name(proposed),
                wal_file_name(proposed),
                temp_file_name(&snapshot_file_name(proposed), PublicationRole::Compact),
                temp_file_name(&wal_file_name(proposed), PublicationRole::Compact),
            ]);
        }
        let mut candidates = Vec::new();
        let mut recognized_temporary_count = 0usize;
        for entry in std::fs::read_dir(dir).map_err(|source| StoreError::Io {
            operation: "inventory checkpoint publication artifacts",
            path: dir.to_path_buf(),
            source,
        })? {
            let entry = entry.map_err(|source| StoreError::Io {
                operation: "read a checkpoint publication directory entry",
                path: dir.to_path_buf(),
                source,
            })?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if let Some(classification) = layout::classify_namespace_artifact(&name)
                && classification.form != layout::ArtifactForm::Final
            {
                recognized_temporary_count += 1;
                if recognized_temporary_count > layout::MAX_TEMP_FILES {
                    return Err(StoreError::TooManyTemporaryFiles {
                        dir: dir.to_path_buf(),
                        max: layout::MAX_TEMP_FILES,
                    });
                }
            }
            if names.contains(&name) {
                candidates.push(fsio::CheckpointFilePathBinding::open(
                    &entry.path(),
                    "bind an abandoned checkpoint publication artifact",
                )?);
                continue;
            }
            if let Some(canonical) = layout::canonical_artifact_name_ignoring_ascii_case(&name)
                && canonical != name
                && names.contains(&canonical)
            {
                return Err(StoreError::UnsafeFilesystemObject {
                    path: entry.path(),
                    reason: "a noncanonical case alias conflicts with a cleanup artifact",
                });
            }
        }
        let generations = layout::scan_generations(dir, &mut *cancelled)?.ok_or(
            StoreError::AuthorityMissingOrAmbiguous {
                dir: dir.to_path_buf(),
                reason: "generation inventory was cancelled during publication cleanup",
            },
        )?;
        if let Some(proposed) = current_generation.checked_add(1)
            && generations.keys().any(|generation| *generation > proposed)
        {
            return Err(StoreError::AuthorityMissingOrAmbiguous {
                dir: dir.to_path_buf(),
                reason: "more than one unpublished generation is present",
            });
        }
        if cancelled() {
            return Ok(None);
        }
        namespace.verify("revalidate the checkpoint namespace before publication cleanup")?;
        marker.verify("revalidate CURRENT before deleting publication artifacts")?;
        Self::require_current_generation(dir, current_generation)?;
        let removed = candidates.len();
        for candidate in candidates {
            namespace.verify("revalidate the checkpoint namespace during publication cleanup")?;
            marker.verify("revalidate CURRENT during publication cleanup")?;
            candidate.remove("remove an abandoned checkpoint publication artifact")?;
        }
        if removed > 0 {
            namespace.verify("revalidate the checkpoint namespace after publication cleanup")?;
            marker.verify("revalidate CURRENT after publication cleanup")?;
            Self::require_current_generation(dir, current_generation)?;
            namespace.sync("sync cleaned checkpoint publication artifacts")?;
            if cancelled() {
                return Ok(None);
            }
        }
        Ok(Some(removed))
    }

    fn require_current_generation(dir: &Path, expected: u64) -> Result<(), StoreError> {
        let path = dir.join(CURRENT_FILE_NAME);
        let bytes = fsio::read_file_bounded_cancellable(
            &path,
            "CURRENT marker",
            MARKER_READ_MAX_BYTES,
            &mut || false,
        )?
        .expect("non-cancellable CURRENT read cannot cancel")
        .ok_or_else(|| StoreError::AuthorityMissingOrAmbiguous {
            dir: dir.to_path_buf(),
            reason: "CURRENT disappeared while publication artifacts were being cleaned",
        })?;
        let found = decode_current_marker(&bytes).map_err(|source| StoreError::Decode {
            artifact: "CURRENT marker",
            path,
            source,
        })?;
        if found != expected {
            return Err(StoreError::AuthorityMissingOrAmbiguous {
                dir: dir.to_path_buf(),
                reason: "CURRENT changed while publication artifacts were being cleaned",
            });
        }
        Ok(())
    }

    /// Enforces the configured transaction bound from the transaction
    /// header's `body_len` field (offset 20 within the fixed 36-byte
    /// envelope header) before a complete frame can be decoded into
    /// operations.
    ///
    /// This is a preflight-only check against the configured
    /// `limits.max_transaction_bytes` bound (which may be smaller than the
    /// format's own hard `WAL_MAX_TX_BODY_BYTES` cap); full structural
    /// validation (magic, envelope version, flags, length complement,
    /// header CRC, and format bounds) happens afterward in
    /// `scan_one_transaction`. A buffer shorter than the fixed header is
    /// left to that function to classify as a torn tail or an error.
    fn validate_declared_transaction_size(
        bytes: &[u8],
        wal_path: &Path,
        limits: &StoreLimits,
    ) -> Result<(), StoreError> {
        if bytes.len() < TX_HEADER_BYTES {
            return Ok(());
        }
        let mut encoded_len = [0u8; size_of::<u32>()];
        encoded_len.copy_from_slice(&bytes[20..24]);
        // The declared body is surrounded by the fixed 36-byte header and
        // the trailing 4-byte frame CRC.
        let transaction_bytes = u64::from(u32::from_be_bytes(encoded_len))
            .checked_add(TX_HEADER_BYTES as u64)
            .and_then(|value| value.checked_add(TX_FRAME_CRC_BYTES as u64))
            .ok_or(StoreError::AccountingOverflow { bytes: u64::MAX })?;
        if transaction_bytes > limits.max_transaction_bytes {
            return Err(StoreError::FileTooLarge {
                artifact: "WAL transaction",
                path: wal_path.to_path_buf(),
                len: transaction_bytes,
                max: limits.max_transaction_bytes,
            });
        }
        Ok(())
    }

    /// Loads and validates one complete generation: its snapshot as the
    /// recovery base, then its WAL replayed in strict sequence order.
    ///
    /// The snapshot buffer is dropped before the WAL is read, and WAL
    /// transactions are decoded and applied one at a time. This preserves
    /// transaction atomicity without retaining every raw artifact and every
    /// decoded operation concurrently.
    pub(super) fn load_generation(
        dir: &Path,
        generation: u64,
        namespace_id: &str,
        limits: &StoreLimits,
        max_tracked_files: u32,
        fingerprint_bytes: u64,
        read_mode: fsio::ArtifactReadMode,
        cancelled: &mut impl FnMut() -> bool,
    ) -> Result<Option<LoadedGeneration>, StoreError> {
        Self::load_generation_inspecting_append(
            dir,
            generation,
            namespace_id,
            limits,
            max_tracked_files,
            fingerprint_bytes,
            read_mode,
            None,
            cancelled,
        )
        .map(|loaded| {
            loaded.map(|(loaded, observation)| {
                debug_assert!(observation.is_none());
                loaded
            })
        })
    }

    fn load_generation_inspecting_append(
        dir: &Path,
        generation: u64,
        namespace_id: &str,
        limits: &StoreLimits,
        max_tracked_files: u32,
        fingerprint_bytes: u64,
        read_mode: fsio::ArtifactReadMode,
        append_boundary: Option<u64>,
        cancelled: &mut impl FnMut() -> bool,
    ) -> Result<Option<(LoadedGeneration, Option<WalAppendObservation>)>, StoreError> {
        let snapshot_path = dir.join(snapshot_file_name(generation));
        let wal_path = dir.join(wal_file_name(generation));

        let Some(snapshot_bytes) = fsio::read_file_bounded_cancellable_with_mode(
            &snapshot_path,
            "snapshot",
            limits.max_snapshot_bytes,
            read_mode,
            &mut *cancelled,
        )?
        else {
            return Ok(None);
        };
        let Some(snapshot_bytes) = snapshot_bytes else {
            return Err(StoreError::IncompleteGeneration {
                dir: dir.to_path_buf(),
                generation,
                missing: "the snapshot file",
            });
        };
        let snapshot_header =
            decode_snapshot_header(&snapshot_bytes).map_err(|source| StoreError::Decode {
                artifact: "snapshot",
                path: snapshot_path.clone(),
                source,
            })?;
        let expected_namespace_digest = namespace_digest(namespace_id);
        if snapshot_header.namespace_digest != expected_namespace_digest {
            return Err(StoreError::NamespaceMismatch {
                artifact: "snapshot",
                path: snapshot_path.clone(),
            });
        }
        if snapshot_header.generation != generation {
            return Err(StoreError::GenerationMismatch {
                artifact: "snapshot",
                path: snapshot_path,
                expected: generation,
                found: snapshot_header.generation,
            });
        }
        if snapshot_header.record_count > max_tracked_files {
            return Err(StoreError::RecoveredTrackedFilesExceedMaximum {
                dir: dir.to_path_buf(),
                tracked: snapshot_header.record_count as usize,
                max: max_tracked_files,
            });
        }
        let snapshot =
            decode_snapshot(&snapshot_bytes, &expected_namespace_digest).map_err(|source| {
                StoreError::Decode {
                    artifact: "snapshot",
                    path: snapshot_path.clone(),
                    source,
                }
            })?;
        let snapshot_records = snapshot.records.len();
        let mut table =
            CheckpointTable::from_snapshot_records(snapshot.records).map_err(|source| {
                StoreError::Decode {
                    artifact: "snapshot records",
                    path: snapshot_path,
                    source,
                }
            })?;
        drop(snapshot_bytes);
        if cancelled() {
            return Ok(None);
        }
        Self::validate_recovered_configuration(&table, dir, max_tracked_files, fingerprint_bytes)?;
        Self::reject_recovered_reserved_reason_codes(&table, dir, generation)?;

        let Some(wal_bytes) = fsio::read_file_bounded_cancellable_with_mode(
            &wal_path,
            "WAL",
            limits.max_wal_bytes,
            read_mode,
            &mut *cancelled,
        )?
        else {
            return Ok(None);
        };
        let Some(wal_bytes) = wal_bytes else {
            return Err(StoreError::IncompleteGeneration {
                dir: dir.to_path_buf(),
                generation,
                missing: "the WAL file",
            });
        };
        let wal_header = decode_wal_header(&wal_bytes).map_err(|source| StoreError::Decode {
            artifact: "WAL",
            path: wal_path.clone(),
            source,
        })?;
        if wal_header.namespace_digest != expected_namespace_digest {
            return Err(StoreError::NamespaceMismatch {
                artifact: "WAL",
                path: wal_path.clone(),
            });
        }
        if wal_header.generation != generation {
            return Err(StoreError::GenerationMismatch {
                artifact: "WAL",
                path: wal_path.clone(),
                expected: generation,
                found: wal_header.generation,
            });
        }

        let append_boundary = append_boundary
            .map(|boundary| {
                usize::try_from(boundary).map_err(|_| StoreError::WalAppendRecoveryMismatch {
                    path: wal_path.clone(),
                    boundary,
                    reason: "the known boundary does not fit this platform's address space",
                })
            })
            .transpose()?;
        let mut append_observation = None;
        let mut cursor = WAL_HEADER_LEN;
        let mut expected_sequence = 1u64;
        let mut transactions_replayed = 0usize;
        let mut torn_tail_bytes = 0usize;
        while cursor < wal_bytes.len() {
            if cancelled() {
                return Ok(None);
            }
            let at_append_boundary = append_boundary == Some(cursor);
            if append_observation.is_some() {
                return Err(StoreError::WalAppendRecoveryMismatch {
                    path: wal_path.clone(),
                    boundary: append_boundary.unwrap_or(cursor) as u64,
                    reason: "bytes follow the one transaction being reconciled",
                });
            }
            if append_boundary.is_some_and(|boundary| cursor > boundary) {
                return Err(StoreError::WalAppendRecoveryMismatch {
                    path: wal_path.clone(),
                    boundary: append_boundary.unwrap_or(cursor) as u64,
                    reason: "the known boundary falls inside a recovered transaction",
                });
            }
            let remaining = &wal_bytes[cursor..];
            Self::validate_declared_transaction_size(remaining, &wal_path, limits)?;
            match scan_one_transaction(remaining, expected_sequence).map_err(|source| {
                StoreError::Decode {
                    artifact: "WAL",
                    path: wal_path.clone(),
                    source,
                }
            })? {
                TransactionScan::TornTail(bytes) => {
                    if at_append_boundary {
                        append_observation = Some(WalAppendObservation::Torn { bytes });
                    }
                    torn_tail_bytes = bytes;
                    break;
                }
                TransactionScan::Complete(transaction, consumed) => {
                    let recovered_transactions = u64::try_from(transactions_replayed)
                        .unwrap_or(u64::MAX)
                        .checked_add(1)
                        .ok_or(StoreError::CounterOverflow {
                            counter: "replayed WAL transaction",
                            value: u64::MAX,
                        })?;
                    if recovered_transactions > limits.max_wal_transactions {
                        return Err(StoreError::RecoveredWalTransactionsExceedMaximum {
                            path: wal_path.clone(),
                            transactions: recovered_transactions,
                            max: limits.max_wal_transactions,
                        });
                    }
                    expected_sequence =
                        expected_sequence
                            .checked_add(1)
                            .ok_or(StoreError::SequenceOverflow {
                                sequence: transaction.sequence,
                            })?;
                    if let Err(error) = Self::ensure_fingerprint_capacity_for(
                        &transaction.operations,
                        fingerprint_bytes,
                    ) {
                        return Err(match error {
                            StoreError::FingerprintExceedsConfiguredMaximum {
                                file_id,
                                len,
                                max,
                                ..
                            } => StoreError::FingerprintExceedsConfiguredMaximum {
                                context: "recovered fingerprint",
                                file_id,
                                len,
                                max,
                            },
                            other => other,
                        });
                    }
                    if let Err(error) = Self::ensure_tracked_capacity_for(
                        &table,
                        &transaction.operations,
                        max_tracked_files,
                        dir,
                    ) {
                        return Err(match error {
                            StoreError::TrackedFilesExhausted {
                                tracked,
                                registrations,
                                max,
                                ..
                            } => {
                                let tracked = tracked.checked_add(registrations).ok_or(
                                    StoreError::CounterOverflow {
                                        counter: "recovered tracked files",
                                        value: tracked as u64,
                                    },
                                )?;
                                StoreError::RecoveredTrackedFilesExceedMaximum {
                                    dir: dir.to_path_buf(),
                                    tracked,
                                    max,
                                }
                            }
                            other => other,
                        });
                    }
                    Self::reject_recovered_operation_reason_codes(
                        &transaction.operations,
                        dir,
                        generation,
                    )?;
                    table
                        .apply_transaction(&transaction, namespace_id)
                        .map_err(|source| StoreError::Apply {
                            operation: "replay the recovered checkpoint WAL",
                            path: wal_path.clone(),
                            source,
                        })?;
                    cursor =
                        cursor
                            .checked_add(consumed)
                            .ok_or(StoreError::AccountingOverflow {
                                bytes: cursor as u64,
                            })?;
                    transactions_replayed = transactions_replayed.checked_add(1).ok_or(
                        StoreError::CounterOverflow {
                            counter: "replayed WAL transaction",
                            value: u64::try_from(transactions_replayed).unwrap_or(u64::MAX),
                        },
                    )?;
                    if at_append_boundary {
                        append_observation = Some(WalAppendObservation::Complete {
                            transaction,
                            bytes: consumed,
                        });
                    }
                }
            }
        }
        if let Some(boundary) = append_boundary
            && append_observation.is_none()
        {
            if cursor == boundary && cursor == wal_bytes.len() {
                append_observation = Some(WalAppendObservation::NoWrite);
            } else {
                return Err(StoreError::WalAppendRecoveryMismatch {
                    path: wal_path.clone(),
                    boundary: boundary as u64,
                    reason: "the known boundary was not a complete recovered transaction boundary",
                });
            }
        }
        let wal_valid_len = u64::try_from(cursor)
            .map_err(|_| StoreError::AccountingOverflow { bytes: u64::MAX })?;

        if cancelled() {
            return Ok(None);
        }
        Ok(Some((
            LoadedGeneration {
                table,
                snapshot_records,
                transactions_replayed,
                torn_tail_bytes,
                wal_valid_len,
                next_sequence: expected_sequence,
            },
            append_observation,
        )))
    }

    /// Loads one generation through the ordinary bounded decoder without
    /// repairing permissions or performing any recovery mutation.
    pub(super) fn load_generation_read_only(
        dir: &Path,
        generation: u64,
        namespace_id: &str,
        limits: &StoreLimits,
        max_tracked_files: u32,
        fingerprint_bytes: u64,
    ) -> Result<LoadedGeneration, StoreError> {
        Self::load_generation(
            dir,
            generation,
            namespace_id,
            limits,
            max_tracked_files,
            fingerprint_bytes,
            fsio::ArtifactReadMode::PreserveMetadata,
            &mut || false,
        )
        .map(|loaded| {
            loaded.expect("a non-cancellable read-only generation load cannot be cancelled")
        })
    }

    /// Fails closed when recovered durable state carries the reserved
    /// reason code the format forbids an encoder from writing.
    ///
    /// Quarantine evidence is the only reason code that survives in the
    /// table -- a removal's reason leaves with the record it removed -- and
    /// compaction re-encodes that evidence verbatim into the next snapshot.
    /// Every append this store accepts is already checked, so a recovered
    /// record carrying a reserved value did not come from here; accepting it would
    /// make the store itself produce the value the format forbids, so
    /// recovery reports it rather than propagating it.
    fn reject_recovered_reserved_reason_codes(
        table: &CheckpointTable,
        dir: &Path,
        generation: u64,
    ) -> Result<(), StoreError> {
        for (file_id, record) in table.iter() {
            let reserved = record.quarantine_evidence.as_ref().and_then(|evidence| {
                quarantine_reason_is_reserved(evidence.reason_code).then_some(evidence.reason_code)
            });
            if let Some(reason_code) = reserved {
                return Err(StoreError::ReservedReasonCodeRecovered {
                    dir: dir.to_path_buf(),
                    generation,
                    file_id: *file_id,
                    field: "quarantine_file.reason_code",
                    reason_code,
                });
            }
        }
        Ok(())
    }

    fn reject_recovered_operation_reason_codes(
        operations: &[Operation],
        dir: &Path,
        generation: u64,
    ) -> Result<(), StoreError> {
        for operation in operations {
            let (file_id, field, reserved) = match operation {
                Operation::QuarantineFile(op) => (
                    op.file_id,
                    "quarantine_file.reason_code",
                    quarantine_reason_is_reserved(op.reason_code).then_some(op.reason_code),
                ),
                Operation::RemoveFile(op) => (
                    op.file_id,
                    "remove_file.removal_reason",
                    (op.removal_reason == REASON_CODE_RESERVED).then_some(op.removal_reason),
                ),
                Operation::RegisterFile(_)
                | Operation::UpdateProgress(_)
                | Operation::ResetAfterTruncate(_)
                | Operation::UpdateFingerprint(_)
                | Operation::UpdateMetadata(_)
                | Operation::ResetQuarantinedFile(_) => continue,
            };
            if let Some(reason_code) = reserved {
                return Err(StoreError::ReservedReasonCodeRecovered {
                    dir: dir.to_path_buf(),
                    generation,
                    file_id,
                    field,
                    reason_code,
                });
            }
        }
        Ok(())
    }

    /// Writes and syncs a complete generation (snapshot plus empty WAL) and
    /// then atomically makes it authoritative.
    fn create_generation_cancellable(
        dir: &Path,
        namespace_id: &str,
        generation: u64,
        records: &[SnapshotRecord],
        limits: &StoreLimits,
        faults: &mut FaultPlan,
        cancelled: &mut impl FnMut() -> bool,
    ) -> Result<bool, StoreError> {
        if !Self::stage_generation_cancellable(
            dir,
            namespace_id,
            generation,
            records,
            limits,
            PublicationRole::Create,
            faults,
            &mut *cancelled,
        )? {
            return Ok(false);
        }
        if cancelled() {
            return Ok(false);
        }
        Self::publish_marker(dir, generation, PublicationRole::Create, faults)
            .map_err(|failure| failure.error)?;
        Ok(true)
    }

    /// Steps 1-3 of Appendix B's compaction sequence: write the complete new
    /// snapshot and empty WAL, sync both files, then sync the directory that
    /// holds them. Nothing here is authoritative yet, so a failure leaves
    /// the currently selected generation untouched.
    ///
    /// The encoded snapshot is measured against this configuration's
    /// snapshot cap *before* the first byte is written, so a namespace can
    /// never be left holding a snapshot that its own recovery would refuse
    /// to read.
    pub(super) fn stage_generation(
        dir: &Path,
        namespace_id: &str,
        generation: u64,
        records: &[SnapshotRecord],
        limits: &StoreLimits,
        faults: &mut FaultPlan,
    ) -> Result<(), StoreError> {
        Self::stage_generation_cancellable(
            dir,
            namespace_id,
            generation,
            records,
            limits,
            PublicationRole::Compact,
            faults,
            &mut || false,
        )
        .map(|completed| {
            debug_assert!(completed, "non-cancellable generation staging completes");
        })
    }

    fn stage_generation_cancellable(
        dir: &Path,
        namespace_id: &str,
        generation: u64,
        records: &[SnapshotRecord],
        limits: &StoreLimits,
        role: PublicationRole,
        faults: &mut FaultPlan,
        cancelled: &mut impl FnMut() -> bool,
    ) -> Result<bool, StoreError> {
        let snapshot_bytes =
            encode_snapshot(generation, namespace_id, records).map_err(|source| {
                StoreError::Encode {
                    artifact: "snapshot",
                    generation,
                    source,
                }
            })?;
        let snapshot_len = snapshot_bytes.len() as u64;
        if snapshot_len > limits.max_snapshot_bytes {
            return Err(StoreError::SnapshotTooLarge {
                dir: dir.to_path_buf(),
                generation,
                records: records.len(),
                len: snapshot_len,
                max: limits.max_snapshot_bytes,
            });
        }
        let wal_bytes =
            encode_wal(generation, namespace_id, &[]).map_err(|source| StoreError::Encode {
                artifact: "WAL header",
                generation,
                source,
            })?;

        if cancelled() {
            return Ok(false);
        }
        let snapshot_result = fsio::write_file_atomically(
            dir,
            &snapshot_file_name(generation),
            &snapshot_bytes,
            role,
            AtomicInstallMode::NoReplace,
            faults,
            AtomicWriteFaults::SNAPSHOT,
        )
        .map_err(|failure| failure.error);
        if cancelled() {
            return Ok(false);
        }
        snapshot_result?;
        let wal_result = fsio::write_file_atomically(
            dir,
            &wal_file_name(generation),
            &wal_bytes,
            role,
            AtomicInstallMode::NoReplace,
            faults,
            AtomicWriteFaults::WAL,
        )
        .map_err(|failure| failure.error);
        if cancelled() {
            return Ok(false);
        }
        wal_result?;
        faults.check(FaultPoint::BeforeGenerationDirSync)?;
        let synced = fsio::sync_directory(dir);
        if cancelled() {
            return Ok(false);
        }
        synced?;
        faults.check(FaultPoint::AfterGenerationDirSync)?;
        Ok(true)
    }

    /// Steps 4-5 of Appendix B's compaction sequence: atomically replace
    /// `CURRENT`, then sync the marker's directory entry.
    ///
    /// The returned failure reports whether the replacement had already
    /// happened, because that is the point after which the caller's
    /// in-memory view no longer matches the authoritative generation.
    pub(super) fn publish_marker(
        dir: &Path,
        generation: u64,
        role: PublicationRole,
        faults: &mut FaultPlan,
    ) -> Result<(), AtomicWriteError> {
        let marker = encode_current_marker(generation);
        fsio::write_file_atomically(
            dir,
            CURRENT_FILE_NAME,
            &marker,
            role,
            match role {
                PublicationRole::Create => AtomicInstallMode::NoReplace,
                PublicationRole::Compact => AtomicInstallMode::Replace,
            },
            faults,
            AtomicWriteFaults::MARKER,
        )?;
        faults
            .check(FaultPoint::BeforeMarkerDirSync)
            .map_err(|error| AtomicWriteError {
                error,
                destination_may_have_changed: true,
            })?;
        fsio::sync_directory(dir).map_err(|error| AtomicWriteError {
            error,
            destination_may_have_changed: true,
        })?;
        faults
            .check(FaultPoint::AfterMarkerDirSync)
            .map_err(|error| AtomicWriteError {
                error,
                destination_may_have_changed: true,
            })
    }

    /// The namespace directory this store owns.
    #[must_use]
    pub fn namespace_dir(&self) -> &Path {
        &self.namespace_dir
    }

    /// The exact `checkpoint.id` this namespace belongs to.
    #[must_use]
    pub fn namespace_id(&self) -> &str {
        &self.namespace_id
    }

    /// The authoritative generation.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// What opening this namespace found and did.
    #[must_use]
    pub fn recovery(&self) -> &RecoveryReport {
        &self.recovery
    }

    /// The worst-case durable sizes this store's configuration implies, and
    /// therefore the caps it reads and writes against.
    #[must_use]
    pub fn limits(&self) -> StoreLimits {
        self.limits
    }

    /// The recovered, in-memory checkpoint table.
    #[must_use]
    pub fn table(&self) -> &CheckpointTable {
        &self.table
    }

    /// Durable and in-memory accounting for this store instance.
    #[must_use]
    pub fn stats(&self) -> StoreStats {
        let known_complete_pending = self
            .pending_wal_append
            .filter(|pending| pending.requires_sync);
        StoreStats {
            generation: self.generation,
            retired_generations: self.retired_generations.clone(),
            records: self.table.len(),
            wal_bytes: known_complete_pending
                .map_or(self.wal_bytes, |pending| pending.wal_bytes_after),
            wal_transactions: known_complete_pending.map_or(self.wal_transactions, |pending| {
                pending.wal_transactions_after
            }),
            unsynced_transactions: known_complete_pending
                .map_or(self.unsynced_transactions, |pending| {
                    pending.unsynced_transactions_after
                }),
            next_sequence: self.next_sequence,
            syncs: self.syncs,
            quarantined_records: self.table.quarantined_len(),
            wal_bytes_appended: self.wal_bytes_appended,
            transactions_appended: self.transactions_appended,
            persist_duration_ns: self.persist_duration_ns,
            persist_operations: self.persist_operations,
            sync_duration_ns: self.sync_duration_ns,
            sync_operations: self.sync_operations,
            sync_delay_ns: self.sync_delay_ns,
            sync_delay_operations: self.sync_delay_operations,
            namespace_lock_wait_ns: duration_nanos(self._lock.waited()),
            namespace_lock_contentions: self._lock.contentions(),
            quarantine_reset_beginning: self.quarantine_reset_beginning,
            quarantine_reset_end: self.quarantine_reset_end,
            quarantine_keep_failed: self.quarantine_keep_failed,
            quarantine_removals: self.quarantine_removals,
            preappend_compactions: self.preappend_compactions,
            preappend_compaction_duration_ns: self.preappend_compaction_duration_ns,
            preappend_cleanup_generations: self.preappend_cleanup_generations,
            preappend_cleanup_failures: self.preappend_cleanup_failures,
        }
    }

    /// Whether a compaction threshold is met.
    #[must_use]
    pub fn compaction_due(&self) -> bool {
        self.wal_bytes >= self.compact_after_bytes
            || self.wal_transactions >= u64::from(self.compact_after_transactions)
    }

    fn projected_wal_state(&self, transaction_bytes: u64) -> Result<(u64, u64), StoreError> {
        let wal_bytes = self.wal_bytes.checked_add(transaction_bytes).ok_or(
            StoreError::AccountingOverflow {
                bytes: self.wal_bytes,
            },
        )?;
        let wal_transactions =
            self.wal_transactions
                .checked_add(1)
                .ok_or(StoreError::CounterOverflow {
                    counter: "WAL transactions",
                    value: self.wal_transactions,
                })?;
        Ok((wal_bytes, wal_transactions))
    }

    fn append_requires_compaction(&self, transaction_bytes: u64) -> Result<bool, StoreError> {
        let (wal_bytes, wal_transactions) = self.projected_wal_state(transaction_bytes)?;
        Ok(wal_bytes > self.compact_after_bytes
            || wal_transactions > u64::from(self.compact_after_transactions))
    }

    fn compact_before_append(&mut self) -> Result<(), StoreError> {
        let started = Instant::now();
        if !self.retired_generations.is_empty() {
            match self.cleanup_retired_generations() {
                Ok(cleaned) => {
                    self.preappend_cleanup_generations = self
                        .preappend_cleanup_generations
                        .saturating_add(u64::try_from(cleaned).unwrap_or(u64::MAX));
                }
                Err(error) => {
                    self.preappend_cleanup_failures =
                        self.preappend_cleanup_failures.saturating_add(1);
                    return Err(error);
                }
            }
        }
        self.compact()?;
        self.preappend_compactions = self.preappend_compactions.saturating_add(1);
        self.preappend_compaction_duration_ns = self
            .preappend_compaction_duration_ns
            .saturating_add(duration_nanos(started.elapsed()));
        Ok(())
    }

    /// Appends one atomic transaction carrying `operations`.
    ///
    /// The transaction is validated against the in-memory table before any
    /// byte is written, so an operation that could not be replayed never
    /// reaches the WAL. Whether the append is synced before returning
    /// depends on the operations it carries (see [`sync_policy_for`]) and on
    /// the configured sync interval.
    ///
    /// This is the single funnel every public append path goes through, so
    /// the format rules that no encoder may break -- the reserved reason
    /// code, the per-transaction operation maximum -- and both size bounds
    /// (the tracked-record population and the WAL cap) are all enforced
    /// here, once. An append that would push the WAL past
    /// [`limits::StoreLimits::max_wal_bytes`] is refused with the table
    /// untouched; compacting frees the whole budget and the same
    /// transaction then fits.
    pub fn append(&mut self, operations: Vec<Operation>) -> Result<AppendOutcome, StoreError> {
        self.ensure_not_unusable("append a checkpoint transaction")?;
        if operations.is_empty() {
            return Err(StoreError::EmptyTransaction);
        }
        if operations.len() > WAL_MAX_OPS_PER_TX as usize {
            return Err(StoreError::TransactionTooLarge {
                operations: operations.len(),
                max: WAL_MAX_OPS_PER_TX,
            });
        }
        // The coarse check above already rejects anything wider than the
        // widest class (`WAL_MAX_OPS_PER_TX`, progress-only); this tightens
        // it to the non-progress class's narrower maximum. A mixed-class
        // transaction is left to `Transaction::encode`'s own
        // `MixedTransactionClass` error below, since there is no single
        // class maximum to report for it here.
        if let ClassifyOutcome::Class(class) = classify_operations(&operations)
            && operations.len() > class.max_ops() as usize
        {
            return Err(StoreError::TransactionTooLarge {
                operations: operations.len(),
                max: class.max_ops(),
            });
        }
        reject_reserved_reason_codes(&operations)?;
        let mut reset_beginning = 0u64;
        let mut reset_end = 0u64;
        let mut keep_failed = 0u64;
        let mut quarantine_removals = 0u64;
        for operation in &operations {
            match operation {
                Operation::ResetQuarantinedFile(reset) => match reset.action {
                    ResetQuarantineAction::ResetToBeginning => {
                        reset_beginning = reset_beginning.saturating_add(1);
                    }
                    ResetQuarantineAction::ResetToEnd => {
                        reset_end = reset_end.saturating_add(1);
                    }
                    ResetQuarantineAction::KeepFailed => {
                        keep_failed = keep_failed.saturating_add(1);
                    }
                },
                Operation::RemoveFile(removal)
                    if removal.administrative
                        && removal.expected_prior_state == LifecycleState::Quarantined =>
                {
                    quarantine_removals = quarantine_removals.saturating_add(1);
                }
                _ => {}
            }
        }
        self.ensure_fingerprint_capacity(&operations)?;
        self.ensure_tracked_capacity(&operations)?;
        let policy = sync_policy_for(&operations);
        let operation_count = operations.len();
        let mut transaction = Transaction {
            sequence: self.next_sequence,
            operations,
        };
        let mut bytes = transaction.encode().map_err(|source| StoreError::Encode {
            artifact: "WAL transaction",
            generation: self.generation,
            source,
        })?;
        let staged = self
            .table
            .stage_operations(&transaction.operations, &self.namespace_id)
            .map_err(|source| StoreError::Apply {
                operation: "validate a checkpoint transaction before persisting it",
                path: self.wal_path.clone(),
                source,
            })?;
        let requires_compaction = self.pending_wal_append.is_none()
            && self.append_requires_compaction(bytes.len() as u64)?;
        self.ensure_append_counters(requires_compaction)?;
        if requires_compaction {
            self.compact_before_append()?;
            transaction.sequence = self.next_sequence;
            bytes = transaction.encode().map_err(|source| StoreError::Encode {
                artifact: "WAL transaction",
                generation: self.generation,
                source,
            })?;
            if self.append_requires_compaction(bytes.len() as u64)? {
                return Err(StoreError::TransactionExceedsCompactionThreshold {
                    path: self.wal_path.clone(),
                    transaction_bytes: bytes.len() as u64,
                    compact_after_bytes: self.compact_after_bytes,
                    compact_after_transactions: self.compact_after_transactions,
                });
            }
        }
        // The prospective configured thresholds are enforced above. This
        // independent recovery-cap guard remains fail-closed if accounting
        // or a hand-built option ever violates that relationship.
        self.ensure_wal_capacity(bytes.len() as u64)?;
        let started = Instant::now();
        let result = self.write_transaction(
            &transaction,
            &staged,
            &bytes,
            policy,
            transaction.sequence,
            operation_count,
        );
        self.persist_duration_ns = self
            .persist_duration_ns
            .saturating_add(duration_nanos(started.elapsed()));
        self.persist_operations = self.persist_operations.saturating_add(1);
        if let Ok(outcome) = &result {
            self.table.commit_staged(staged);
            self.wal_bytes_appended = self.wal_bytes_appended.saturating_add(outcome.bytes);
            self.transactions_appended = self.transactions_appended.saturating_add(1);
            self.quarantine_reset_beginning = self
                .quarantine_reset_beginning
                .saturating_add(reset_beginning);
            self.quarantine_reset_end = self.quarantine_reset_end.saturating_add(reset_end);
            self.quarantine_keep_failed = self.quarantine_keep_failed.saturating_add(keep_failed);
            self.quarantine_removals = self.quarantine_removals.saturating_add(quarantine_removals);
        }
        result
    }

    /// Appends `operations` (always non-progress; progress commits go
    /// through the unsplit `append`/`commit_progress` path instead) as a
    /// series of atomic transactions, each bounded by both the non-progress
    /// class's operation-count maximum (`WAL_MAX_NON_PROGRESS_OPS_PER_TX`)
    /// and the format's hard 16 MiB transaction-body cap
    /// (`WAL_MAX_TX_BODY_BYTES`).
    ///
    /// `preflight_batched` fully preflights and byte-encodes every chunk
    /// (business rules, tracked-file/WAL capacity, and a real
    /// `Transaction::encode`) before the first chunk is appended, so a
    /// failure in a later chunk (an oversized encoding, or a business-rule
    /// conflict) is reported before any chunk becomes durable. Each chunk
    /// remains atomic on its own thereafter: splitting across chunks is
    /// safe because every operation carries its own expected state, so a
    /// crash between chunks leaves the earlier chunks durable and the later
    /// ones absent, which widens the duplicate window for the files in the
    /// later chunks and never loses acknowledged progress.
    pub fn append_batched(
        &mut self,
        operations: Vec<Operation>,
    ) -> Result<Vec<AppendOutcome>, StoreError> {
        self.ensure_not_unusable("append batched checkpoint transactions")?;
        if operations.is_empty() {
            return Err(StoreError::EmptyTransaction);
        }
        self.preflight_batched(&operations)?;
        let chunk_lengths = chunk_non_progress_operations(&operations)?;
        let mut outcomes = Vec::with_capacity(chunk_lengths.len());
        let mut remaining = operations;
        for length in chunk_lengths {
            let rest = remaining.split_off(length);
            outcomes.push(self.append(remaining)?);
            remaining = rest;
        }
        Ok(outcomes)
    }

    /// Appends a bounded logical batch without splitting any caller-defined
    /// atomic group across WAL transactions.
    ///
    /// The complete plan is preflighted before the first write. Groups are
    /// greedily packed into format-bounded transactions, but a group such as
    /// `register_file` plus its mandatory quarantine transition always
    /// remains in one transaction. A filesystem failure may persist an
    /// earlier complete transaction; it can never persist only part of one
    /// atomic group.
    #[cfg(test)]
    pub(crate) fn append_atomic_groups(
        &mut self,
        groups: Vec<Vec<Operation>>,
    ) -> Result<Vec<AppendOutcome>, StoreError> {
        match self.append_atomic_groups_cancellable(groups, || false)? {
            AtomicGroupAppendOutcome::Completed(outcomes) => Ok(outcomes),
            AtomicGroupAppendOutcome::Cancelled { .. } => {
                unreachable!("a non-cancellable grouped append cannot be cancelled")
            }
        }
    }

    /// Appends caller-defined atomic groups while checking cancellation
    /// immediately before every format-bounded WAL transaction.
    ///
    /// Cancellation can leave a complete durable transaction prefix, but
    /// never a partial caller-defined atomic group. Recovery safely resumes
    /// from that prefix without treating the entire plan as applied.
    #[cfg(test)]
    pub(crate) fn append_atomic_groups_cancellable(
        &mut self,
        groups: Vec<Vec<Operation>>,
        mut cancelled: impl FnMut() -> bool,
    ) -> Result<AtomicGroupAppendOutcome, StoreError> {
        let mut plan = self.prepare_atomic_group_append(groups)?;
        self.append_atomic_group_plan_cancellable(&mut plan, &mut cancelled)
    }

    /// Preflights and retains one bounded grouped-append plan for exact
    /// filesystem-failure retries.
    pub(crate) fn prepare_atomic_group_append(
        &self,
        groups: Vec<Vec<Operation>>,
    ) -> Result<AtomicGroupAppendPlan, StoreError> {
        self.ensure_not_unusable("append grouped checkpoint transactions")?;
        let (operations, transaction_lengths) = pack_atomic_groups(groups)?;
        let mut offset = 0usize;
        let transaction_slices = transaction_lengths.iter().map(|length| {
            let start = offset;
            offset += *length;
            &operations[start..offset]
        });
        self.preflight_transactions(&operations, transaction_slices)?;
        Ok(AtomicGroupAppendPlan {
            operations,
            transaction_lengths,
            next_transaction: 0,
            next_operation: 0,
        })
    }

    /// Appends the remaining transactions in a preflighted grouped plan.
    pub(crate) fn append_atomic_group_plan_cancellable(
        &mut self,
        plan: &mut AtomicGroupAppendPlan,
        mut cancelled: impl FnMut() -> bool,
    ) -> Result<AtomicGroupAppendOutcome, StoreError> {
        self.ensure_not_unusable("resume grouped checkpoint transactions")?;
        let mut outcomes = Vec::with_capacity(
            plan.transaction_lengths
                .len()
                .saturating_sub(plan.next_transaction),
        );
        while plan.next_transaction < plan.transaction_lengths.len() {
            if cancelled() {
                return Ok(AtomicGroupAppendOutcome::Cancelled {
                    completed: outcomes,
                });
            }
            let length = plan.transaction_lengths[plan.next_transaction];
            let end =
                plan.next_operation
                    .checked_add(length)
                    .ok_or(StoreError::CounterOverflow {
                        counter: "grouped checkpoint operation cursor",
                        value: u64::try_from(plan.next_operation).unwrap_or(u64::MAX),
                    })?;
            let transaction = plan.operations[plan.next_operation..end].to_vec();
            outcomes.push(self.append(transaction)?);
            plan.next_transaction += 1;
            plan.next_operation = end;
        }
        Ok(AtomicGroupAppendOutcome::Completed(outcomes))
    }

    /// Registers newly discovered files. Every registration in one call is
    /// batched into as few synced transactions as the format allows, and is
    /// durable before this call returns, so the receiver never reads a file
    /// whose registration could be lost.
    pub fn register_files(
        &mut self,
        registrations: Vec<RegisterFile>,
    ) -> Result<Vec<AppendOutcome>, StoreError> {
        self.append_batched(
            registrations
                .into_iter()
                .map(Operation::RegisterFile)
                .collect(),
        )
    }

    /// Records Ack-driven progress for one or more files.
    ///
    /// Offset and framing-resume state advance atomically per file, and all
    /// updates in one chunk commit or fail together. The sync itself may be
    /// deferred up to the configured sync interval.
    pub fn commit_progress(
        &mut self,
        updates: Vec<UpdateProgress>,
    ) -> Result<Vec<AppendOutcome>, StoreError> {
        self.append(updates.into_iter().map(Operation::UpdateProgress).collect())
            .map(|outcome| vec![outcome])
    }

    /// Replaces guarded fingerprint matching evidence.
    pub fn update_fingerprints(
        &mut self,
        updates: Vec<UpdateFingerprint>,
    ) -> Result<Vec<AppendOutcome>, StoreError> {
        self.append(
            updates
                .into_iter()
                .map(Operation::UpdateFingerprint)
                .collect(),
        )
        .map(|outcome| vec![outcome])
    }

    /// Updates guarded last-seen time and advisory path metadata.
    pub fn update_metadata(
        &mut self,
        updates: Vec<UpdateMetadata>,
    ) -> Result<Vec<AppendOutcome>, StoreError> {
        self.append_batched(updates.into_iter().map(Operation::UpdateMetadata).collect())
    }

    /// Persists a detected-truncation stream reset. Durable before this call
    /// returns, so the replacement stream is never read before the epoch
    /// change survives a crash.
    pub fn reset_after_truncate(
        &mut self,
        reset: ResetAfterTruncate,
    ) -> Result<AppendOutcome, StoreError> {
        self.append(vec![Operation::ResetAfterTruncate(reset)])
    }

    /// Persists fail-policy quarantines. Durable before this call returns, so
    /// a quarantine is never reported before it survives a crash.
    ///
    /// Every quarantine reason reserved from version-1 encoder output is
    /// refused, as it is on every other path that can encode one.
    pub fn quarantine_files(
        &mut self,
        quarantines: Vec<QuarantineFile>,
    ) -> Result<Vec<AppendOutcome>, StoreError> {
        self.append_batched(
            quarantines
                .into_iter()
                .map(Operation::QuarantineFile)
                .collect(),
        )
    }

    /// Applies an operator-authorized quarantine reset. Exact namespace,
    /// replacement fingerprint, and non-empty audit evidence are mandatory.
    pub fn reset_quarantined_file(
        &mut self,
        reset: ResetQuarantinedFile,
    ) -> Result<AppendOutcome, StoreError> {
        if reset.audit_reason.is_empty() {
            return Err(StoreError::AuditReasonRequired {
                operation: "reset_quarantined_file",
            });
        }
        self.append(vec![Operation::ResetQuarantinedFile(reset)])
    }

    /// Removes one complete runtime-vetted retention set through one filtered
    /// compaction and returns how many records were removed.
    ///
    /// The worker, not durable wall-clock metadata, proves continuous
    /// monotonic absence and every runtime veto before calling this method.
    /// This boundary independently rejects missing or quarantined records so
    /// an invalid set cannot publish a partial subset. Retention never encodes
    /// `remove_file`; the complete vetted set becomes authoritative only when
    /// the replacement `CURRENT` publication is durable.
    #[cfg(test)]
    fn remove_vetted_retention_records(
        &mut self,
        vetted: &HashSet<FileId>,
    ) -> Result<usize, StoreError> {
        self.remove_vetted_retention_records_cancellable(vetted, &mut || false)
            .map(|removed| removed.expect("non-cancellable retention cannot be cancelled"))
    }

    pub(crate) fn remove_vetted_retention_records_cancellable(
        &mut self,
        vetted: &HashSet<FileId>,
        cancelled: &mut impl FnMut() -> bool,
    ) -> Result<Option<usize>, StoreError> {
        self.ensure_not_unusable("remove expired checkpoint records")?;
        if cancelled() {
            return Ok(None);
        }
        if vetted.is_empty() {
            self.ensure_usable("remove expired checkpoint records")?;
            return Ok(Some(0));
        }
        if let Some(file_id) = vetted
            .iter()
            .copied()
            .filter(|file_id| self.table.get(file_id).is_none())
            .min()
        {
            return Err(StoreError::RetentionCandidateMissing { file_id });
        }
        if let Some(file_id) = vetted
            .iter()
            .copied()
            .filter(|file_id| {
                self.table
                    .get(file_id)
                    .is_some_and(|record| record.lifecycle_state == LifecycleState::Quarantined)
            })
            .min()
        {
            return Err(StoreError::RetentionCandidateQuarantined { file_id });
        }
        let removed = vetted.len();
        let records = self
            .table
            .snapshot_records()
            .into_iter()
            .filter(|record| !vetted.contains(&record.file_id))
            .collect();
        self.compact_replacing_table_cancellable(Some(records), cancelled)
            .map(|compacted| compacted.map(|()| removed))
    }

    /// Removes a quarantined record through the only path the format allows:
    /// an administrative removal naming this exact checkpoint namespace, the
    /// exact `file_id`, and a non-empty audit reason.
    ///
    /// Returns `Ok(None)` when the record is already absent, which the
    /// format defines as an idempotent no-op.
    pub fn remove_quarantined_file(
        &mut self,
        file_id: FileId,
        removal_reason: u16,
        removal_time_unix_nano: u64,
        audit_reason: String,
    ) -> Result<Option<AppendOutcome>, StoreError> {
        self.ensure_not_unusable("remove a quarantined checkpoint record")?;
        if audit_reason.is_empty() {
            return Err(StoreError::AuditReasonRequired {
                operation: "remove_quarantined_file",
            });
        }
        // Checked before the record lookup as well as inside `append`, so a
        // reserved reason code is refused rather than silently accepted as
        // the idempotent "already absent" no-op below.
        reject_reserved_reason_code("remove_file.removal_reason", removal_reason)?;
        let Some(record) = self.table.get(&file_id) else {
            self.ensure_usable("remove a quarantined checkpoint record")?;
            return Ok(None);
        };
        if record.lifecycle_state != LifecycleState::Quarantined {
            return Err(StoreError::NotQuarantined {
                operation: "remove_quarantined_file",
                file_id,
                state: record.lifecycle_state,
            });
        }
        // For a quarantined record the format compares the stored quarantine
        // epoch. A record that is quarantined without evidence is rejected by
        // replay, so the fallback below simply lets that fail closed there
        // instead of inventing a value here.
        let expected_file_epoch = record
            .quarantine_evidence
            .as_ref()
            .map_or(record.file_epoch, |evidence| evidence.quarantine_epoch);
        let removal = RemoveFile {
            file_id,
            expected_file_epoch,
            expected_prior_state: record.lifecycle_state,
            removal_reason,
            removal_time_unix_nano,
            administrative: true,
            namespace_id: Some(self.namespace_id.clone()),
            audit_reason: Some(audit_reason),
        };
        self.append(vec![Operation::RemoveFile(removal)]).map(Some)
    }

    /// Syncs any transaction written but not yet synced.
    pub fn sync(&mut self) -> Result<(), StoreError> {
        self.ensure_usable("sync the checkpoint WAL")?;
        if self.unsynced_transactions == 0 {
            return Ok(());
        }
        self.sync_wal()
    }

    /// Syncs outstanding interval-governed state once its deadline has
    /// elapsed, returning whether a sync was performed.
    ///
    /// The dedicated read/checkpoint worker must use
    /// [`Self::next_sync_deadline`] to drive this even when no later
    /// transaction arrives.
    pub fn sync_if_due(&mut self) -> Result<bool, StoreError> {
        self.ensure_usable("sync due checkpoint WAL state")?;
        if self.unsynced_transactions == 0 || !self.interval_sync_due() {
            return Ok(false);
        }
        self.sync_wal()?;
        Ok(true)
    }

    /// The deadline at which the worker must next call
    /// [`Self::sync_if_due`], or `None` when nothing is outstanding.
    #[must_use]
    pub fn next_sync_deadline(&self) -> Option<Instant> {
        if self.unsynced_transactions == 0 {
            return None;
        }
        Some(
            self.last_sync
                .checked_add(self.sync_interval)
                .unwrap_or_else(Instant::now),
        )
    }

    #[cfg(test)]
    pub(crate) fn force_sync_due_for_test(&mut self) {
        self.last_sync = Instant::now()
            .checked_sub(self.sync_interval)
            .unwrap_or(self.last_sync);
    }

    /// Makes every outstanding change durable as part of pipeline drain.
    ///
    /// The sync interval may defer an Ack-driven transaction's sync, so
    /// drain must force one: without it, a clean shutdown could widen the
    /// duplicate window exactly as an unclean one does.
    pub fn drain(&mut self) -> Result<(), StoreError> {
        self.sync()
    }

    /// Compacts if a configured threshold is met, reporting whether it did.
    pub fn compact_if_due(&mut self) -> Result<bool, StoreError> {
        self.compact_if_due_cancellable(&mut || false)
            .map(|compacted| compacted.expect("non-cancellable compaction cannot be cancelled"))
    }

    pub(crate) fn compact_if_due_cancellable(
        &mut self,
        mut cancelled: impl FnMut() -> bool,
    ) -> Result<Option<bool>, StoreError> {
        if cancelled() {
            return Ok(None);
        }
        self.ensure_usable("check whether checkpoint compaction is due")?;
        if !self.compaction_due() {
            return Ok(Some(false));
        }
        let Some(()) = self.compact_cancellable(&mut cancelled)? else {
            return Ok(None);
        };
        Ok(Some(true))
    }

    /// Compacts the namespace: writes and syncs a complete new generation
    /// holding the current table, then atomically repoints `CURRENT` at it.
    ///
    /// The previous generation stays on disk and fully recoverable until
    /// [`Self::cleanup_retired_generations`] removes it, so a crash at any
    /// point in this sequence leaves recovery with either the complete old
    /// generation or the complete new one.
    pub fn compact(&mut self) -> Result<(), StoreError> {
        self.compact_cancellable(&mut || false)
            .map(|compacted| compacted.expect("non-cancellable compaction cannot be cancelled"))
    }

    fn compact_cancellable(
        &mut self,
        cancelled: &mut impl FnMut() -> bool,
    ) -> Result<Option<()>, StoreError> {
        self.compact_replacing_table_cancellable(None, cancelled)
    }

    fn compact_replacing_table_cancellable(
        &mut self,
        replacement_records: Option<Vec<SnapshotRecord>>,
        cancelled: &mut impl FnMut() -> bool,
    ) -> Result<Option<()>, StoreError> {
        if cancelled() {
            return Ok(None);
        }
        self.ensure_usable("compact the checkpoint namespace")?;
        self._namespace
            .verify("revalidate the checkpoint namespace before compaction")?;
        if let Some(generation) = self.retired_generations.first().copied() {
            return Err(StoreError::RetiredGenerationCleanupRequired {
                dir: self.namespace_dir.clone(),
                generation,
            });
        }
        // Everything still unsynced belongs to the generation being
        // replaced; sync it first so a failure part way through compaction
        // cannot lose it from the old generation either.
        if self.unsynced_transactions > 0 {
            let synced = self.sync_wal();
            if cancelled() {
                return Ok(None);
            }
            synced?;
        }

        let previous = self.generation;
        let next = previous
            .checked_add(1)
            .ok_or(StoreError::GenerationOverflow {
                generation: previous,
            })?;
        let Some(_removed) = Self::cleanup_abandoned_publication_artifacts(
            &self.namespace_dir,
            previous,
            &mut *cancelled,
        )?
        else {
            return Ok(None);
        };

        let replace_table = replacement_records.is_some();
        let records = replacement_records.unwrap_or_else(|| self.table.snapshot_records());
        if cancelled() {
            return Ok(None);
        }
        let staged = Self::stage_generation(
            &self.namespace_dir,
            &self.namespace_id,
            next,
            &records,
            &self.limits,
            &mut self.faults,
        );
        if cancelled() {
            return Ok(None);
        }
        staged?;

        let wal_path = self.namespace_dir.join(wal_file_name(next));
        let Some(wal) = fsio::open_for_append_cancellable(&wal_path, &mut *cancelled)? else {
            return Ok(None);
        };

        let published = Self::publish_marker(
            &self.namespace_dir,
            next,
            PublicationRole::Compact,
            &mut self.faults,
        );
        let cancelled_after_publish = cancelled();
        if let Err(failure) = published {
            if failure.destination_may_have_changed {
                self.unusable =
                    Some("CURRENT was repointed or may have changed when publication failed");
            }
            return Err(failure.error);
        }
        if let Err(error) = self
            ._namespace
            .verify("revalidate the checkpoint namespace after compaction publication")
        {
            self.unusable = Some("the namespace changed after CURRENT publication");
            return Err(error);
        }

        // Replacing the handle closes the previous generation's WAL, which
        // Windows requires before that file can be removed by cleanup.
        self.wal = wal;
        self.wal_path = wal_path;
        self.generation = next;
        self.wal_bytes = WAL_HEADER_LEN as u64;
        self.wal_transactions = 0;
        self.next_sequence = 1;
        self.unsynced_transactions = 0;
        self.last_sync = Instant::now();
        self.record_sync_delay();
        self.retired_generations.push(previous);
        if replace_table {
            self.table = CheckpointTable::from_snapshot_records(records)
                .expect("filtered records from a valid table remain reachable");
        }
        if cancelled_after_publish {
            Ok(None)
        } else {
            Ok(Some(()))
        }
    }

    /// Generations kept on disk after compaction, oldest first.
    #[must_use]
    pub fn retired_generations(&self) -> &[u64] {
        &self.retired_generations
    }

    /// Removes every retired generation's files, returning how many
    /// generations were removed.
    ///
    /// This is the "later cleanup" step: it only ever touches generations
    /// that are no longer authoritative, never the generation `CURRENT`
    /// selects, and never the marker or the ownership lock.
    ///
    /// Cleanup is resumable. The complete pending list is retained until
    /// every unlink and the final directory sync succeed. A retry can
    /// therefore repeat already completed removals idempotently, including
    /// after both files disappeared but their directory updates were not
    /// known durable.
    pub fn cleanup_retired_generations(&mut self) -> Result<usize, StoreError> {
        self.cleanup_retired_generations_cancellable(&mut || false)
            .map(|cleaned| cleaned.expect("non-cancellable cleanup cannot be cancelled"))
    }

    pub(crate) fn cleanup_retired_generations_cancellable(
        &mut self,
        mut cancelled: impl FnMut() -> bool,
    ) -> Result<Option<usize>, StoreError> {
        if cancelled() {
            return Ok(None);
        }
        self.ensure_usable("clean up retired checkpoint generations")?;
        self._namespace
            .verify("revalidate the checkpoint namespace before retired cleanup")?;
        if self.retired_generations.is_empty() {
            return Ok(Some(0));
        }
        Self::require_current_generation(&self.namespace_dir, self.generation)?;
        let completed = self.retired_generations.len();
        for generation in self.retired_generations.iter().copied() {
            if generation == self.generation {
                return Err(StoreError::Unusable {
                    dir: self.namespace_dir.clone(),
                    operation: "clean up retired checkpoint generations",
                    reason: "the authoritative generation appeared in the retired list",
                });
            }
            let Some(_removed) = Self::remove_generation_files_cancellable(
                &self.namespace_dir,
                generation,
                &mut self.faults,
                &mut cancelled,
            )?
            else {
                return Ok(None);
            };
        }
        // Keep the complete pending list until the directory sync succeeds.
        // A retry after either an unlink or sync failure can therefore
        // repeat idempotent removals and retry the durability boundary.
        Self::require_current_generation(&self.namespace_dir, self.generation)?;
        self.faults.check(FaultPoint::BeforeRetiredDirectorySync)?;
        let Some(()) = fsio::sync_directory_cancellable(&self.namespace_dir, &mut cancelled)?
        else {
            return Ok(None);
        };
        self.retired_generations.clear();
        self._namespace
            .verify("revalidate the checkpoint namespace after retired cleanup")?;
        Ok(Some(completed))
    }

    /// Removes one retired generation's snapshot/WAL pair, reporting
    /// whether either file was present.
    fn remove_generation_files_cancellable(
        dir: &Path,
        generation: u64,
        faults: &mut FaultPlan,
        cancelled: &mut impl FnMut() -> bool,
    ) -> Result<Option<bool>, StoreError> {
        if cancelled() {
            return Ok(None);
        }
        faults.check(FaultPoint::BeforeRetiredGenerationRemoval)?;
        let snapshot_removed =
            fsio::remove_file_if_present(&dir.join(snapshot_file_name(generation)));
        if cancelled() {
            return Ok(None);
        }
        let snapshot_removed = snapshot_removed?;
        faults.check(FaultPoint::AfterRetiredSnapshotRemoval)?;
        if cancelled() {
            return Ok(None);
        }
        let wal_removed = fsio::remove_file_if_present(&dir.join(wal_file_name(generation)));
        if cancelled() {
            return Ok(None);
        }
        let wal_removed = wal_removed?;
        Ok(Some(snapshot_removed || wal_removed))
    }

    pub(super) fn ensure_usable(&self, operation: &'static str) -> Result<(), StoreError> {
        self.ensure_not_unusable(operation)?;
        if let Some(pending) = self.pending_wal_append {
            return Err(StoreError::PendingWalAppend {
                path: self.wal_path.clone(),
                operation,
                sequence: pending.sequence,
            });
        }
        Ok(())
    }

    pub(super) fn ensure_not_unusable(&self, operation: &'static str) -> Result<(), StoreError> {
        match self.unusable {
            Some(reason) => Err(StoreError::Unusable {
                dir: self.namespace_dir.clone(),
                operation,
                reason,
            }),
            None => Ok(()),
        }
    }

    pub(super) fn mark_unusable(&mut self, reason: &'static str) {
        self.unusable = Some(reason);
    }

    #[must_use]
    pub(crate) const fn has_pending_wal_append(&self) -> bool {
        self.pending_wal_append.is_some()
    }

    pub(super) fn admin_lock(&self) -> &NamespaceLock {
        &self._lock
    }

    pub(super) fn release_admin(self) -> Result<(), StoreError> {
        let Self { _lock, wal, .. } = self;
        drop(wal);
        _lock.release()
    }

    /// Refuses an append whose bytes would push the live WAL past the
    /// largest WAL this configuration can recover.
    fn ensure_wal_capacity(&self, transaction_bytes: u64) -> Result<(), StoreError> {
        let projected = self.wal_bytes.checked_add(transaction_bytes).ok_or(
            StoreError::AccountingOverflow {
                bytes: self.wal_bytes,
            },
        )?;
        if projected > self.limits.max_wal_bytes {
            return Err(StoreError::WalWouldExceedMaximum {
                path: self.wal_path.clone(),
                wal_bytes: self.wal_bytes,
                transaction_bytes,
                max: self.limits.max_wal_bytes,
            });
        }
        Ok(())
    }

    /// Preflights every deterministic failure for a batch before its first
    /// chunk is persisted.
    ///
    /// Filesystem failures can still occur between chunks and are handled
    /// by reopening the store, but capacity, sequence, encoding, and
    /// transition failures must not return an error after an earlier chunk
    /// has already consumed durable tracked-file or WAL capacity.
    fn preflight_batched(&self, operations: &[Operation]) -> Result<(), StoreError> {
        reject_reserved_reason_codes(operations)?;
        let chunk_lengths = chunk_non_progress_operations(operations)?;
        let mut offset = 0usize;
        let chunks = chunk_lengths.iter().map(|&length| {
            let start = offset;
            offset += length;
            &operations[start..offset]
        });
        self.preflight_transactions(operations, chunks)
    }

    fn preflight_transactions<'a>(
        &self,
        operations: &[Operation],
        transactions: impl IntoIterator<Item = &'a [Operation]>,
    ) -> Result<(), StoreError> {
        reject_reserved_reason_codes(operations)?;
        Self::ensure_fingerprint_capacity_for(operations, self.fingerprint_bytes)?;
        Self::ensure_tracked_capacity_for(
            &self.table,
            operations,
            self.max_tracked_files,
            &self.namespace_dir,
        )?;
        self.table
            .validate_operations(operations, &self.namespace_id)
            .map_err(|source| StoreError::Apply {
                operation: "preflight batched checkpoint transactions",
                path: self.wal_path.clone(),
                source,
            })?;

        let mut sequence = self.next_sequence;
        let mut projected_wal_bytes = self.wal_bytes;
        let mut projected_wal_transactions = self.wal_transactions;
        let mut projected_unsynced_transactions = self.unsynced_transactions;
        let mut projected_syncs = self.syncs;
        let mut projected_generation = self.generation;
        for chunk in transactions {
            let mut transaction = Transaction {
                sequence,
                operations: chunk.to_vec(),
            };
            let mut encoded = transaction.encode().map_err(|source| StoreError::Encode {
                artifact: "WAL transaction",
                generation: self.generation,
                source,
            })?;
            let mut transaction_bytes = encoded.len() as u64;
            let mut prospective_wal_bytes = projected_wal_bytes
                .checked_add(transaction_bytes)
                .ok_or(StoreError::AccountingOverflow {
                    bytes: projected_wal_bytes,
                })?;
            let mut prospective_wal_transactions = projected_wal_transactions
                .checked_add(1)
                .ok_or(StoreError::CounterOverflow {
                    counter: "WAL transactions",
                    value: projected_wal_transactions,
                })?;
            if prospective_wal_bytes > self.compact_after_bytes
                || prospective_wal_transactions > u64::from(self.compact_after_transactions)
            {
                projected_generation =
                    projected_generation
                        .checked_add(1)
                        .ok_or(StoreError::GenerationOverflow {
                            generation: projected_generation,
                        })?;
                if projected_unsynced_transactions > 0 {
                    projected_syncs =
                        projected_syncs
                            .checked_add(1)
                            .ok_or(StoreError::CounterOverflow {
                                counter: "WAL syncs",
                                value: projected_syncs,
                            })?;
                }
                projected_wal_bytes = WAL_HEADER_LEN as u64;
                projected_unsynced_transactions = 0;
                transaction.sequence = 1;
                encoded = transaction.encode().map_err(|source| StoreError::Encode {
                    artifact: "WAL transaction",
                    generation: self.generation,
                    source,
                })?;
                transaction_bytes = encoded.len() as u64;
                prospective_wal_bytes = projected_wal_bytes.checked_add(transaction_bytes).ok_or(
                    StoreError::AccountingOverflow {
                        bytes: projected_wal_bytes,
                    },
                )?;
                prospective_wal_transactions = 1;
                if prospective_wal_bytes > self.compact_after_bytes
                    || prospective_wal_transactions > u64::from(self.compact_after_transactions)
                {
                    return Err(StoreError::TransactionExceedsCompactionThreshold {
                        path: self.wal_path.clone(),
                        transaction_bytes,
                        compact_after_bytes: self.compact_after_bytes,
                        compact_after_transactions: self.compact_after_transactions,
                    });
                }
            }
            projected_unsynced_transactions = projected_unsynced_transactions
                .checked_add(1)
                .ok_or(StoreError::CounterOverflow {
                    counter: "unsynced WAL transactions",
                    value: projected_unsynced_transactions,
                })?;
            // Conservatively reserve one sync count per chunk. Interval
            // timing can cross a deadline between preflight and append, so
            // assuming fewer could expose a late deterministic overflow
            // after an earlier chunk was already persisted.
            projected_syncs =
                projected_syncs
                    .checked_add(1)
                    .ok_or(StoreError::CounterOverflow {
                        counter: "WAL syncs",
                        value: projected_syncs,
                    })?;
            if self.should_sync(sync_policy_for(chunk)) {
                projected_unsynced_transactions = 0;
            }
            sequence = transaction
                .sequence
                .checked_add(1)
                .ok_or(StoreError::SequenceOverflow {
                    sequence: transaction.sequence,
                })?;
            projected_wal_bytes = prospective_wal_bytes;
            projected_wal_transactions = prospective_wal_transactions;
            if projected_wal_bytes > self.limits.max_wal_bytes {
                return Err(StoreError::WalWouldExceedMaximum {
                    path: self.wal_path.clone(),
                    wal_bytes: projected_wal_bytes - transaction_bytes,
                    transaction_bytes,
                    max: self.limits.max_wal_bytes,
                });
            }
        }
        Ok(())
    }

    fn ensure_append_counters(&self, requires_compaction: bool) -> Result<(), StoreError> {
        if requires_compaction {
            let _next_generation =
                self.generation
                    .checked_add(1)
                    .ok_or(StoreError::GenerationOverflow {
                        generation: self.generation,
                    })?;
        }
        let wal_transactions = if requires_compaction {
            0
        } else {
            self.wal_transactions
        };
        let unsynced_transactions = if requires_compaction {
            0
        } else {
            self.unsynced_transactions
        };
        let sequence = if requires_compaction {
            1
        } else {
            self.next_sequence
        };
        let _wal_transactions =
            wal_transactions
                .checked_add(1)
                .ok_or(StoreError::CounterOverflow {
                    counter: "WAL transactions",
                    value: wal_transactions,
                })?;
        let _unsynced_transactions =
            unsynced_transactions
                .checked_add(1)
                .ok_or(StoreError::CounterOverflow {
                    counter: "unsynced WAL transactions",
                    value: unsynced_transactions,
                })?;
        let syncs_required = 1 + u64::from(requires_compaction && self.unsynced_transactions > 0);
        let _syncs = self
            .syncs
            .checked_add(syncs_required)
            .ok_or(StoreError::CounterOverflow {
                counter: "WAL syncs",
                value: self.syncs,
            })?;
        let _next_sequence = sequence
            .checked_add(1)
            .ok_or(StoreError::SequenceOverflow { sequence })?;
        Ok(())
    }

    /// Refuses a transaction whose registrations would push the durable
    /// record population past the configured `max_tracked_files`.
    ///
    /// This is what keeps the snapshot bound out of reach of any legal
    /// append sequence: the bound is sized for `max_tracked_files`
    /// worst-case records, so a table that stays inside the population
    /// always encodes inside the bound. Without it a caller could register
    /// past the population, then find every compaction refused as oversized
    /// while the WAL filled, and finally find even the removals that would
    /// shrink the table refused for want of WAL space.
    ///
    /// A transaction is measured by registrations for records not already
    /// tracked. The one exception is a validated non-administrative
    /// exact-locator supersede pair: its matching removal and replacement
    /// registration consume one existing slot atomically and have zero net
    /// growth.
    ///
    /// Recovery separately rejects a namespace that already exceeds the
    /// configured population. Shrinking this limit is an explicit state
    /// migration, not a way to open an over-capacity table and drain it.
    fn ensure_tracked_capacity(&self, operations: &[Operation]) -> Result<(), StoreError> {
        Self::ensure_tracked_capacity_for(
            &self.table,
            operations,
            self.max_tracked_files,
            &self.namespace_dir,
        )
    }

    fn ensure_tracked_capacity_for(
        table: &CheckpointTable,
        operations: &[Operation],
        max_tracked_files: u32,
        namespace_dir: &Path,
    ) -> Result<(), StoreError> {
        let mut registrations: HashSet<FileId> = HashSet::new();
        for operation in operations {
            if let Operation::RegisterFile(op) = operation
                && table.get(&op.file_id).is_none()
            {
                let _ = registrations.insert(op.file_id);
            }
        }
        if registrations.is_empty() {
            return Ok(());
        }
        let mut supersede_replacements = HashSet::new();
        for operation in operations {
            let Operation::RemoveFile(removal) = operation else {
                continue;
            };
            if removal.administrative {
                continue;
            }
            let Some(existing) = table.get(&removal.file_id) else {
                continue;
            };
            if let Some(replacement) = operations.iter().find_map(|candidate| {
                let Operation::RegisterFile(register) = candidate else {
                    return None;
                };
                (registrations.contains(&register.file_id)
                    && register.file_id != removal.file_id
                    && register.locator == existing.locator)
                    .then_some(register.file_id)
            }) {
                let _ = supersede_replacements.insert(replacement);
            }
        }
        let additional = registrations
            .len()
            .checked_sub(supersede_replacements.len())
            .expect("supersede replacements are a subset of new registrations");
        let tracked = table.len();
        let exhausted = match (tracked as u64).checked_add(additional as u64) {
            Some(projected) => projected > u64::from(max_tracked_files),
            // A population that is not even representable is past any
            // configured maximum.
            None => true,
        };
        if exhausted {
            return Err(StoreError::TrackedFilesExhausted {
                dir: namespace_dir.to_path_buf(),
                tracked,
                registrations: additional,
                max: max_tracked_files,
            });
        }
        Ok(())
    }

    /// Enforces the configured fingerprint window on every operation that
    /// can introduce fingerprint bytes into durable state.
    fn ensure_fingerprint_capacity(&self, operations: &[Operation]) -> Result<(), StoreError> {
        Self::ensure_fingerprint_capacity_for(operations, self.fingerprint_bytes)
    }

    fn ensure_fingerprint_capacity_for(
        operations: &[Operation],
        fingerprint_bytes: u64,
    ) -> Result<(), StoreError> {
        let validate = |context: &'static str,
                        file_id: FileId,
                        fingerprint: &[u8]|
         -> Result<(), StoreError> {
            if fingerprint.len() as u64 > fingerprint_bytes {
                return Err(StoreError::FingerprintExceedsConfiguredMaximum {
                    context,
                    file_id,
                    len: fingerprint.len(),
                    max: fingerprint_bytes,
                });
            }
            Ok(())
        };
        for operation in operations {
            match operation {
                Operation::RegisterFile(op) => {
                    validate("registration fingerprint", op.file_id, &op.fingerprint)?;
                }
                Operation::UpdateFingerprint(op) => {
                    validate(
                        "expected update fingerprint",
                        op.file_id,
                        &op.expected_fingerprint,
                    )?;
                    validate("new update fingerprint", op.file_id, &op.new_fingerprint)?;
                }
                Operation::ResetAfterTruncate(op) => {
                    validate(
                        "truncate reset fingerprint",
                        op.file_id,
                        &op.new_fingerprint,
                    )?;
                }
                Operation::ResetQuarantinedFile(op) => {
                    validate(
                        "quarantine reset fingerprint",
                        op.file_id,
                        &op.new_fingerprint,
                    )?;
                }
                Operation::UpdateProgress(_)
                | Operation::UpdateMetadata(_)
                | Operation::QuarantineFile(_)
                | Operation::RemoveFile(_) => {}
            }
        }
        Ok(())
    }

    fn should_sync(&self, policy: SyncPolicy) -> bool {
        match policy {
            SyncPolicy::Immediate => true,
            SyncPolicy::Interval => self.interval_sync_due(),
        }
    }

    fn interval_sync_due(&self) -> bool {
        self.sync_interval.is_zero()
            || self
                .last_sync
                .checked_add(self.sync_interval)
                .is_none_or(|deadline| Instant::now() >= deadline)
    }

    fn write_transaction(
        &mut self,
        transaction: &Transaction,
        staged: &StagedOperations,
        bytes: &[u8],
        policy: SyncPolicy,
        sequence: u64,
        operation_count: usize,
    ) -> Result<AppendOutcome, StoreError> {
        let mut pending = PendingWalAppend::new(self, bytes, policy, sequence, operation_count)?;
        if let Some(expected) = self.pending_wal_append {
            if !expected.matches(pending) {
                return Err(StoreError::PendingWalAppendMismatch {
                    path: self.wal_path.clone(),
                    expected_sequence: expected.sequence,
                    expected_bytes: expected.transaction_bytes,
                    found_sequence: pending.sequence,
                    found_bytes: pending.transaction_bytes,
                });
            }
            match self.reconcile_pending_append(transaction, staged, expected)? {
                PendingAppendResolution::RetryWrite => {}
                PendingAppendResolution::Complete(outcome) => return Ok(outcome),
            }
        }
        self.faults.check(FaultPoint::BeforeWalTransactionWrite)?;
        if let Err(error) = self.faults.check(FaultPoint::DuringWalTransactionWrite) {
            let prefix_len = (bytes.len() / 2).max(1);
            let result = self.wal.write_all(&bytes[..prefix_len]);
            self.pending_wal_append = Some(pending);
            return match result {
                Ok(()) => Err(error),
                Err(source) => Err(StoreError::Io {
                    operation: "write a partial checkpoint WAL transaction",
                    path: self.wal_path.clone(),
                    source,
                }),
            };
        }
        let append_started = Instant::now();
        if policy == SyncPolicy::Interval && self.first_unsynced_at.is_none() {
            self.first_unsynced_at = Some(append_started);
            pending.started_sync_delay = true;
        }
        if let Err(source) = self.wal.write_all(bytes) {
            self.pending_wal_append = Some(pending);
            return Err(StoreError::Io {
                operation: "append a transaction to the checkpoint WAL",
                path: self.wal_path.clone(),
                source,
            });
        }
        if let Err(error) = self.faults.check(FaultPoint::AfterWalTransactionWrite) {
            self.pending_wal_append = Some(pending);
            return Err(error);
        }
        self.install_pending_append_accounting(pending);

        let synced = if self.should_sync(policy) {
            if let Err(error) = self.sync_wal_for_pending_append() {
                self.restore_pending_append_accounting(pending);
                pending.requires_sync = true;
                self.pending_wal_append = Some(pending);
                return Err(error);
            }
            true
        } else {
            false
        };
        Ok(AppendOutcome {
            sequence,
            operations: operation_count,
            bytes: pending.transaction_bytes,
            synced,
            compaction_due: self.compaction_due(),
        })
    }

    fn reconcile_pending_append(
        &mut self,
        transaction: &Transaction,
        staged: &StagedOperations,
        mut pending: PendingWalAppend,
    ) -> Result<PendingAppendResolution, StoreError> {
        fsio::verify_checkpoint_file_path_binding(
            &self.wal,
            &self.wal_path,
            "verify the checkpoint WAL before append reconciliation",
        )?;
        let Some((loaded, observation)) = Self::load_generation_inspecting_append(
            &self.namespace_dir,
            self.generation,
            &self.namespace_id,
            &self.limits,
            self.max_tracked_files,
            self.fingerprint_bytes,
            fsio::ArtifactReadMode::RepairPermissions,
            Some(pending.boundary),
            &mut || false,
        )?
        else {
            unreachable!("non-cancellable WAL append reconciliation cannot be cancelled")
        };
        fsio::verify_checkpoint_file_path_binding(
            &self.wal,
            &self.wal_path,
            "verify the checkpoint WAL after append reconciliation",
        )?;
        let observation = observation.expect("append reconciliation requested an observation");
        let replayed = u64::try_from(loaded.transactions_replayed).map_err(|_| {
            StoreError::WalAppendRecoveryMismatch {
                path: self.wal_path.clone(),
                boundary: pending.boundary,
                reason: "the recovered transaction count does not fit u64",
            }
        })?;

        let prefix_matches = loaded.wal_valid_len == pending.boundary
            && replayed == pending.wal_transactions_before
            && loaded.next_sequence == pending.sequence
            && loaded.table == self.table;
        let resolution = match observation {
            WalAppendObservation::NoWrite => {
                if loaded.torn_tail_bytes != 0 || !prefix_matches {
                    return Err(self.wal_append_recovery_mismatch(
                        pending,
                        "a no-write result did not reproduce the exact known prefix",
                    ));
                }
                if pending.repair_sync_required {
                    self.sync_torn_append_repair(pending)?;
                }
                if pending.started_sync_delay {
                    self.first_unsynced_at = None;
                }
                PendingAppendResolution::RetryWrite
            }
            WalAppendObservation::Torn { bytes } => {
                if bytes == 0 || loaded.torn_tail_bytes != bytes || !prefix_matches {
                    return Err(self.wal_append_recovery_mismatch(
                        pending,
                        "a torn append did not begin at the exact known prefix",
                    ));
                }
                pending.repair_sync_required = true;
                self.pending_wal_append = Some(pending);
                self.sync_torn_append_repair(pending)?;
                if pending.started_sync_delay {
                    self.first_unsynced_at = None;
                }
                PendingAppendResolution::RetryWrite
            }
            WalAppendObservation::Complete {
                transaction: recovered,
                bytes,
            } => {
                let bytes = u64::try_from(bytes).map_err(|_| {
                    self.wal_append_recovery_mismatch(
                        pending,
                        "the recovered transaction length does not fit u64",
                    )
                })?;
                if &recovered != transaction
                    || bytes != pending.transaction_bytes
                    || loaded.torn_tail_bytes != 0
                    || loaded.wal_valid_len != pending.wal_bytes_after
                    || replayed != pending.wal_transactions_after
                    || loaded.next_sequence != pending.next_sequence_after
                    || !loaded.table.matches_staged_commit(&self.table, staged)
                {
                    return Err(self.wal_append_recovery_mismatch(
                        pending,
                        "the complete suffix is not exactly the pending transaction",
                    ));
                }
                self.reopen_validated_wal()?;
                self.install_pending_append_accounting(pending);
                let synced = if pending.requires_sync || self.should_sync(pending.policy) {
                    if let Err(error) = self.sync_wal_for_pending_append() {
                        self.restore_pending_append_accounting(pending);
                        let mut pending = pending;
                        pending.requires_sync = true;
                        self.pending_wal_append = Some(pending);
                        return Err(error);
                    }
                    true
                } else {
                    false
                };
                self.pending_wal_append = None;
                return Ok(PendingAppendResolution::Complete(AppendOutcome {
                    sequence: pending.sequence,
                    operations: pending.operation_count,
                    bytes: pending.transaction_bytes,
                    synced,
                    compaction_due: self.compaction_due(),
                }));
            }
        };

        self.reopen_validated_wal()?;
        self.pending_wal_append = None;
        Ok(resolution)
    }

    fn sync_torn_append_repair(&mut self, pending: PendingWalAppend) -> Result<(), StoreError> {
        fsio::verify_checkpoint_file_path_binding(
            &self.wal,
            &self.wal_path,
            "verify the checkpoint WAL before torn-append repair",
        )?;
        self.faults.check(FaultPoint::BeforeTornTailTruncate)?;
        let repair = fsio::open_for_wal_repair_cancellable(&self.wal_path, &mut || false)?
            .expect("non-cancellable WAL repair open cannot be cancelled");
        fsio::verify_same_checkpoint_file(
            &self.wal,
            &repair,
            &self.wal_path,
            "verify the checkpoint WAL repair handle",
        )?;
        repair
            .set_len(pending.boundary)
            .map_err(|source| StoreError::Io {
                operation: "truncate a torn checkpoint WAL append",
                path: self.wal_path.clone(),
                source,
            })?;
        repair.sync_all().map_err(|source| StoreError::Io {
            operation: "sync a truncated checkpoint WAL append",
            path: self.wal_path.clone(),
            source,
        })?;
        self.faults.check(FaultPoint::AfterTornTailTruncate)?;
        fsio::verify_checkpoint_file_path_binding(
            &self.wal,
            &self.wal_path,
            "verify the checkpoint WAL after torn-append repair",
        )
    }

    fn reopen_validated_wal(&mut self) -> Result<(), StoreError> {
        fsio::verify_checkpoint_file_path_binding(
            &self.wal,
            &self.wal_path,
            "verify the checkpoint WAL before reopening it",
        )?;
        let wal = fsio::open_for_append_cancellable(&self.wal_path, &mut || false)?
            .expect("non-cancellable WAL append reopen cannot be cancelled");
        fsio::verify_same_checkpoint_file(
            &self.wal,
            &wal,
            &self.wal_path,
            "verify the reopened checkpoint WAL identity",
        )?;
        fsio::verify_checkpoint_file_path_binding(
            &wal,
            &self.wal_path,
            "verify the reopened checkpoint WAL path",
        )?;
        self.wal = wal;
        Ok(())
    }

    fn install_pending_append_accounting(&mut self, pending: PendingWalAppend) {
        if pending.policy == SyncPolicy::Interval
            && pending.unsynced_transactions_before == 0
            && pending.unsynced_transactions_after != 0
            && self.first_unsynced_at.is_none()
        {
            self.first_unsynced_at = Some(Instant::now());
        }
        self.wal_bytes = pending.wal_bytes_after;
        self.wal_transactions = pending.wal_transactions_after;
        self.unsynced_transactions = pending.unsynced_transactions_after;
        self.next_sequence = pending.next_sequence_after;
    }

    fn restore_pending_append_accounting(&mut self, pending: PendingWalAppend) {
        self.wal_bytes = pending.boundary;
        self.wal_transactions = pending.wal_transactions_before;
        self.unsynced_transactions = pending.unsynced_transactions_before;
        self.next_sequence = pending.sequence;
    }

    fn wal_append_recovery_mismatch(
        &self,
        pending: PendingWalAppend,
        reason: &'static str,
    ) -> StoreError {
        StoreError::WalAppendRecoveryMismatch {
            path: self.wal_path.clone(),
            boundary: pending.boundary,
            reason,
        }
    }

    fn sync_wal(&mut self) -> Result<(), StoreError> {
        self.sync_wal_with_failure_mode(true)
    }

    fn sync_wal_for_pending_append(&mut self) -> Result<(), StoreError> {
        self.sync_wal_with_failure_mode(false)
    }

    fn sync_wal_with_failure_mode(&mut self, mark_unusable: bool) -> Result<(), StoreError> {
        let started = Instant::now();
        let result = self.sync_wal_inner(mark_unusable);
        self.sync_duration_ns = self
            .sync_duration_ns
            .saturating_add(duration_nanos(started.elapsed()));
        self.sync_operations = self.sync_operations.saturating_add(1);
        result
    }

    fn sync_wal_inner(&mut self, mark_unusable: bool) -> Result<(), StoreError> {
        let Some(syncs) = self.syncs.checked_add(1) else {
            return Err(StoreError::CounterOverflow {
                counter: "WAL syncs",
                value: self.syncs,
            });
        };
        if let Err(error) = self.faults.check(FaultPoint::BeforeWalSync) {
            if mark_unusable {
                self.unusable =
                    Some("a fault was injected before syncing appended WAL transactions");
            }
            return Err(error);
        }
        // `sync_data` is sufficient and cheaper than `sync_all` here: POSIX
        // requires it to persist the metadata needed to read the data back,
        // which for an append-only file includes the new length.
        if let Err(source) = self.wal.sync_data() {
            if mark_unusable {
                self.unusable =
                    Some("a WAL sync failed, so acknowledged progress may not be durable");
            }
            return Err(StoreError::Io {
                operation: "sync the checkpoint WAL",
                path: self.wal_path.clone(),
                source,
            });
        }
        if let Err(error) = self.faults.check(FaultPoint::AfterWalSync) {
            if mark_unusable {
                self.unusable =
                    Some("a fault followed WAL sync, so the caller cannot rely on its outcome");
            }
            return Err(error);
        }
        self.unsynced_transactions = 0;
        self.last_sync = Instant::now();
        self.syncs = syncs;
        self.record_sync_delay();
        Ok(())
    }

    fn record_sync_delay(&mut self) {
        if let Some(first_unsynced_at) = self.first_unsynced_at.take() {
            self.sync_delay_ns = self
                .sync_delay_ns
                .saturating_add(duration_nanos(first_unsynced_at.elapsed()));
            self.sync_delay_operations = self.sync_delay_operations.saturating_add(1);
        }
    }
}

fn duration_nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

/// The generation `open` selected, and how it was selected.
#[derive(Debug, Clone, Copy)]
struct Selection {
    generation: u64,
    created: bool,
}

#[derive(Debug)]
enum WalAppendObservation {
    NoWrite,
    Torn {
        bytes: usize,
    },
    Complete {
        transaction: Transaction,
        bytes: usize,
    },
}

/// One validated generation, recovered into memory.
#[derive(Debug)]
pub(super) struct LoadedGeneration {
    /// Snapshot state with every complete WAL transaction applied.
    pub(super) table: CheckpointTable,
    /// Number of records decoded from the snapshot recovery base.
    pub(super) snapshot_records: usize,
    /// Number of complete WAL transactions replayed.
    pub(super) transactions_replayed: usize,
    /// Allowed structurally incomplete bytes at the final WAL tail.
    pub(super) torn_tail_bytes: usize,
    pub(super) wal_valid_len: u64,
    pub(super) next_sequence: u64,
}
