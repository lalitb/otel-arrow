// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Replay errors owned by core-nodes.

use otel_arrow_dfe_filelog_checkpoint::FileId;

/// Semantic/business-rule failure while replaying a decoded WAL operation
/// against the in-memory checkpoint table. These failures only make sense
/// after structural decoding and while checking durable state.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ApplyError {
    /// `register_file` found an existing record with at least one differing
    /// field (not a benign identical replay).
    #[error("register_file for {file_id:?} conflicts with an existing differing record")]
    ConflictingRegistration {
        /// The file identity involved.
        file_id: FileId,
    },
    /// An operation's precondition did not match the current record.
    #[error("{operation} for {file_id:?} has an impossible transition: {reason}")]
    ImpossibleTransition {
        /// The operation name.
        operation: &'static str,
        /// The file identity involved.
        file_id: FileId,
        /// A short, specific explanation.
        reason: &'static str,
    },
    /// `update_progress` would move the durable offset backward.
    #[error("update_progress for {file_id:?} would regress offset from {current} to {attempted}")]
    OffsetRegression {
        /// The file identity involved.
        file_id: FileId,
        /// The currently committed offset.
        current: u64,
        /// The offset the operation attempted to commit.
        attempted: u64,
    },
    /// An epoch increment would overflow `u32`.
    #[error("{operation} for {file_id:?} would overflow file_epoch")]
    EpochOverflow {
        /// The operation name.
        operation: &'static str,
        /// The file identity involved.
        file_id: FileId,
    },
    /// An idempotent quarantine replay carried conflicting evidence.
    #[error("quarantine_file for {file_id:?} conflicts with an existing differing quarantine")]
    ConflictingQuarantine {
        /// The file identity involved.
        file_id: FileId,
    },
    /// A truncate reset carried a reason other than `read_new`.
    #[error("reset_after_truncate for {file_id:?} carried an invalid reason code {reason_code:#x}")]
    InvalidTruncateReason {
        /// The file identity involved.
        file_id: FileId,
        /// The reason code actually found.
        reason_code: u16,
    },
    /// An administrative operation named the wrong checkpoint namespace.
    #[error(
        "administrative operation for {file_id:?} named namespace {named:?}, expected {actual:?}"
    )]
    NamespaceMismatch {
        /// The file identity involved.
        file_id: FileId,
        /// The namespace named by the operation.
        named: String,
        /// The namespace the WAL actually belongs to.
        actual: String,
    },
    /// A quarantined record carried no quarantine evidence.
    #[error("{operation} found {file_id:?} in state Quarantined with no quarantine_evidence")]
    MissingQuarantineEvidence {
        /// The operation that encountered the inconsistent record.
        operation: &'static str,
        /// The file identity involved.
        file_id: FileId,
    },
    /// A frontier guard was not canonical for its paired offset.
    #[error("{operation} for {file_id:?} carries an invalid committed_frontier_guard: {reason}")]
    InvalidCommittedFrontierGuard {
        /// The operation name.
        operation: &'static str,
        /// The file identity involved.
        file_id: FileId,
        /// A short, specific explanation.
        reason: &'static str,
    },
    /// `keep_failed` attempted to change stored operational state.
    #[error("reset_quarantined_file keep_failed for {file_id:?} would change stored state")]
    KeepFailedStateChange {
        /// The file identity involved.
        file_id: FileId,
    },
    /// A progress transaction repeated one `file_id`.
    #[error("transaction contains duplicate update_progress for {file_id:?}")]
    DuplicateProgressFileId {
        /// The repeated progress key.
        file_id: FileId,
    },
    /// Two live records would claim one exact runtime locator.
    #[error(
        "one live locator would be claimed by both {existing_file_id:?} and {conflicting_file_id:?}"
    )]
    LiveLocatorConflict {
        /// Existing or first staged claimant.
        existing_file_id: FileId,
        /// Later staged claimant.
        conflicting_file_id: FileId,
    },
}
