// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Error types for the filelog checkpoint codec.
//!
//! [`DecodeError`] covers structural failures found while parsing raw bytes
//! (bad magic, unsupported version, checksum mismatch, a length exceeding a
//! bound, an unknown structural discriminant, invalid UTF-8, and arithmetic
//! that would overflow). [`EncodeError`] covers analogous failures while
//! writing, including an oversized field or a value the format reserves
//! encoders from producing. [`ApplyError`] covers semantic/business-rule failures found
//! while replaying an already-decoded operation against the in-memory
//! checkpoint table (a stale epoch, a conflicting record, an impossible
//! transition). Reserved reason codes remain structurally valid to decode.
//! Keeping these separate matches the specification's
//! distinction between decode-time structural failures and apply-time
//! semantic failures (see `docs/filelog-checkpoint-format.md`, "Reason
//! codes are not structural").
//!
//! [`DecodeError::DuplicateFileId`] is also reused by
//! [`super::apply::CheckpointTable::from_snapshot_records`], which does not
//! itself parse bytes but enforces the same "`file_id` is a unique key"
//! structural invariant this format's byte-level `decode_snapshot` enforces,
//! so a table seeded directly from already-decoded records stays fail-closed
//! too.

use super::primitives::FileId;

/// Structural failure while decoding checkpoint bytes.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DecodeError {
    /// Fewer bytes were available than a length-prefixed or fixed-width
    /// field required.
    #[error("truncated input: needed {needed} bytes, {available} available")]
    Truncated {
        /// Number of bytes the field required.
        needed: usize,
        /// Number of bytes actually remaining.
        available: usize,
    },
    /// A declared length exceeded the field's documented maximum.
    #[error("field {field} declared length {declared} exceeds maximum {max}")]
    LengthExceedsMaximum {
        /// The field name.
        field: &'static str,
        /// The declared length.
        declared: usize,
        /// The field's documented maximum.
        max: usize,
    },
    /// A fixed magic value did not match what this version expects.
    #[error("bad magic for {context}")]
    BadMagic {
        /// Which file/section the magic belongs to.
        context: &'static str,
    },
    /// `format_version` was not `1`.
    #[error("unsupported format_version {found} in {context}; migration required")]
    UnsupportedFormatVersion {
        /// Which file/section the version belongs to.
        context: &'static str,
        /// The version value actually found.
        found: u16,
    },
    /// A reserved bit or field that v1 requires to be zero was nonzero.
    #[error("reserved field {field} was nonzero: {value:#x}")]
    ReservedFieldNonZero {
        /// The field name.
        field: &'static str,
        /// The nonzero value found.
        value: u64,
    },
    /// A structural discriminant (one that governs subsequent byte layout)
    /// had an unrecognized value.
    #[error("unknown discriminant for {field}: {value:#x}")]
    UnknownDiscriminant {
        /// The discriminant field name.
        field: &'static str,
        /// The unrecognized value.
        value: u32,
    },
    /// A CRC-32C checksum did not match its covered bytes.
    #[error("CRC-32C mismatch in {context}: expected {expected:#010x}, computed {computed:#010x}")]
    ChecksumMismatch {
        /// Which frame the checksum belongs to.
        context: &'static str,
        /// The checksum stored on disk.
        expected: u32,
        /// The checksum recomputed over the actual bytes.
        computed: u32,
    },
    /// A length-prefixed byte field was declared to be valid UTF-8 by this
    /// format but was not.
    #[error("field {field} is not valid UTF-8")]
    InvalidUtf8 {
        /// The field name.
        field: &'static str,
    },
    /// A checked arithmetic operation needed to validate a length or offset
    /// would have overflowed.
    #[error("arithmetic overflow while computing {context}")]
    ArithmeticOverflow {
        /// What was being computed.
        context: &'static str,
    },
    /// A structurally complete record's declared length did not match the
    /// number of bytes its defined fields actually consumed (v1 has no
    /// extension-bytes mechanism; every declared length must be exact).
    #[error("{context} declared length {declared} but fields consumed {consumed}")]
    UnconsumedBytes {
        /// Which frame this applies to.
        context: &'static str,
        /// The declared length.
        declared: usize,
        /// The number of bytes the defined fields actually consumed.
        consumed: usize,
    },
    /// Bytes remained after a structurally complete, self-terminating
    /// container (a snapshot file, which -- unlike the WAL -- has no
    /// torn-tail tolerance).
    #[error("{context} has {remaining} trailing byte(s) after a complete parse")]
    TrailingBytes {
        /// Which container this applies to.
        context: &'static str,
        /// The number of unexpected trailing bytes.
        remaining: usize,
    },
    /// A WAL transaction sequence was not exactly one greater than the
    /// previous transaction's sequence.
    #[error("WAL sequence out of order: expected {expected}, found {found}")]
    SequenceOutOfOrder {
        /// The expected next sequence.
        expected: u64,
        /// The sequence actually found.
        found: u64,
    },
    /// A transaction declared zero operations, which this format forbids.
    #[error("transaction {sequence} declared zero operations")]
    EmptyTransaction {
        /// The transaction's sequence number.
        sequence: u64,
    },
    /// A transaction declared more operations than the format allows for
    /// its class (`WAL_MAX_OPS_PER_TX` for progress-only,
    /// `WAL_MAX_NON_PROGRESS_OPS_PER_TX` for non-progress).
    #[error("transaction {sequence} declared {op_count} operations, exceeding the maximum {max}")]
    TooManyOperations {
        /// The transaction's sequence number.
        sequence: u64,
        /// The declared operation count.
        op_count: u16,
        /// The maximum allowed for this transaction's class.
        max: u16,
    },
    /// A transaction's operations mixed `update_progress` with any other
    /// operation kind; every transaction must be either progress-only or
    /// non-progress.
    #[error("transaction {sequence} mixes update_progress with other operation kinds")]
    MixedTransactionClass {
        /// The transaction's sequence number.
        sequence: u64,
    },
    /// A transaction's encoded body exceeded `WAL_MAX_TX_BODY_BYTES`
    /// (16 MiB), or its declared `body_len` was outside
    /// `TX_MIN_BODY_BYTES..=WAL_MAX_TX_BODY_BYTES`.
    #[error("transaction {sequence} body is {len} bytes, exceeding the maximum {max}")]
    TransactionBodyTooLarge {
        /// The transaction's sequence number.
        sequence: u64,
        /// The declared or encoded body length.
        len: u64,
        /// The maximum allowed.
        max: u64,
    },
    /// A WAL transaction header's `body_len` and `body_len_complement`
    /// fields were not bitwise complements of one another.
    #[error("transaction {sequence} has an inconsistent body_len complement")]
    LengthComplementMismatch {
        /// The transaction's sequence number.
        sequence: u64,
    },
    /// A snapshot or WAL header's `namespace_digest` did not equal the
    /// expected digest for the selected namespace.
    #[error("{context} namespace_digest does not match the expected namespace")]
    NamespaceMismatch {
        /// Which artifact's header carried the mismatched digest.
        context: &'static str,
    },
    /// A CRC-valid, structurally well-formed snapshot record violated a
    /// documented reachable-state invariant (for example an epoch of `0`,
    /// an inconsistent committed-frontier guard, or a lifecycle/evidence
    /// mismatch). This is not a candidate for repair: the record is
    /// discarded and recovery fails closed.
    #[error("snapshot record for {file_id:?} violates a reachable-state invariant: {reason}")]
    InvalidSnapshotState {
        /// The file identity involved.
        file_id: FileId,
        /// A short, specific explanation.
        reason: &'static str,
    },
    /// A field this format documents as mandatory and non-empty (for
    /// example `reset_quarantined_file.audit_reason`, or
    /// `remove_file.namespace_id`/`audit_reason` when `administrative` is
    /// set) was declared with length zero.
    #[error("field {field} is required to be non-empty but was empty")]
    EmptyRequiredField {
        /// The field name.
        field: &'static str,
    },
    /// A field this format documents as absent under the current
    /// discriminant (for example `remove_file.namespace_id` when
    /// `administrative` is `0x00`) was declared with a nonzero length.
    #[error("field {field} must be absent here but declared a nonzero length")]
    UnexpectedPresentField {
        /// The field name.
        field: &'static str,
    },
    /// Two records (in a snapshot) declared the same `file_id`, or a
    /// `from_snapshot_records` caller supplied two records with the same
    /// `file_id`. `file_id` is the record key and MUST be unique within a
    /// single snapshot/table.
    #[error("duplicate file_id {file_id:?} in {context}")]
    DuplicateFileId {
        /// The file identity that appeared more than once.
        file_id: FileId,
        /// Which collection this duplicate was found in.
        context: &'static str,
    },
    /// An `AdvisoryPath` value violated one of its documented structural
    /// invariants: a reserved flag bit, an inconsistent kind/length/flag
    /// combination, or a digest that failed to recompute for a value this
    /// format requires to be verifiable (`Unavailable` or a complete,
    /// untruncated path).
    #[error("advisory path in {field} is structurally invalid: {reason}")]
    InvalidAdvisoryPath {
        /// The field name.
        field: &'static str,
        /// A short, specific explanation.
        reason: &'static str,
    },
}

