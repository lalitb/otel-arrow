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
//! only after the append and any required sync succeed. A write error marks
//! the handle unusable because disk may contain a partial or complete frame;
//! a failed required sync attempts to restore and sync the prior WAL length.
//! Reopening always recovers the authoritative WAL state.
//!
//! # Resource bounds
//!
//! Recovery reads a whole snapshot and a whole WAL into memory before it
//! can decode them, so each needs a size cap. Those caps are not separate
//! knobs a caller can set inconsistently: [`StoreOptions`] carries the
//! receiver's own `checkpoint.compact_after_bytes`,
//! `limits.max_tracked_files`, and `identity.fingerprint_bytes`, and
//! [`limits::StoreLimits::derive`] turns them into the worst-case sizes
//! this configuration can legally write:
//!
//! - the snapshot cap is the snapshot header and footer plus
//!   `max_tracked_files` worst-case records (a record whose fingerprint is
//!   the configured maximum, whose advisory path is the format's maximum,
//!   and which carries quarantine evidence);
//! - the WAL cap is the WAL header plus `compact_after_bytes` plus one
//!   maximal transaction, which is what a WAL can reach if a caller
//!   compacts as soon as [`CheckpointStore::compaction_due`] reports it.
//!
//! Both write paths enforce exactly those caps: an append that would push
//! the live WAL past its cap is refused before the in-memory table
//! advances (compact and retry), and a compaction whose encoded snapshot
//! exceeds its cap is refused before any byte is written, leaving the
//! current generation authoritative. A namespace can therefore never be
//! left holding an artifact its own configuration cannot read back.
//!
//! The caps travel with the configuration, not with the files: shrinking
//! `max_tracked_files`, `fingerprint_bytes`, or `compact_after_bytes`
//! below what a namespace already holds makes the next open fail closed
//! with [`StoreError::FileTooLarge`] rather than truncate durable state.

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

use super::apply::CheckpointTable;
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
use fsio::{AtomicWriteError, AtomicWriteFaults};
use layout::{
    CURRENT_BACKUP_FILE_NAME, CURRENT_FILE_NAME, CURRENT_TEMP_FILE_NAME, INITIAL_GENERATION,
    snapshot_file_name, wal_file_name,
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
    /// Compact once the live WAL body, excluding its fixed header, reaches
    /// this many bytes and contains at least one transaction.
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
    /// directory by the shared version-1 namespace contract.
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
    /// Whether this open adopted a complete generation that had no
    /// `CURRENT` marker, either from interrupted first creation or from a
    /// validated `CURRENT.tmp` publication postcondition.
    pub adopted_without_marker: bool,
    /// Records recovered from the snapshot before WAL replay.
    pub snapshot_records: usize,
    /// WAL transactions replayed on top of the snapshot.
    pub transactions_replayed: usize,
    /// Bytes discarded from a structurally incomplete final WAL
    /// transaction. Any other WAL damage fails recovery closed instead.
    pub torn_tail_bytes: usize,
    /// Abandoned same-directory temporary files removed during recovery.
    pub removed_temp_files: usize,
    /// Generations still on disk that are no longer authoritative. They stay
    /// recoverable until [`CheckpointStore::cleanup_retired_generations`]
    /// removes them.
    pub retired_generations: Vec<u64>,
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
}

