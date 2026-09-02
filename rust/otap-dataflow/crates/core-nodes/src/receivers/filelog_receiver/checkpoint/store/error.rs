// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Errors returned by the durable checkpoint store.
//!
//! Every variant names the operation that failed, the path it failed on
//! (when a path is involved), and keeps the underlying cause as a
//! [`std::error::Error`] source, so an operator sees which durable step
//! failed on which file rather than a bare `io::Error`.
//!
//! The store never converts a durable-state failure into a success-shaped
//! fallback: an unreadable, inconsistent, or unsupported namespace is
//! reported here and the caller decides what to do.

use std::path::PathBuf;
use std::time::Duration;

use super::super::error::ApplyError;
use super::super::primitives::FileId;
use super::super::{DecodeError, EncodeError};
use super::fault::FaultPoint;
use super::limits::LimitsError;

/// A durable checkpoint store failure.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// A filesystem operation failed.
    #[error("failed to {operation} at {path}: {source}")]
    Io {
        /// The durable step that failed, phrased as an action.
        operation: &'static str,
        /// The path the step was operating on.
        path: PathBuf,
        /// The underlying operating-system error.
        #[source]
        source: std::io::Error,
    },
    /// A namespace or artifact path resolved to a filesystem object the
    /// store must not follow or mutate.
    #[error("refusing unsafe checkpoint filesystem object at {path}: {reason}")]
    UnsafeFilesystemObject {
        /// The rejected path.
        path: PathBuf,
        /// The violated filesystem invariant.
        reason: &'static str,
    },
    /// A namespace component cannot be represented on the filesystem that
    /// will contain it.
    #[error(
        "checkpoint namespace component {path} is {len} bytes, exceeding this filesystem's \
         {max}-byte component limit"
    )]
    NamespaceComponentTooLong {
        /// The component path that would be created.
        path: PathBuf,
        /// Encoded component length in bytes.
        len: usize,
        /// Filesystem-reported maximum component length in bytes.
        max: usize,
    },
    /// The namespace ownership lock is held by another writer and was not
    /// released within the configured bounded wait.
    #[error(
        "checkpoint namespace lock {path} is held by another writer; \
         waited {waited:?} of the {timeout:?} ownership timeout"
    )]
    NamespaceLocked {
        /// The lock file that could not be acquired.
        path: PathBuf,
        /// How long acquisition was actually attempted.
        waited: Duration,
        /// The configured bound on that attempt.
        timeout: Duration,
    },
    /// A stored artifact failed structural decoding (bad magic, unsupported
    /// version, checksum mismatch, invalid length, unknown discriminant).
    #[error("failed to decode {artifact} at {path}: {source}")]
    Decode {
        /// Which artifact failed to decode.
        artifact: &'static str,
        /// The file the artifact was read from.
        path: PathBuf,
        /// The structural decode failure.
        #[source]
        source: DecodeError,
    },
    /// An artifact could not be encoded for writing.
    #[error("failed to encode {artifact} for generation {generation}: {source}")]
    Encode {
        /// Which artifact failed to encode.
        artifact: &'static str,
        /// The generation the artifact belongs to.
        generation: u64,
        /// The structural encode failure.
        #[source]
        source: EncodeError,
    },
    /// A decoded operation failed its apply-time preconditions, either while
    /// replaying a recovered WAL or while validating a caller-supplied
    /// transaction before it reaches the WAL.
    #[error("failed to {operation} for checkpoint namespace at {path}: {source}")]
    Apply {
        /// The store operation that was being performed.
        operation: &'static str,
        /// The WAL the operation belongs (or would belong) to.
        path: PathBuf,
        /// The apply-time failure.
        #[source]
        source: ApplyError,
    },
    /// A stored artifact's embedded generation disagreed with the generation
    /// selected by `CURRENT` or encoded in its own file name.
    #[error("{artifact} at {path} declares generation {found}, expected {expected}")]
    GenerationMismatch {
        /// Which artifact carried the disagreeing generation.
        artifact: &'static str,
        /// The file that was read.
        path: PathBuf,
        /// The generation the store selected.
        expected: u64,
        /// The generation the artifact declared.
        found: u64,
    },
    /// A stored artifact's `namespace_digest` did not equal the digest
    /// expected for the selected `checkpoint.id`.
    #[error("{artifact} at {path} has a namespace_digest that does not match this namespace")]
    NamespaceMismatch {
        /// Which artifact carried the mismatched digest.
        artifact: &'static str,
        /// The file that was read.
        path: PathBuf,
    },
    /// The selected generation is missing one of its two required files.
    #[error(
        "checkpoint generation {generation} in {dir} is incomplete: \
         {missing} is missing"
    )]
    IncompleteGeneration {
        /// The namespace directory.
        dir: PathBuf,
        /// The generation that is incomplete.
        generation: u64,
        /// Which file of the pair is missing.
        missing: &'static str,
    },
    /// A namespace without valid `CURRENT` contains an artifact set that is
    /// not the exact bounded interrupted-first-publication state.
    #[error("checkpoint namespace {dir} has no valid CURRENT authority and is ambiguous: {reason}")]
    AuthorityMissingOrAmbiguous {
        /// The checkpoint namespace.
        dir: PathBuf,
        /// Which exact first-publication condition was violated.
        reason: &'static str,
    },
    /// A stored file was larger than the configured bound, so it was
    /// rejected before any buffer was allocated for it.
    #[error("{artifact} at {path} is {len} bytes, exceeding the {max}-byte maximum")]
    FileTooLarge {
        /// Which artifact was oversized.
        artifact: &'static str,
        /// The file that was rejected.
        path: PathBuf,
        /// The file's actual length.
        len: u64,
        /// The configured maximum.
        max: u64,
    },
    /// A bounded checkpoint read could not reserve its validated buffer.
    #[error("failed to reserve {requested} bytes while reading {artifact} at {path}: {source}")]
    Allocation {
        /// Which artifact was being read.
        artifact: &'static str,
        /// The artifact path.
        path: PathBuf,
        /// Validated allocation size requested.
        requested: usize,
        /// The allocator failure.
        #[source]
        source: std::collections::TryReserveError,
    },
    /// The options a store was opened with imply a worst-case artifact or
    /// recovery working set that exceeds its bound, so no namespace was
    /// opened.
    #[error("refusing to open the checkpoint namespace at {namespace_dir}: {source}")]
    ResourceBounds {
        /// The namespace directory that was refused.
        namespace_dir: PathBuf,
        /// Which bound could not be honored, and the knob to reduce.
        #[source]
        source: LimitsError,
    },
    /// A compaction encoded a snapshot larger than this configuration can
    /// read back, so it was refused before any byte was written and the
    /// current generation stays authoritative.
    #[error(
        "refusing to publish generation {generation} in {dir}: its {records}-record snapshot \
         encodes to {len} bytes, exceeding the {max}-byte maximum this configuration can \
         recover; reduce the tracked-file population or raise limits.max_tracked_files"
    )]
    SnapshotTooLarge {
        /// The namespace directory.
        dir: PathBuf,
        /// The generation that was being staged.
        generation: u64,
        /// How many records the snapshot holds.
        records: usize,
        /// The encoded snapshot's size.
        len: u64,
        /// The largest snapshot this configuration can recover.
        max: u64,
    },
    /// Appending the transaction would grow the live WAL past the largest
    /// WAL this configuration can read back, so it was refused before the
    /// in-memory table advanced.
    #[error(
        "refusing to append {transaction_bytes} bytes to the checkpoint WAL at {path}: it \
         already holds {wal_bytes} bytes and the maximum this configuration can recover is \
         {max} bytes; compact the namespace and retry"
    )]
    WalWouldExceedMaximum {
        /// The live WAL.
        path: PathBuf,
        /// Bytes the WAL already holds.
        wal_bytes: u64,
        /// Bytes the refused transaction would add.
        transaction_bytes: u64,
        /// The largest WAL this configuration can recover.
        max: u64,
    },
    /// A transaction could not fit even after compaction reset the WAL to
    /// its header and transaction count to zero. Configuration validation
    /// normally makes this unreachable.
    #[error(
        "checkpoint transaction of {transaction_bytes} bytes cannot fit the configured fresh WAL \
         at {path}: byte threshold {compact_after_bytes}, transaction threshold \
         {compact_after_transactions}"
    )]
    TransactionExceedsCompactionThreshold {
        /// Live WAL path.
        path: PathBuf,
        /// Encoded transaction frame bytes.
        transaction_bytes: u64,
        /// Complete-WAL byte threshold.
        compact_after_bytes: u64,
        /// Complete-transaction threshold.
        compact_after_transactions: u32,
    },
    /// A caller supplied more operations than one WAL transaction may carry.
    #[error(
        "transaction carries {operations} operations, exceeding the \
         {max}-operation maximum for a single transaction"
    )]
    TransactionTooLarge {
        /// The number of operations supplied.
        operations: usize,
        /// The format's per-transaction maximum.
        max: u16,
    },
    /// A caller tried to append a transaction with no operations, which the
    /// format forbids.
    #[error("refusing to append a checkpoint transaction with no operations")]
    EmptyTransaction,
    /// A transaction's registrations would push the durable record
    /// population past the configured `limits.max_tracked_files`, whose
    /// worth of worst-case records is exactly what the snapshot bound is
    /// sized for.
    #[error(
        "refusing to register {registrations} new files in {dir}: the namespace already tracks \
         {tracked} records and limits.max_tracked_files is {max}; remove or expire records, or \
         raise limits.max_tracked_files"
    )]
    TrackedFilesExhausted {
        /// The namespace directory.
        dir: PathBuf,
        /// Records the namespace already tracks.
        tracked: usize,
        /// New registrations the refused transaction carried.
        registrations: usize,
        /// The configured population maximum.
        max: u32,
    },
    /// Recovered state already exceeds the configured tracked-file
    /// population, so opening it would accept a limit reduction that the
    /// current configuration cannot represent safely.
    #[error(
        "checkpoint namespace {dir} holds {tracked} records, exceeding \
         limits.max_tracked_files ({max}); restore the previous limit or \
         administratively reduce the namespace before reopening"
    )]
    RecoveredTrackedFilesExceedMaximum {
        /// The checkpoint namespace.
        dir: PathBuf,
        /// Records recovered from the selected generation.
        tracked: usize,
        /// The configured population maximum.
        max: u32,
    },
    /// Recovery found more complete WAL transactions than the interacting
    /// configured thresholds permit.
    #[error(
        "checkpoint WAL at {path} contains {transactions} complete transactions, exceeding the \
         configured recovery maximum {max}"
    )]
    RecoveredWalTransactionsExceedMaximum {
        /// Selected WAL artifact.
        path: PathBuf,
        /// Complete transactions encountered.
        transactions: u64,
        /// Maximum admitted by byte/count thresholds.
        max: u64,
    },
    /// Runtime retention named a record that is not present in the live
    /// checkpoint table.
    #[error("runtime-vetted checkpoint retention record {file_id:?} is not tracked")]
    RetentionCandidateMissing {
        /// Missing durable identity.
        file_id: FileId,
    },
    /// Runtime retention attempted to remove durable quarantine.
    #[error("runtime-vetted checkpoint retention record {file_id:?} is quarantined")]
    RetentionCandidateQuarantined {
        /// Quarantined durable identity.
        file_id: FileId,
    },
    /// More recognized generations were present than recovery is willing
    /// to retain in memory or on disk.
    #[error(
        "checkpoint namespace {dir} contains more than {max} recognized generations; \
         remove obsolete generation pairs only after identifying the authoritative \
         generation from CURRENT"
    )]
    TooManyGenerations {
        /// The checkpoint namespace.
        dir: PathBuf,
        /// Maximum recognized generations allowed on disk.
        max: usize,
    },
    /// More abandoned temporary artifacts were present than one bounded
    /// store lifecycle can create.
    #[error(
        "checkpoint namespace {dir} contains more than {max} recognized temporary artifacts; \
         refusing unbounded recovery cleanup"
    )]
    TooManyTemporaryFiles {
        /// The checkpoint namespace.
        dir: PathBuf,
        /// Maximum recognized temporary files cleanup will process.
        max: usize,
    },
    /// Compaction was requested before the previous generation was cleaned
    /// up, which would permit retired artifacts to grow without bound.
    #[error(
        "checkpoint namespace {dir} still has retired generation {generation}; \
         clean up retired generations before compacting again"
    )]
    RetiredGenerationCleanupRequired {
        /// The checkpoint namespace.
        dir: PathBuf,
        /// Oldest generation still awaiting cleanup.
        generation: u64,
    },
    /// A fingerprint in a caller-supplied operation or recovered record is
    /// wider than this store's configured fingerprint window.
    #[error(
        "{context} for file {file_id:?} is {len} bytes, exceeding the configured \
         identity.fingerprint_bytes maximum of {max}; restore the previous limit \
         or migrate the checkpoint namespace explicitly"
    )]
    FingerprintExceedsConfiguredMaximum {
        /// Whether the value came from recovery or a caller operation.
        context: &'static str,
        /// The record or operation carrying the fingerprint.
        file_id: FileId,
        /// Actual fingerprint length.
        len: usize,
        /// Configured fingerprint maximum.
        max: u64,
    },
    /// Durable state recovered from disk carries the reserved reason code
    /// the format forbids an encoder from writing. Accepting it would make
    /// the next compaction re-encode it, so recovery fails closed instead.
    #[error(
        "checkpoint generation {generation} in {dir} holds a record ({file_id:?}) whose {field} \
         is the reserved value {reason_code:#06x}, which no encoder may write"
    )]
    ReservedReasonCodeRecovered {
        /// The namespace directory.
        dir: PathBuf,
        /// The generation the record was recovered from.
        generation: u64,
        /// The record carrying the reserved value.
        file_id: FileId,
        /// The durable field carrying it.
        field: &'static str,
        /// The reserved value.
        reason_code: u16,
    },
    /// The configured `checkpoint.id` cannot be represented by the durable
    /// format, so an administrative operation could never name this
    /// namespace correctly.
    #[error(
        "refusing to open the checkpoint namespace at {namespace_dir}: its id is invalid, {reason}"
    )]
    InvalidNamespaceId {
        /// The namespace directory that was refused.
        namespace_dir: PathBuf,
        /// Why the id cannot be used.
        reason: &'static str,
    },
    /// A durable write failed after the in-memory table had already
    /// advanced, or after `CURRENT` had already been repointed, so the store
    /// instance can no longer be trusted to mirror durable state. The
    /// namespace on disk remains recoverable; the store must be reopened.
    #[error(
        "checkpoint store for {dir} refused to {operation}: the store is \
         unusable because {reason}; reopen the namespace to recover"
    )]
    Unusable {
        /// The namespace directory.
        dir: PathBuf,
        /// The operation that was refused.
        operation: &'static str,
        /// Why the store became unusable.
        reason: &'static str,
    },
    /// The generation counter would overflow, so no new generation can be
    /// created.
    #[error("checkpoint generation counter would overflow past {generation}")]
    GenerationOverflow {
        /// The current generation.
        generation: u64,
    },
    /// An administrative publication attempted to reuse or move backward
    /// from a recognized generation number.
    #[error(
        "checkpoint generation {proposed} must be strictly greater than every recognized \
         generation (highest is {highest})"
    )]
    GenerationNotIncreasing {
        /// Proposed generation.
        proposed: u64,
        /// Highest generation already recognized by the locked session.
        highest: u64,
    },
    /// The WAL transaction sequence would overflow, so no further
    /// transaction can be appended to this generation.
    #[error("checkpoint WAL sequence would overflow past {sequence}; compaction is required")]
    SequenceOverflow {
        /// The sequence that could not be advanced.
        sequence: u64,
    },
    /// WAL byte accounting would overflow `u64`.
    #[error("checkpoint WAL byte accounting would overflow past {bytes} bytes")]
    AccountingOverflow {
        /// The accumulated byte count that could not be advanced.
        bytes: u64,
    },
    /// An in-memory durability counter would overflow.
    #[error("checkpoint {counter} counter would overflow past {value}")]
    CounterOverflow {
        /// Counter that could not be advanced.
        counter: &'static str,
        /// Current value that could not be incremented.
        value: u64,
    },
    /// An administrative operation was requested without the mandatory,
    /// non-empty audit reason the format requires.
    #[error("administrative {operation} requires a non-empty audit reason")]
    AuditReasonRequired {
        /// The administrative operation that was refused.
        operation: &'static str,
    },
    /// A quarantine-only administrative operation targeted a record that is
    /// no longer quarantined.
    #[error(
        "administrative {operation} requires file {file_id:?} to be quarantined, \
         but its current state is {state:?}"
    )]
    NotQuarantined {
        /// The administrative operation that was refused.
        operation: &'static str,
        /// The targeted checkpoint record.
        file_id: FileId,
        /// Its current lifecycle state.
        state: super::super::primitives::LifecycleState,
    },
    /// A reason code reserved by this format version was supplied.
    #[error("{field} must not use reserved reason code {reason_code:#06x}")]
    ReservedReasonCode {
        /// The durable field that was given the reserved value.
        field: &'static str,
        /// The reserved value.
        reason_code: u16,
    },
    /// An uncertain WAL append is awaiting an exact retry and no unrelated
    /// store operation may proceed until it is reconciled.
    #[error(
        "checkpoint WAL at {path} is reconciling transaction {sequence}; \
         refusing to {operation} until that exact append is retried"
    )]
    PendingWalAppend {
        /// The WAL whose final append has an uncertain result.
        path: PathBuf,
        /// The unrelated operation that was refused.
        operation: &'static str,
        /// Sequence of the transaction that must be retried.
        sequence: u64,
    },
    /// A caller retried a different transaction while an uncertain append
    /// still owns the next WAL sequence.
    #[error(
        "checkpoint WAL at {path} requires an exact retry of transaction {expected_sequence} \
         ({expected_bytes} bytes), but received transaction {found_sequence} \
         ({found_bytes} bytes)"
    )]
    PendingWalAppendMismatch {
        /// The WAL whose append is awaiting reconciliation.
        path: PathBuf,
        /// Sequence retained by the failed append.
        expected_sequence: u64,
        /// Encoded length retained by the failed append.
        expected_bytes: u64,
        /// Sequence supplied by the new append request.
        found_sequence: u64,
        /// Encoded length supplied by the new append request.
        found_bytes: u64,
    },
    /// Reopening a WAL after an uncertain append did not reproduce the exact
    /// known prefix plus either no transaction, one torn attempt, or the
    /// expected complete transaction.
    #[error(
        "checkpoint WAL append reconciliation at {path} disagreed with the known valid \
         boundary {boundary}: {reason}"
    )]
    WalAppendRecoveryMismatch {
        /// The WAL being reconciled.
        path: PathBuf,
        /// Byte offset immediately after the previously validated prefix.
        boundary: u64,
        /// Which exact recovery invariant failed.
        reason: &'static str,
    },
    /// A test armed a fault point and execution reached it. Production code
    /// has no way to arm a fault point (see [`super::fault::FaultPlan`]), so
    /// this variant is unreachable outside this crate's own tests.
    #[error("injected checkpoint fault at persistence boundary {point}")]
    InjectedFault {
        /// The boundary that was armed.
        point: FaultPoint,
    },
}