/// Structural failure while encoding checkpoint bytes.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EncodeError {
    /// A field value exceeded its documented maximum encoded length.
    #[error("field {field} length {len} exceeds maximum {max}")]
    FieldTooLong {
        /// The field name.
        field: &'static str,
        /// The value's actual length.
        len: usize,
        /// The field's documented maximum.
        max: usize,
    },
    /// A field this format documents as mandatory and non-empty was
    /// constructed with an empty value.
    #[error("field {field} is required to be non-empty but was empty")]
    RequiredFieldEmpty {
        /// The field name.
        field: &'static str,
    },
    /// A quarantine or removal reason used a value this version reserves and
    /// forbids encoders from producing.
    #[error("field {field} uses reserved reason code {reason_code:#06x}")]
    ReservedReasonCode {
        /// The reason-code field that carried the reserved value.
        field: &'static str,
        /// The reserved value.
        reason_code: u16,
    },
    /// A transaction was constructed with zero operations, which this
    /// format forbids (mirrors `DecodeError::EmptyTransaction`).
    #[error("transaction {sequence} has zero operations")]
    EmptyTransaction {
        /// The transaction's sequence number.
        sequence: u64,
    },
    /// A transaction was constructed with more operations than its class
    /// allows (mirrors `DecodeError::TooManyOperations`).
    #[error("transaction {sequence} has {op_count} operations, exceeding the maximum {max}")]
    TooManyOperations {
        /// The transaction's sequence number.
        sequence: u64,
        /// The actual operation count.
        op_count: usize,
        /// The maximum allowed for this transaction's class.
        max: u16,
    },
    /// A transaction's operations mixed `update_progress` with any other
    /// operation kind (mirrors `DecodeError::MixedTransactionClass`).
    #[error("transaction {sequence} mixes update_progress with other operation kinds")]
    MixedTransactionClass {
        /// The transaction's sequence number.
        sequence: u64,
    },
    /// A transaction's encoded body exceeded `WAL_MAX_TX_BODY_BYTES`
    /// (16 MiB) (mirrors `DecodeError::TransactionBodyTooLarge`).
    #[error("transaction {sequence} body is {len} bytes, exceeding the maximum {max}")]
    TransactionBodyTooLarge {
        /// The transaction's sequence number.
        sequence: u64,
        /// The encoded body length.
        len: u64,
        /// The maximum allowed.
        max: u64,
    },
    /// Two records passed to `encode_snapshot` declared the same
    /// `file_id`; `file_id` must uniquely identify a record within a
    /// single snapshot.
    #[error("duplicate file_id {file_id:?} in snapshot records")]
    DuplicateFileId {
        /// The file identity that appeared more than once.
        file_id: FileId,
    },
    /// A record's `lifecycle_state` was `Quarantined` but it carried no
    /// `quarantine_evidence`; this format requires evidence to be present
    /// iff the state is `Quarantined`.
    #[error("record {file_id:?} is Quarantined but carries no quarantine_evidence")]
    MissingQuarantineEvidence {
        /// The file identity involved.
        file_id: FileId,
    },
    /// A record's `lifecycle_state` was not `Quarantined` but it carried
    /// `quarantine_evidence`; this format requires evidence to be absent
    /// unless the state is `Quarantined`.
    #[error("record {file_id:?} is not Quarantined but carries quarantine_evidence")]
    UnexpectedQuarantineEvidence {
        /// The file identity involved.
        file_id: FileId,
    },
    /// A record violates a documented reachable-state invariant (mirrors
    /// `DecodeError::InvalidSnapshotState`): the compaction/encode path
    /// enforces the same invariants replay enforces, so an in-memory state
    /// replay could never produce is refused at encode time too.
    #[error("snapshot record for {file_id:?} violates a reachable-state invariant: {reason}")]
    InvalidSnapshotState {
        /// The file identity involved.
        file_id: FileId,
        /// A short, specific explanation.
        reason: &'static str,
    },
    /// An `AdvisoryPath` value could not be constructed because the input
    /// violated a documented invariant (for example empty native path
    /// bytes/code units, or a length that overflows `u64`).
    #[error("advisory path is invalid: {reason}")]
    InvalidAdvisoryPath {
        /// A short, specific explanation.
        reason: &'static str,
    },
}