/// Whether a transaction must be synced before the call returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncPolicy {
    /// Sync before returning, whatever the configured interval is.
    Immediate,
    /// Sync when the configured interval has elapsed.
    Interval,
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
/// [`CheckpointStore::remove_files`], and raw
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
    syncs: u64,
    wal_bytes_appended: u64,
    transactions_appended: u64,
    persist_duration_ns: u64,
    persist_operations: u64,
    sync_duration_ns: u64,
    sync_operations: u64,
    quarantine_reset_beginning: u64,
    quarantine_reset_end: u64,
    quarantine_keep_failed: u64,
    quarantine_removals: u64,
    faults: FaultPlan,
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
            adopted_without_marker: false,
            snapshot_records: loaded.snapshot_records,
            transactions_replayed: loaded.transactions_replayed,
            torn_tail_bytes: repaired_torn_tail_bytes,
            removed_temp_files: 0,
            retired_generations: retired_generations.clone(),
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
            syncs: 0,
            wal_bytes_appended: 0,
            transactions_appended: 0,
            persist_duration_ns: 0,
            persist_operations: 0,
            sync_duration_ns: 0,
            sync_operations: 0,
            quarantine_reset_beginning: 0,
            quarantine_reset_end: 0,
            quarantine_keep_failed: 0,
            quarantine_removals: 0,
            faults,
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
            namespace_dir,
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
        let Some(()) = fsio::create_namespace_dir_cancellable(&namespace_dir, &mut *cancelled)?
        else {
            return Ok(None);
        };
        let Some(lock) = NamespaceLock::acquire_cancellable(
            &namespace_dir,
            ownership_timeout,
            ownership_retry_interval,
            &mut *cancelled,
        )?
        else {
            return Ok(None);
        };
        // Only safe once this writer owns the namespace: a temporary file
        // could otherwise belong to a live writer's in-flight publication.
        let Some(mut removed_temp_files) =
            layout::remove_stale_temp_files(&namespace_dir, &mut *cancelled)?
        else {
            return Ok(None);
        };

        let marker_path = namespace_dir.join(CURRENT_FILE_NAME);
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
                    adopted_without_marker: false,
                    marker_temp_authoritative: false,
                }
            }
            None => {
                let marker_temp_path = namespace_dir.join(CURRENT_TEMP_FILE_NAME);
                let marker_backup_path = namespace_dir.join(CURRENT_BACKUP_FILE_NAME);
                let Some(marker_temp_bytes) = fsio::read_file_bounded_cancellable(
                    &marker_temp_path,
                    "CURRENT temporary marker",
                    MARKER_READ_MAX_BYTES,
                    &mut *cancelled,
                )?
                else {
                    return Ok(None);
                };
                let Some(marker_backup_bytes) = fsio::read_file_bounded_cancellable(
                    &marker_backup_path,
                    "CURRENT backup marker",
                    MARKER_READ_MAX_BYTES,
                    &mut *cancelled,
                )?
                else {
                    return Ok(None);
                };
                match (marker_temp_bytes, marker_backup_bytes) {
                    (Some(bytes), marker_backup_bytes) => {
                        let Some(selection) = Self::select_from_temporary_marker(
                            &namespace_dir,
                            &marker_temp_path,
                            &bytes,
                            &marker_backup_path,
                            marker_backup_bytes.as_deref(),
                            &mut *cancelled,
                        )?
                        else {
                            return Ok(None);
                        };
                        selection
                    }
                    (None, Some(_)) => {
                        let Some(generations) =
                            layout::scan_generations(&namespace_dir, &mut *cancelled)?
                        else {
                            return Ok(None);
                        };
                        return Err(StoreError::MissingMarker {
                            dir: namespace_dir,
                            marker: CURRENT_FILE_NAME,
                            highest_generation: generations
                                .keys()
                                .next_back()
                                .copied()
                                .unwrap_or(INITIAL_GENERATION),
                        });
                    }
                    (None, None) => {
                        let Some(selection) = Self::select_without_marker(
                            &namespace_dir,
                            &namespace_id,
                            &mut faults,
                            &limits,
                            max_tracked_files,
                            &mut *cancelled,
                        )?
                        else {
                            return Ok(None);
                        };
                        selection
                    }
                }
            }
        };
        if cancelled() {
            return Ok(None);
        }

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
            // A checksum-valid CURRENT is not enough to discard other
            // marker evidence: retain CURRENT.tmp and CURRENT.bak until the
            // selected generation passes complete bounded validation.
            let Some(removed) =
                Self::remove_marker_recovery_files(&namespace_dir, &mut *cancelled)?
            else {
                return Ok(None);
            };
            removed_temp_files += removed;
        } else if selection.marker_temp_authoritative {
            // Windows ReplaceFileW failures 1176 and 1177 can remove
            // CURRENT while leaving the synced replacement under
            // CURRENT.tmp. The candidate marker and its exact generation
            // layout were checked before load, and the selected pair has
            // now passed full bounded validation, so finish that interrupted
            // publication without first deleting the only marker evidence.
            if cancelled() {
                return Ok(None);
            }
            Self::publish_existing_marker_temp(&namespace_dir).map_err(|failure| failure.error)?;
            removed_temp_files += 1;
        } else if selection.adopted_without_marker {
            // Finish the interrupted creation now that the adopted pair has
            // been validated in full: naming it in `CURRENT` restores the
            // namespace invariant that a marker always selects the
            // authoritative generation, so the next open takes the ordinary
            // path instead of relying on the first-store fallback again.
            if cancelled() {
                return Ok(None);
            }
            Self::publish_marker(&namespace_dir, generation, &mut faults)
                .map_err(|failure| failure.error)?;
        }
        if cancelled() {
            return Ok(None);
        }
        let Some(wal) = fsio::open_for_append_cancellable(&wal_path, &mut *cancelled)? else {
            return Ok(None);
        };

        let Some(generations) = layout::scan_generations(&namespace_dir, &mut *cancelled)? else {
            return Ok(None);
        };
        let retired_generations: Vec<u64> = generations
            .into_keys()
            .filter(|found| *found != generation)
            .collect();
        if cancelled() {
            return Ok(None);
        }

        let recovery = RecoveryReport {
            generation,
            created: selection.created,
            adopted_without_marker: selection.adopted_without_marker,
            snapshot_records: loaded.snapshot_records,
            transactions_replayed: loaded.transactions_replayed,
            torn_tail_bytes: loaded.torn_tail_bytes,
            removed_temp_files,
            retired_generations: retired_generations.clone(),
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
            syncs: 0,
            wal_bytes_appended: 0,
            transactions_appended: 0,
            persist_duration_ns: 0,
            persist_operations: 0,
            sync_duration_ns: 0,
            sync_operations: 0,
            quarantine_reset_beginning: 0,
            quarantine_reset_end: 0,
            quarantine_keep_failed: 0,
            quarantine_removals: 0,
            faults,
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

    fn remove_marker_recovery_files(
        dir: &Path,
        cancelled: &mut impl FnMut() -> bool,
    ) -> Result<Option<usize>, StoreError> {
        if cancelled() {
            return Ok(None);
        }
        let mut removed = 0usize;
        for name in [CURRENT_TEMP_FILE_NAME, CURRENT_BACKUP_FILE_NAME] {
            removed += usize::from(fsio::remove_file_if_present(&dir.join(name))?);
            if cancelled() {
                return Ok(None);
            }
        }
        Ok(Some(removed))
    }

    /// Selects the generation named by `CURRENT.tmp` only when the missing
    /// `CURRENT`, optional `CURRENT.bak`, and generation population exactly
    /// match a publication postcondition this store can produce.
    ///
    /// Generation 0 may be the sole pair after interrupted first
    /// publication. A later candidate must be complete, strictly newer than
    /// the complete generation named by `CURRENT.bak`, and no recognized
    /// generation may be newer than it. This also covers an audited
    /// namespace reset that deliberately jumps above retained evidence.
    /// Any other layout is ambiguous and retains both recovery names for
    /// operator diagnosis.
    fn select_from_temporary_marker(
        dir: &Path,
        marker_temp_path: &Path,
        marker_temp_bytes: &[u8],
        marker_backup_path: &Path,
        marker_backup_bytes: Option<&[u8]>,
        cancelled: &mut impl FnMut() -> bool,
    ) -> Result<Option<Selection>, StoreError> {
        let generation =
            decode_current_marker(marker_temp_bytes).map_err(|source| StoreError::Decode {
                artifact: "CURRENT temporary marker",
                path: marker_temp_path.to_path_buf(),
                source,
            })?;
        let Some(generations) = layout::scan_generations(dir, &mut *cancelled)? else {
            return Ok(None);
        };
        let candidate = generations.get(&generation).copied().unwrap_or_default();
        if !candidate.is_complete() {
            return Err(StoreError::IncompleteGeneration {
                dir: dir.to_path_buf(),
                generation,
                missing: candidate.missing(),
            });
        }

        let backup_generation = marker_backup_bytes
            .map(|bytes| {
                decode_current_marker(bytes).map_err(|source| StoreError::Decode {
                    artifact: "CURRENT backup marker",
                    path: marker_backup_path.to_path_buf(),
                    source,
                })
            })
            .transpose()?;
        let exact_publication_layout = if generation == INITIAL_GENERATION {
            generations.len() == 1 && backup_generation.is_none()
        } else {
            let Some(found) = backup_generation else {
                return Err(StoreError::MissingMarker {
                    dir: dir.to_path_buf(),
                    marker: CURRENT_FILE_NAME,
                    highest_generation: generations
                        .keys()
                        .next_back()
                        .copied()
                        .unwrap_or(generation),
                });
            };
            found < generation
                && generations
                    .get(&found)
                    .is_some_and(|files| files.is_complete())
                && generations
                    .keys()
                    .all(|recognized| *recognized <= generation)
        };
        if !exact_publication_layout {
            return Err(StoreError::MissingMarker {
                dir: dir.to_path_buf(),
                marker: CURRENT_FILE_NAME,
                highest_generation: generations
                    .keys()
                    .next_back()
                    .copied()
                    .unwrap_or(generation),
            });
        }

        Ok(Some(Selection {
            generation,
            created: false,
            adopted_without_marker: true,
            marker_temp_authoritative: true,
        }))
    }

    /// Chooses a generation for a namespace whose `CURRENT` marker is
    /// absent, creating the initial generation when the namespace is new.
    ///
    /// `CURRENT` is written last when a namespace is created and is only
    /// ever replaced atomically afterwards, so the sole state in which it
    /// can legitimately be missing is an interrupted *first* creation. The
    /// fallback to "the highest complete snapshot/WAL pair" is therefore
    /// deliberately restricted to that case: a namespace that already holds
    /// *any* trace of a generation beyond the initial one has been compacted
    /// at least once, so a missing marker there means something outside this
    /// store removed it, and guessing which generation was authoritative
    /// could silently discard durable progress. That case fails closed --
    /// including when the newer generation is itself incomplete, because an
    /// older complete pair is then a *stale* recovery base, not an
    /// authoritative one.
    fn select_without_marker(
        dir: &Path,
        namespace_id: &str,
        faults: &mut FaultPlan,
        limits: &StoreLimits,
        max_tracked_files: u32,
        cancelled: &mut impl FnMut() -> bool,
    ) -> Result<Option<Selection>, StoreError> {
        let Some(scan) = layout::scan_generations(dir, &mut *cancelled)? else {
            return Ok(None);
        };
        // Any file belonging to a later generation, complete or not, proves
        // the namespace has been compacted, which in turn proves a marker
        // once existed. Its absence is then unexplained, so nothing here is
        // adopted or recreated.
        match scan.keys().next_back().copied() {
            Some(highest_present) if highest_present > INITIAL_GENERATION => {
                return Err(StoreError::MissingMarker {
                    dir: dir.to_path_buf(),
                    marker: CURRENT_FILE_NAME,
                    highest_generation: highest_present,
                });
            }
            _ => {}
        }

        // Only the initial generation can be present from here on: this is a
        // first-store namespace, so the highest complete pair is the initial
        // pair, and an interrupted first creation is the only way the marker
        // can be missing.
        let initial_pair = scan.get(&INITIAL_GENERATION).copied().unwrap_or_default();
        if initial_pair.is_complete() {
            return Ok(Some(Selection {
                generation: INITIAL_GENERATION,
                created: false,
                adopted_without_marker: true,
                marker_temp_authoritative: false,
            }));
        }

        // Either an empty namespace, or an initial generation whose pair was
        // never completed. Neither was ever authoritative (no marker has ever
        // named it), so creating the initial generation cannot discard
        // durable progress.
        if !Self::verify_incomplete_initial_pair_is_empty(
            dir,
            namespace_id,
            limits,
            max_tracked_files,
            &mut *cancelled,
        )? {
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
        Ok(Some(Selection {
            generation: INITIAL_GENERATION,
            created: true,
            adopted_without_marker: false,
            marker_temp_authoritative: false,
        }))
    }

    /// Refuses to overwrite an incomplete initial generation that carries
    /// durable state.
    ///
    /// Creation writes the snapshot, then the WAL, then the marker, so the
    /// only incomplete initial pair it can leave behind holds an *empty*
    /// snapshot and an *empty* WAL: no caller ever received a store handle
    /// for a generation that was never published, so nothing could have been
    /// appended to it. A leftover that does carry records or transactions
    /// therefore did not come from an interrupted creation -- something
    /// outside this store removed the marker or the pair's other half -- and
    /// recreating the generation would silently discard durable progress.
    fn verify_incomplete_initial_pair_is_empty(
        dir: &Path,
        namespace_id: &str,
        limits: &StoreLimits,
        max_tracked_files: u32,
        cancelled: &mut impl FnMut() -> bool,
    ) -> Result<bool, StoreError> {
        let snapshot_path = dir.join(snapshot_file_name(INITIAL_GENERATION));
        let Some(snapshot) = fsio::read_file_bounded_cancellable(
            &snapshot_path,
            "snapshot",
            limits.max_snapshot_bytes,
            &mut *cancelled,
        )?
        else {
            return Ok(false);
        };
        if let Some(bytes) = snapshot {
            let header = decode_snapshot_header(&bytes).map_err(|source| StoreError::Decode {
                artifact: "snapshot",
                path: snapshot_path.clone(),
                source,
            })?;
            if header.generation != INITIAL_GENERATION {
                return Err(StoreError::GenerationMismatch {
                    artifact: "snapshot",
                    path: snapshot_path,
                    expected: INITIAL_GENERATION,
                    found: header.generation,
                });
            }
            if header.record_count > max_tracked_files {
                return Err(StoreError::RecoveredTrackedFilesExceedMaximum {
                    dir: dir.to_path_buf(),
                    tracked: header.record_count as usize,
                    max: max_tracked_files,
                });
            }
            if header.record_count != 0 {
                return Err(StoreError::IncompleteInitialGeneration {
                    dir: dir.to_path_buf(),
                    marker: CURRENT_FILE_NAME,
                    reason: "its snapshot already holds records",
                });
            }
            let snapshot =
                decode_snapshot(&bytes, &namespace_digest(namespace_id)).map_err(|source| {
                    StoreError::Decode {
                        artifact: "snapshot",
                        path: snapshot_path.clone(),
                        source,
                    }
                })?;
            debug_assert!(snapshot.records.is_empty());
        }
        if cancelled() {
            return Ok(false);
        }
        let wal_path = dir.join(wal_file_name(INITIAL_GENERATION));
        let Some(wal) = fsio::read_file_bounded_cancellable(
            &wal_path,
            "WAL",
            limits.max_wal_bytes,
            &mut *cancelled,
        )?
        else {
            return Ok(false);
        };
        if let Some(bytes) = wal {
            let wal_header = decode_wal_header(&bytes).map_err(|source| StoreError::Decode {
                artifact: "WAL",
                path: wal_path.clone(),
                source,
            })?;
            if wal_header.generation != INITIAL_GENERATION {
                return Err(StoreError::GenerationMismatch {
                    artifact: "WAL",
                    path: wal_path,
                    expected: INITIAL_GENERATION,
                    found: wal_header.generation,
                });
            }
            if bytes.len() > WAL_HEADER_LEN {
                let remaining = &bytes[WAL_HEADER_LEN..];
                Self::validate_declared_transaction_size(remaining, &wal_path, limits)?;
                let scan =
                    scan_one_transaction(remaining, 1).map_err(|source| StoreError::Decode {
                        artifact: "WAL",
                        path: wal_path.clone(),
                        source,
                    })?;
                let reason = match scan {
                    TransactionScan::Complete(_, _) => "its WAL already holds transactions",
                    TransactionScan::TornTail(_) => "its surviving WAL has a torn tail",
                };
                return Err(StoreError::IncompleteInitialGeneration {
                    dir: dir.to_path_buf(),
                    marker: CURRENT_FILE_NAME,
                    reason,
                });
            }
        }
        Ok(!cancelled())
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

        let mut cursor = WAL_HEADER_LEN;
        let mut expected_sequence = 1u64;
        let mut transactions_replayed = 0usize;
        let mut torn_tail_bytes = 0usize;
        while cursor < wal_bytes.len() {
            if cancelled() {
                return Ok(None);
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
                    torn_tail_bytes = bytes;
                    break;
                }
                TransactionScan::Complete(transaction, consumed) => {
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
                }
            }
        }
        let wal_valid_len = u64::try_from(cursor)
            .map_err(|_| StoreError::AccountingOverflow { bytes: u64::MAX })?;

        if cancelled() {
            return Ok(None);
        }
        Ok(Some(LoadedGeneration {
            table,
            snapshot_records,
            transactions_replayed,
            torn_tail_bytes,
            wal_valid_len,
            next_sequence: expected_sequence,
        }))
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
            faults,
            &mut *cancelled,
        )? {
            return Ok(false);
        }
        if cancelled() {
            return Ok(false);
        }
        Self::publish_marker(dir, generation, faults).map_err(|failure| failure.error)?;
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
        faults: &mut FaultPlan,
    ) -> Result<(), AtomicWriteError> {
        let marker = encode_current_marker(generation);
        fsio::write_file_atomically(
            dir,
            CURRENT_FILE_NAME,
            &marker,
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

    /// Finishes publication of a bounded, checksum-valid `CURRENT.tmp`
    /// whose complete target generation has already been loaded.
    fn publish_existing_marker_temp(dir: &Path) -> Result<(), AtomicWriteError> {
        fsio::publish_existing_temp_file(dir, CURRENT_FILE_NAME)?;
        fsio::sync_directory(dir).map_err(|error| AtomicWriteError {
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
        StoreStats {
            generation: self.generation,
            retired_generations: self.retired_generations.clone(),
            records: self.table.len(),
            wal_bytes: self.wal_bytes,
            wal_transactions: self.wal_transactions,
            unsynced_transactions: self.unsynced_transactions,
            next_sequence: self.next_sequence,
            syncs: self.syncs,
            quarantined_records: self.table.quarantined_len(),
            wal_bytes_appended: self.wal_bytes_appended,
            transactions_appended: self.transactions_appended,
            persist_duration_ns: self.persist_duration_ns,
            persist_operations: self.persist_operations,
            sync_duration_ns: self.sync_duration_ns,
            sync_operations: self.sync_operations,
            namespace_lock_wait_ns: duration_nanos(self._lock.waited()),
            namespace_lock_contentions: self._lock.contentions(),
            quarantine_reset_beginning: self.quarantine_reset_beginning,
            quarantine_reset_end: self.quarantine_reset_end,
            quarantine_keep_failed: self.quarantine_keep_failed,
            quarantine_removals: self.quarantine_removals,
        }
    }

    /// Whether a compaction threshold is met.
    #[must_use]
    pub fn compaction_due(&self) -> bool {
        (self.wal_transactions > 0
            && self.wal_bytes.saturating_sub(WAL_HEADER_LEN as u64) >= self.compact_after_bytes)
            || self.wal_transactions >= u64::from(self.compact_after_transactions)
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
        self.ensure_usable("append a checkpoint transaction")?;
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
        self.ensure_append_counters()?;
        let operation_count = operations.len();
        let transaction = Transaction {
            sequence: self.next_sequence,
            operations,
        };
        let _next_sequence =
            transaction
                .sequence
                .checked_add(1)
                .ok_or(StoreError::SequenceOverflow {
                    sequence: transaction.sequence,
                })?;
        let bytes = transaction.encode().map_err(|source| StoreError::Encode {
            artifact: "WAL transaction",
            generation: self.generation,
            source,
        })?;
        // Checked before the in-memory table advances: a transaction the
        // WAL has no room for must leave the store exactly as it was, so
        // the caller can compact and retry it unchanged.
        self.ensure_wal_capacity(bytes.len() as u64)?;
        let staged = self
            .table
            .stage_operations(&transaction.operations, &self.namespace_id)
            .map_err(|source| StoreError::Apply {
                operation: "validate a checkpoint transaction before persisting it",
                path: self.wal_path.clone(),
                source,
            })?;
        let started = Instant::now();
        let result = self.write_transaction(&bytes, policy, transaction.sequence, operation_count);
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
        self.ensure_usable("append batched checkpoint transactions")?;
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
    pub(crate) fn append_atomic_groups_cancellable(
        &mut self,
        groups: Vec<Vec<Operation>>,
        mut cancelled: impl FnMut() -> bool,
    ) -> Result<AtomicGroupAppendOutcome, StoreError> {
        self.ensure_usable("append grouped checkpoint transactions")?;
        let (operations, transaction_lengths) = pack_atomic_groups(groups)?;
        let mut offset = 0usize;
        let transaction_slices = transaction_lengths.iter().map(|length| {
            let start = offset;
            offset += *length;
            &operations[start..offset]
        });
        self.preflight_transactions(&operations, transaction_slices)?;

        let mut outcomes = Vec::with_capacity(transaction_lengths.len());
        let mut operations = operations.into_iter();
        for length in transaction_lengths {
            let transaction: Vec<Operation> = operations.by_ref().take(length).collect();
            if cancelled() {
                return Ok(AtomicGroupAppendOutcome::Cancelled {
                    completed: outcomes,
                });
            }
            outcomes.push(self.append(transaction)?);
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

    /// Updates advisory metadata (locator, last-seen time, advisory path).
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

    /// Applies an operator-authorized quarantine reset. The audit reason is
    /// mandatory and must be non-empty.
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

    /// Appends caller-constructed removals.
    ///
    /// A `removal_reason` of [`REASON_CODE_RESERVED`] is refused, as it is
    /// on every other path that can encode one.
    pub fn remove_files(
        &mut self,
        removals: Vec<RemoveFile>,
    ) -> Result<Vec<AppendOutcome>, StoreError> {
        self.append_batched(removals.into_iter().map(Operation::RemoveFile).collect())
    }

    /// The records ordinary retention would remove: those whose last-seen
    /// time is at least `retention` older than `now_unix_nano`.
    ///
    /// A quarantined record is never included, whatever its age: quarantine
    /// is exempt from ordinary retention and can only be removed by an
    /// explicit administrative operation. A zero `retention` means
    /// indefinite retention and selects nothing.
    #[must_use]
    pub fn retention_candidates(
        &self,
        eligible_absent: &HashSet<FileId>,
        now_unix_nano: u64,
        retention: Duration,
    ) -> Vec<FileId> {
        if retention.is_zero() {
            return Vec::new();
        }
        let retention_nanos = retention.as_nanos();
        let Some(cutoff) = u128::from(now_unix_nano).checked_sub(retention_nanos) else {
            return Vec::new();
        };
        let mut candidates: Vec<FileId> = self
            .table
            .iter()
            .filter(|(file_id, _)| eligible_absent.contains(file_id))
            .filter(|(_, record)| record.lifecycle_state != LifecycleState::Quarantined)
            .filter(|(_, record)| u128::from(record.last_seen_time_unix_nano) <= cutoff)
            .map(|(file_id, _)| *file_id)
            .collect();
        candidates.sort_unstable();
        candidates
    }

    /// Removes every record ordinary retention selects, in synced,
    /// bounded transactions, and returns how many were removed.
    ///
    /// Quarantined records are never removed here, both because
    /// [`Self::retention_candidates`] excludes them and because the format's
    /// replay rules reject a non-administrative removal of a quarantined
    /// record.
    pub fn remove_expired(
        &mut self,
        eligible_absent: &HashSet<FileId>,
        now_unix_nano: u64,
        retention: Duration,
        removal_reason: u16,
    ) -> Result<usize, StoreError> {
        self.ensure_usable("remove expired checkpoint records")?;
        // Checked before the table is scanned as well as inside `append`,
        // so a reserved reason code is refused even when retention selects
        // nothing and no transaction would be built at all.
        reject_reserved_reason_code("remove_file.removal_reason", removal_reason)?;
        let candidates = self.retention_candidates(eligible_absent, now_unix_nano, retention);
        if candidates.is_empty() {
            return Ok(0);
        }
        let mut removals = Vec::with_capacity(candidates.len());
        for file_id in candidates {
            let Some(record) = self.table.get(&file_id) else {
                continue;
            };
            removals.push(Operation::RemoveFile(RemoveFile {
                file_id,
                expected_file_epoch: record.file_epoch,
                expected_prior_state: record.lifecycle_state,
                removal_reason,
                removal_time_unix_nano: now_unix_nano,
                administrative: false,
                namespace_id: None,
                audit_reason: None,
            }));
        }
        let removed = removals.len();
        if removed == 0 {
            return Ok(0);
        }
        let _outcomes = self.append_batched(removals)?;
        Ok(removed)
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
        self.ensure_usable("remove a quarantined checkpoint record")?;
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

    /// Publishes a complete empty generation strictly above every generation
    /// the administration session recognized, retaining all old artifacts.
    pub(super) fn reset_to_empty_generation(
        &mut self,
        new_generation: u64,
    ) -> Result<(), StoreError> {
        self.ensure_usable("reset the complete checkpoint namespace")?;
        let highest = self
            .retired_generations
            .iter()
            .copied()
            .chain(std::iter::once(self.generation))
            .max()
            .unwrap_or(self.generation);
        if new_generation <= highest {
            return Err(StoreError::GenerationNotIncreasing {
                proposed: new_generation,
                highest,
            });
        }
        if self.unsynced_transactions > 0 {
            self.sync_wal()?;
        }

        Self::stage_generation(
            &self.namespace_dir,
            &self.namespace_id,
            new_generation,
            &[],
            &self.limits,
            &mut self.faults,
        )?;
        let new_wal_path = self.namespace_dir.join(wal_file_name(new_generation));
        let mut cancelled = || false;
        let new_wal = fsio::open_for_append_cancellable(&new_wal_path, &mut cancelled)?
            .expect("a non-cancellable namespace-reset WAL open cannot be cancelled");
        if let Err(failure) =
            Self::publish_marker(&self.namespace_dir, new_generation, &mut self.faults)
        {
            if failure.destination_may_have_changed {
                self.unusable =
                    Some("CURRENT was repointed or may have changed when publication failed");
            }
            return Err(failure.error);
        }

        let previous = self.generation;
        self.wal = new_wal;
        self.wal_path = new_wal_path;
        self.table = CheckpointTable::new();
        self.generation = new_generation;
        self.wal_bytes = WAL_HEADER_LEN as u64;
        self.wal_transactions = 0;
        self.next_sequence = 1;
        self.unsynced_transactions = 0;
        self.last_sync = Instant::now();
        if !self.retired_generations.contains(&previous) {
            self.retired_generations.push(previous);
        }
        self.retired_generations.sort_unstable();
        self.retired_generations.dedup();
        self.recovery.generation = new_generation;
        self.recovery.snapshot_records = 0;
        self.recovery.transactions_replayed = 0;
        self.recovery.torn_tail_bytes = 0;
        self.recovery.retired_generations = self.retired_generations.clone();
        Ok(())
    }

    fn compact_cancellable(
        &mut self,
        cancelled: &mut impl FnMut() -> bool,
    ) -> Result<Option<()>, StoreError> {
        if cancelled() {
            return Ok(None);
        }
        self.ensure_usable("compact the checkpoint namespace")?;
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

        let records = self.table.snapshot_records();
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

        let published = Self::publish_marker(&self.namespace_dir, next, &mut self.faults);
        let cancelled_after_publish = cancelled();
        if let Err(failure) = published {
            if failure.destination_may_have_changed {
                self.unusable =
                    Some("CURRENT was repointed or may have changed when publication failed");
            }
            return Err(failure.error);
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
        self.retired_generations.push(previous);
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
        if self.retired_generations.is_empty() {
            return Ok(Some(0));
        }
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
        self.faults.check(FaultPoint::BeforeRetiredDirectorySync)?;
        let Some(()) = fsio::sync_directory_cancellable(&self.namespace_dir, &mut cancelled)?
        else {
            return Ok(None);
        };
        self.retired_generations.clear();
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
        for chunk in transactions {
            projected_wal_transactions =
                projected_wal_transactions
                    .checked_add(1)
                    .ok_or(StoreError::CounterOverflow {
                        counter: "WAL transactions",
                        value: projected_wal_transactions,
                    })?;
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
            let transaction = Transaction {
                sequence,
                operations: chunk.to_vec(),
            };
            sequence = sequence
                .checked_add(1)
                .ok_or(StoreError::SequenceOverflow { sequence })?;
            let encoded = transaction.encode().map_err(|source| StoreError::Encode {
                artifact: "WAL transaction",
                generation: self.generation,
                source,
            })?;
            let transaction_bytes = encoded.len() as u64;
            let wal_bytes_before = projected_wal_bytes;
            projected_wal_bytes = projected_wal_bytes.checked_add(transaction_bytes).ok_or(
                StoreError::AccountingOverflow {
                    bytes: projected_wal_bytes,
                },
            )?;
            if projected_wal_bytes > self.limits.max_wal_bytes {
                return Err(StoreError::WalWouldExceedMaximum {
                    path: self.wal_path.clone(),
                    wal_bytes: wal_bytes_before,
                    transaction_bytes,
                    max: self.limits.max_wal_bytes,
                });
            }
        }
        Ok(())
    }

    fn ensure_append_counters(&self) -> Result<(), StoreError> {
        let _wal_transactions =
            self.wal_transactions
                .checked_add(1)
                .ok_or(StoreError::CounterOverflow {
                    counter: "WAL transactions",
                    value: self.wal_transactions,
                })?;
        let _unsynced_transactions =
            self.unsynced_transactions
                .checked_add(1)
                .ok_or(StoreError::CounterOverflow {
                    counter: "unsynced WAL transactions",
                    value: self.unsynced_transactions,
                })?;
        let _syncs = self
            .syncs
            .checked_add(1)
            .ok_or(StoreError::CounterOverflow {
                counter: "WAL syncs",
                value: self.syncs,
            })?;
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
    /// A transaction is measured by the registrations it carries for
    /// records that are not already tracked. Removals in the same
    /// transaction deliberately do not create headroom, which keeps this an
    /// upper bound on the table the transaction can produce.
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
        let tracked = table.len();
        let exhausted = match (tracked as u64).checked_add(registrations.len() as u64) {
            Some(projected) => projected > u64::from(max_tracked_files),
            // A population that is not even representable is past any
            // configured maximum.
            None => true,
        };
        if exhausted {
            return Err(StoreError::TrackedFilesExhausted {
                dir: namespace_dir.to_path_buf(),
                tracked,
                registrations: registrations.len(),
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
                Operation::UpdateProgress(_)
                | Operation::ResetAfterTruncate(_)
                | Operation::UpdateMetadata(_)
                | Operation::QuarantineFile(_)
                | Operation::ResetQuarantinedFile(_)
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
        bytes: &[u8],
        policy: SyncPolicy,
        sequence: u64,
        operation_count: usize,
    ) -> Result<AppendOutcome, StoreError> {
        if let Err(error) = self.faults.check(FaultPoint::BeforeWalTransactionWrite) {
            self.unusable =
                Some("a fault was injected before writing a prevalidated WAL transaction");
            return Err(error);
        }
        if let Err(error) = self.faults.check(FaultPoint::DuringWalTransactionWrite) {
            let prefix_len = (bytes.len() / 2).max(1);
            if let Err(source) = self.wal.write_all(&bytes[..prefix_len]) {
                self.unusable = Some("a partial WAL transaction write failed");
                return Err(StoreError::Io {
                    operation: "write a partial checkpoint WAL transaction",
                    path: self.wal_path.clone(),
                    source,
                });
            }
            self.unusable = Some("a fault left a partial WAL transaction");
            return Err(error);
        }
        if let Err(source) = self.wal.write_all(bytes) {
            self.unusable = Some("a WAL transaction write failed with uncertain partial output");
            return Err(StoreError::Io {
                operation: "append a transaction to the checkpoint WAL",
                path: self.wal_path.clone(),
                source,
            });
        }
        if let Err(error) = self.faults.check(FaultPoint::AfterWalTransactionWrite) {
            self.unusable =
                Some("a fault followed a WAL transaction write with uncertain durability");
            return Err(error);
        }
        let len = bytes.len() as u64;
        let Some(wal_bytes) = self.wal_bytes.checked_add(len) else {
            self.unusable = Some("WAL byte accounting overflowed after a transaction was written");
            return Err(StoreError::AccountingOverflow {
                bytes: self.wal_bytes,
            });
        };
        let Some(next_sequence) = sequence.checked_add(1) else {
            self.unusable = Some("the WAL sequence overflowed after a transaction was written");
            return Err(StoreError::SequenceOverflow { sequence });
        };
        let Some(wal_transactions) = self.wal_transactions.checked_add(1) else {
            self.unusable = Some("the WAL transaction counter overflowed");
            return Err(StoreError::CounterOverflow {
                counter: "WAL transactions",
                value: self.wal_transactions,
            });
        };
        let Some(unsynced_transactions) = self.unsynced_transactions.checked_add(1) else {
            self.unusable = Some("the unsynced transaction counter overflowed");
            return Err(StoreError::CounterOverflow {
                counter: "unsynced WAL transactions",
                value: self.unsynced_transactions,
            });
        };
        let wal_bytes_before = self.wal_bytes;
        let wal_transactions_before = self.wal_transactions;
        let unsynced_transactions_before = self.unsynced_transactions;
        let next_sequence_before = self.next_sequence;
        self.wal_bytes = wal_bytes;
        self.next_sequence = next_sequence;
        self.wal_transactions = wal_transactions;
        self.unsynced_transactions = unsynced_transactions;

        let synced = if self.should_sync(policy) {
            if let Err(error) = self.sync_wal() {
                if self.wal.set_len(wal_bytes_before).is_ok() && self.wal.sync_data().is_ok() {
                    self.wal_bytes = wal_bytes_before;
                    self.wal_transactions = wal_transactions_before;
                    self.unsynced_transactions = unsynced_transactions_before;
                    self.next_sequence = next_sequence_before;
                    self.unusable =
                        Some("a failed WAL sync was rolled back to the prior durable boundary");
                } else {
                    self.unusable =
                        Some("a failed WAL sync could not be rolled back to a durable boundary");
                }
                return Err(error);
            }
            true
        } else {
            false
        };
        Ok(AppendOutcome {
            sequence,
            operations: operation_count,
            bytes: len,
            synced,
            compaction_due: self.compaction_due(),
        })
    }

    fn sync_wal(&mut self) -> Result<(), StoreError> {
        let started = Instant::now();
        let result = self.sync_wal_inner();
        self.sync_duration_ns = self
            .sync_duration_ns
            .saturating_add(duration_nanos(started.elapsed()));
        self.sync_operations = self.sync_operations.saturating_add(1);
        result
    }

    fn sync_wal_inner(&mut self) -> Result<(), StoreError> {
        let Some(syncs) = self.syncs.checked_add(1) else {
            return Err(StoreError::CounterOverflow {
                counter: "WAL syncs",
                value: self.syncs,
            });
        };
        if let Err(error) = self.faults.check(FaultPoint::BeforeWalSync) {
            self.unusable = Some("a fault was injected before syncing appended WAL transactions");
            return Err(error);
        }
        // `sync_data` is sufficient and cheaper than `sync_all` here: POSIX
        // requires it to persist the metadata needed to read the data back,
        // which for an append-only file includes the new length.
        if let Err(source) = self.wal.sync_data() {
            self.unusable = Some("a WAL sync failed, so acknowledged progress may not be durable");
            return Err(StoreError::Io {
                operation: "sync the checkpoint WAL",
                path: self.wal_path.clone(),
                source,
            });
        }
        if let Err(error) = self.faults.check(FaultPoint::AfterWalSync) {
            self.unusable =
                Some("a fault followed WAL sync, so the caller cannot rely on its outcome");
            return Err(error);
        }
        self.unsynced_transactions = 0;
        self.last_sync = Instant::now();
        self.syncs = syncs;
        Ok(())
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
    adopted_without_marker: bool,
    marker_temp_authoritative: bool,
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