/// Semantic/business-rule failure while replaying a decoded WAL operation
/// against the in-memory checkpoint table. Distinct from [`DecodeError`]:
/// these failures only make sense once an operation has already decoded
/// successfully and is being checked against durable state.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ApplyError {
    /// `register_file` found an existing record with at least one differing
    /// field (not a benign identical-replay).
    #[error("register_file for {file_id:?} conflicts with an existing differing record")]
    ConflictingRegistration {
        /// The file identity involved.
        file_id: FileId,
    },
    /// An operation's precondition (expected state, expected epoch, or
    /// expected offset) did not match the current record, or named a record
    /// in a lifecycle state the operation cannot apply to.
    #[error("{operation} for {file_id:?} has an impossible transition: {reason}")]
    ImpossibleTransition {
        /// The operation name.
        operation: &'static str,
        /// The file identity involved.
        file_id: FileId,
        /// A short, specific explanation.
        reason: &'static str,
    },
    /// `update_progress`'s `new_committed_offset` was smaller than the
    /// expected (current) committed offset.
    #[error("update_progress for {file_id:?} would regress offset from {current} to {attempted}")]
    OffsetRegression {
        /// The file identity involved.
        file_id: FileId,
        /// The currently committed offset.
        current: u64,
        /// The offset the operation attempted to commit.
        attempted: u64,
    },
    /// An epoch increment (`reset_after_truncate` or
    /// `reset_quarantined_file`) would overflow `u32`.
    #[error("{operation} for {file_id:?} would overflow file_epoch")]
    EpochOverflow {
        /// The operation name.
        operation: &'static str,
        /// The file identity involved.
        file_id: FileId,
    },
    /// `quarantine_file` found an existing quarantined record whose fields
    /// differ from this operation's fields (not a benign identical replay).
    #[error("quarantine_file for {file_id:?} conflicts with an existing differing quarantine")]
    ConflictingQuarantine {
        /// The file identity involved.
        file_id: FileId,
    },
    /// `reset_after_truncate`'s `reason_code` was not
    /// `TRUNCATE_RESET_REASON_READ_NEW`.
    #[error("reset_after_truncate for {file_id:?} carried an invalid reason code {reason_code:#x}")]
    InvalidTruncateReason {
        /// The file identity involved.
        file_id: FileId,
        /// The reason code actually found.
        reason_code: u16,
    },
    /// `remove_file` administrative removal of a quarantined record named a
    /// `namespace_id` that does not match the namespace the WAL actually
    /// belongs to.
    #[error("remove_file for {file_id:?} named namespace {named:?}, expected {actual:?}")]
    NamespaceMismatch {
        /// The file identity involved.
        file_id: FileId,
        /// The namespace named by the operation.
        named: String,
        /// The namespace the WAL actually belongs to.
        actual: String,
    },
    /// A table record was in state `Quarantined` but carried no
    /// `quarantine_evidence`. This should be unreachable through
    /// `decode_snapshot`/`encode_snapshot` and normal replay (both enforce
    /// the invariant), but a table can also be seeded directly through
    /// `CheckpointTable::from_snapshot_records`; replay checks the
    /// invariant explicitly here rather than trusting an internal panic.
    #[error("{operation} found {file_id:?} in state Quarantined with no quarantine_evidence")]
    MissingQuarantineEvidence {
        /// The operation name that encountered the inconsistent record.
        operation: &'static str,
        /// The file identity involved.
        file_id: FileId,
    },
    /// An operation's `committed_frontier_guard` (or
    /// `new_committed_frontier_guard`) did not satisfy
    /// `window_len == min(committed_offset, 64)`.
    #[error("{operation} for {file_id:?} carries an invalid committed_frontier_guard: {reason}")]
    InvalidCommittedFrontierGuard {
        /// The operation name.
        operation: &'static str,
        /// The file identity involved.
        file_id: FileId,
        /// A short, specific explanation.
        reason: &'static str,
    },
    /// `reset_quarantined_file`'s `action == keep_failed` attempted to
    /// change any operational field of an already-quarantined record
    /// (epoch, offset, guard, or framing-resume state); `keep_failed` MUST
    /// be a byte-identical no-op besides the audit trail.
    #[error("reset_quarantined_file keep_failed for {file_id:?} would change stored state")]
    KeepFailedStateChange {
        /// The file identity involved.
        file_id: FileId,
    },
}
