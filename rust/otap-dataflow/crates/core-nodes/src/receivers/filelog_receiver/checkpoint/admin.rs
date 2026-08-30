// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Exclusive administration for one existing checkpoint namespace.
//!
//! Both session types validate the exact version-1 namespace path and raw ID,
//! require an existing namespace and ownership lock, and acquire the runtime's
//! exclusive operating-system lock. [`CheckpointAdminSession`] requires a
//! valid `CURRENT` authority for inspection and per-file mutations.
//! [`CheckpointEvidenceSession`] retains bounded validation failure evidence
//! when authority is corrupt or missing and exposes read-only validation and
//! backup. Read-only inspection and backup preserve source artifacts
//! byte-for-byte. Explicit per-file mutations construct audited WAL operations
//! internally.

use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::current_marker::decode_current_marker;
use super::error::EncodeError;
use super::namespace::{CheckpointNamespace, CheckpointNamespaceError};
use super::primitives::{
    AUDIT_REASON_MAX_BYTES, AdvisoryPath, AdvisoryPathKind, CommittedFrontierGuard, FileId,
    FramingResume, LifecycleState, REASON_CODE_RESERVED,
};
use super::snapshot::SnapshotRecord;
use super::store::error::StoreError;
use super::store::fault::FaultPlan;
#[cfg(test)]
use super::store::fault::FaultPoint;
use super::store::fsio;
use super::store::layout::{
    self, ArtifactForm, CURRENT_FILE_NAME, MAX_GENERATIONS_ON_DISK, MAX_TEMP_FILES,
    NamespaceArtifactKind, canonical_artifact_name_ignoring_ascii_case,
    classify_namespace_artifact, snapshot_file_name, wal_file_name,
};
use super::store::limits::StoreLimits;
use super::store::lock::NamespaceLock;
use super::store::{CheckpointStore, LoadedGeneration, MARKER_READ_MAX_BYTES, StoreOptions};
use super::wal::{Operation, RemoveFile, ResetQuarantineAction, ResetQuarantinedFile};
use crate::receivers::filelog_receiver::identity::IdentityError;
use crate::receivers::filelog_receiver::identity::platform::{
    StableEofEvidence, open_stable_eof, open_stable_fingerprint,
};

/// File name of the machine-readable evidence-backup manifest.
pub const EVIDENCE_BACKUP_MANIFEST_FILE_NAME: &str = "manifest.json";
/// Version of the evidence-backup manifest schema.
pub const EVIDENCE_BACKUP_MANIFEST_VERSION: u16 = 1;
/// Maximum UTF-8 bytes retained from one authority-validation failure.
pub const NAMESPACE_VALIDATION_DETAIL_MAX_BYTES: usize = 4_096;

/// An administration, mutation, or evidence-backup failure.
#[derive(Debug, thiserror::Error)]
pub enum CheckpointAdminError {
    /// The raw checkpoint namespace ID violated the shared namespace
    /// contract.
    #[error(transparent)]
    Namespace(#[from] CheckpointNamespaceError),
    /// The supplied namespace path did not use the exact version-1 derived
    /// suffix for its raw ID.
    #[error(
        "checkpoint administration path {path} does not end with the required derived suffix \
         {expected_suffix}"
    )]
    NamespacePathMismatch {
        /// Supplied namespace directory.
        path: PathBuf,
        /// Required `filelog/@v1/<checkpoint-id-hex>` suffix.
        expected_suffix: PathBuf,
    },
    /// A bounded store validation or filesystem safety check failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A required already-published checkpoint artifact was absent.
    #[error("checkpoint administration requires existing {artifact} at {path}")]
    RequiredArtifactMissing {
        /// Required artifact role.
        artifact: &'static str,
        /// Expected artifact path.
        path: PathBuf,
    },
    /// A decoded quarantined row lacked its required immutable evidence.
    #[error("quarantined checkpoint file {file_id} has no quarantine evidence")]
    MissingQuarantineEvidence {
        /// Lowercase-hex file ID.
        file_id: String,
    },
    /// A bounded count could not be represented in the stable report
    /// schema.
    #[error("checkpoint {field} count does not fit u64")]
    CountOverflow {
        /// Report field that overflowed.
        field: &'static str,
    },
    /// The requested backup destination already existed.
    #[error("checkpoint evidence-backup destination already exists: {path}")]
    BackupDestinationExists {
        /// Existing path that was refused.
        path: PathBuf,
    },
    /// The requested backup destination was inside the source namespace.
    #[error(
        "checkpoint evidence-backup destination {destination} must not be inside source namespace \
         {source_namespace}"
    )]
    BackupDestinationInsideNamespace {
        /// Source namespace held by this session.
        source_namespace: PathBuf,
        /// Refused destination.
        destination: PathBuf,
    },
    /// A recognized source artifact disappeared after the locked inventory.
    #[error("recognized checkpoint backup artifact disappeared before it could be copied: {path}")]
    BackupArtifactDisappeared {
        /// Source artifact path.
        path: PathBuf,
    },
    /// A destination-side filesystem operation failed.
    #[error("failed to {operation} at {path}: {source}")]
    BackupIo {
        /// Backup step that failed.
        operation: &'static str,
        /// Destination or source directory involved.
        path: PathBuf,
        /// Underlying operating-system error.
        #[source]
        source: std::io::Error,
    },
    /// The bounded backup manifest could not be serialized.
    #[error("failed to serialize checkpoint evidence-backup manifest: {source}")]
    ManifestEncode {
        /// JSON serialization failure.
        #[source]
        source: serde_json::Error,
    },
    /// A native path could not be converted into its bounded report form.
    #[error("failed to encode native checkpoint path {path}: {source}")]
    NativePathEncode {
        /// Internal path that could not be represented.
        path: PathBuf,
        /// Native-path evidence encoding failure.
        #[source]
        source: EncodeError,
    },
    /// The target does not expose one of the supported native path
    /// representations.
    #[error("native checkpoint path reporting is unsupported on this platform: {path}")]
    NativePathUnsupported {
        /// Internal path that could not be represented.
        path: PathBuf,
    },
    /// A recognized artifact used ASCII case that did not exactly match the
    /// checkpoint format's canonical spelling.
    #[error(
        "checkpoint artifact {path} is not canonically named; expected file name \
         {canonical_name}"
    )]
    NonCanonicalArtifactName {
        /// Noncanonical source artifact path.
        path: PathBuf,
        /// Required byte-for-byte canonical ASCII file name.
        canonical_name: String,
    },
    /// A caller-supplied file ID was not exactly 32 lowercase hexadecimal
    /// characters.
    #[error("checkpoint file_id must be exactly 32 lowercase hexadecimal characters")]
    InvalidFileId,
    /// A mutation's audit reason was empty.
    #[error("checkpoint administrative {operation} requires a non-empty audit reason")]
    AuditReasonRequired {
        /// Administrative operation that was refused.
        operation: &'static str,
    },
    /// A mutation's audit reason exceeded the checkpoint-format bound.
    #[error(
        "checkpoint administrative {operation} audit reason is {len} bytes, exceeding the \
         {max}-byte maximum"
    )]
    AuditReasonTooLong {
        /// Administrative operation that was refused.
        operation: &'static str,
        /// Actual UTF-8 byte length.
        len: usize,
        /// Checkpoint-format maximum.
        max: usize,
    },
    /// The exact requested file ID is absent from the locked authority.
    #[error("checkpoint file {file_id} is absent from the locked namespace")]
    FileNotFound {
        /// Lowercase-hex file ID.
        file_id: String,
    },
    /// A quarantine-only mutation found another lifecycle state.
    #[error("checkpoint file {file_id} must be quarantined for {operation}, but is {state:?}")]
    ExpectedQuarantine {
        /// Administrative operation that was refused.
        operation: &'static str,
        /// Lowercase-hex file ID.
        file_id: String,
        /// Locked current lifecycle.
        state: CheckpointLifecycleReport,
    },
    /// The supplied quarantine epoch was stale.
    #[error(
        "checkpoint file {file_id} quarantine epoch mismatch: expected {expected}, current {actual}"
    )]
    QuarantineEpochMismatch {
        /// Lowercase-hex file ID.
        file_id: String,
        /// Caller-supplied epoch.
        expected: u32,
        /// Locked current quarantine epoch.
        actual: u32,
    },
    /// Incrementing a quarantined file epoch would overflow.
    #[error("checkpoint file {file_id} epoch {epoch} cannot be incremented")]
    FileEpochOverflow {
        /// Lowercase-hex file ID.
        file_id: String,
        /// Current quarantine epoch.
        epoch: u32,
    },
    /// The selected generation changed while this exclusive session was
    /// live.
    #[error(
        "checkpoint authority changed while locked: expected generation {expected}, found {found}"
    )]
    AuthorityChanged {
        /// Generation held by the live session.
        expected: u64,
        /// Generation read from `CURRENT`.
        found: u64,
    },
    /// The authoritative table or WAL sequence changed behind the retained
    /// lock.
    #[error("checkpoint authority state changed while the administration lock was retained")]
    AuthorityStateChanged,
    /// Reset-to-end could not open or read the exact operator-supplied path.
    #[error("failed to {operation} at reset-to-end source {path}: {source}")]
    ResetSourceIo {
        /// Source operation.
        operation: &'static str,
        /// Exact operator-supplied path.
        path: PathBuf,
        /// Underlying operating-system error.
        #[source]
        source: std::io::Error,
    },
    /// Reset-to-end reached a nonregular object.
    #[error("reset-to-end source is not a regular file: {path}")]
    ResetSourceNotRegular {
        /// Exact operator-supplied path.
        path: PathBuf,
    },
    /// Reset-to-end refused a symlink or Windows reparse point under
    /// no-follow policy.
    #[error("reset-to-end source is a symlink or reparse point: {path}")]
    ResetSourceSymlinkOrReparse {
        /// Exact operator-supplied path.
        path: PathBuf,
    },
    /// Reset-to-end opened a locator other than the immutable quarantine
    /// locator.
    #[error("reset-to-end source locator does not match quarantined file {file_id}")]
    ResetSourceLocatorMismatch {
        /// Lowercase-hex checkpoint file ID.
        file_id: String,
        /// Immutable locator held in quarantine.
        expected: LocatorReport,
        /// Locator reached through the supplied path.
        found: LocatorReport,
    },
    /// Reset-to-end evidence changed while its bounded sample was read.
    #[error("reset-to-end source changed while EOF evidence was sampled: {path}")]
    ResetSourceChanged {
        /// Exact operator-supplied path.
        path: PathBuf,
    },
    /// Reset-to-end is unsupported on the current target.
    #[error("reset-to-end source identity is unsupported on this platform: {path}")]
    ResetSourceUnsupported {
        /// Exact operator-supplied path.
        path: PathBuf,
    },
    /// An unexpected identity-layer validation failure occurred.
    #[error("reset-to-end source validation failed at {path}: {reason}")]
    ResetSourceValidation {
        /// Exact operator-supplied path.
        path: PathBuf,
        /// Bounded diagnostic category.
        reason: &'static str,
    },
    /// A freshly written backup no longer matches its manifest or source.
    #[error("checkpoint evidence backup verification failed at {path}: {reason}")]
    BackupVerification {
        /// Backup or source path involved.
        path: PathBuf,
        /// Exact bounded verification failure.
        reason: &'static str,
    },
}

/// Native path encoding used by bounded administration reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativePathKindReport {
    /// Native Unix `OsStr` bytes.
    UnixBytes,
    /// Native Windows UTF-16 code units serialized little-endian.
    WindowsUtf16Le,
}

/// Bounded native path included in serializable administration reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativePathReport {
    /// Native representation used for the path.
    pub kind: NativePathKindReport,
    /// Whether only the bounded suffix is stored.
    pub truncated: bool,
    /// Full native representation length in bytes.
    pub full_path_len: u64,
    /// Lowercase-hex stored bytes, bounded by the checkpoint path limit.
    pub stored_path_hex: String,
    /// Lowercase-hex digest of the complete native path representation.
    pub full_path_digest: String,
}

/// Bounded validation summary for one authoritative checkpoint generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamespaceValidationReport {
    /// Exact raw `checkpoint.id`.
    pub namespace_id: String,
    /// Bounded native representation of the canonical version-1 namespace
    /// path.
    pub derived_namespace_path: NativePathReport,
    /// Generation selected by the valid existing `CURRENT`.
    pub selected_generation: u64,
    /// Records decoded from the authoritative snapshot.
    pub snapshot_record_count: u64,
    /// Complete authoritative WAL transactions replayed.
    pub wal_transaction_count: u64,
    /// Records present after WAL replay.
    pub tracked_file_count: u64,
    /// Quarantined records present after WAL replay.
    pub quarantine_count: u64,
    /// Structurally incomplete bytes in the allowed final WAL tail.
    pub torn_wal_tail_bytes: u64,
    /// Other recognized final generation numbers present in the namespace.
    pub retired_generations: Vec<u64>,
}

/// Bounded category for an invalid checkpoint namespace authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NamespaceAuthorityFailureKind {
    /// The canonical `CURRENT` marker is absent.
    MissingCurrent,
    /// The canonical `CURRENT` marker could not be safely read.
    CurrentUnreadable,
    /// The canonical `CURRENT` marker bytes are invalid.
    CurrentInvalid,
    /// The generation named by `CURRENT` is absent, corrupt, or incompatible.
    SelectedGenerationInvalid,
    /// The bounded recognized-generation inventory is invalid.
    GenerationInventoryInvalid,
    /// A validated authority could not be represented in the report schema.
    ReportInvalid,
}

/// Bounded validation failure retained in inspection and evidence backup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamespaceAuthorityFailureReport {
    /// Stable failure category.
    pub kind: NamespaceAuthorityFailureKind,
    /// Generation decoded from `CURRENT`, when that decoding succeeded.
    pub selected_generation: Option<u64>,
    /// Bounded human-readable detail from the exact failed validation.
    pub detail: String,
    /// Whether `detail` was shortened to its documented bound.
    pub detail_truncated: bool,
}

/// Result of validating the namespace authority under exclusive ownership.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum NamespaceAuthorityReport {
    /// `CURRENT` and its selected snapshot/WAL pair decoded completely.
    Valid {
        /// Complete bounded authority validation.
        validation: NamespaceValidationReport,
    },
    /// Authority is missing, corrupt, incompatible, or otherwise invalid.
    Invalid {
        /// Bounded failure evidence. Raw artifacts remain available to a
        /// separately verified evidence backup.
        failure: NamespaceAuthorityFailureReport,
    },
}

/// Serializable framing-resume evidence for a quarantined record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum FramingResumeReport {
    /// No split logical record is in progress.
    Clean,
    /// A split logical record must resume from the recorded fragment index.
    Continuation {
        /// Original logical-record start offset.
        record_start_offset: u64,
        /// Known record end, or zero for scan-to-LF continuation.
        record_end_offset: u64,
        /// Next fragment index.
        next_fragment_index: u32,
    },
}

/// Serializable platform-neutral locator evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LocatorReport {
    /// No locator was recorded.
    Unspecified,
    /// POSIX device and inode.
    PosixDevIno {
        /// Device identifier.
        dev: u64,
        /// Inode number.
        ino: u64,
    },
    /// Windows volume serial and 128-bit file ID.
    WindowsVolumeFileId {
        /// Volume serial number.
        volume_serial: u64,
        /// Lowercase-hex 128-bit file ID.
        file_id: String,
    },
}

/// Serializable advisory-path encoding kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdvisoryPathKindReport {
    /// No advisory path is available.
    Unavailable,
    /// Native Unix path bytes.
    UnixBytes,
    /// Native Windows UTF-16LE bytes.
    WindowsUtf16Le,
}

/// Bounded advisory-path evidence exposed by administration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdvisoryPathReport {
    /// Native path encoding kind.
    pub kind: AdvisoryPathKindReport,
    /// Whether only the bounded suffix is stored.
    pub truncated: bool,
    /// Full native representation length.
    pub full_path_len: u64,
    /// Lowercase-hex stored bytes, bounded by the checkpoint format.
    pub stored_path_hex: String,
    /// Lowercase-hex domain-separated digest of the complete native path.
    pub full_path_digest: String,
}

/// Bounded inspection report for one quarantined checkpoint record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuarantineInspectionReport {
    /// Lowercase-hex opaque file ID.
    pub file_id: String,
    /// Current file epoch.
    pub epoch: u32,
    /// Ack-committed source-byte offset.
    pub committed_offset: u64,
    /// Durable framing-resume state.
    pub framing_resume: FramingResumeReport,
    /// Immutable runtime locator.
    pub locator: LocatorReport,
    /// Opaque quarantine reason code.
    pub reason_code: u16,
    /// File size observed when quarantine was recorded.
    pub observed_size: u64,
    /// Epoch recorded in immutable quarantine evidence.
    pub quarantine_epoch: u32,
    /// Quarantine timestamp in Unix nanoseconds.
    pub quarantine_time_unix_nano: u64,
    /// Bounded current advisory-path evidence.
    pub advisory_path: AdvisoryPathReport,
}

/// Complete read-only checkpoint inspection result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointInspectionReport {
    /// Namespace validation and recovery summary.
    pub validation: NamespaceValidationReport,
    /// Quarantined rows ordered by lowercase-hex file ID.
    pub quarantines: Vec<QuarantineInspectionReport>,
}

/// Role of one copied evidence-backup artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceArtifactRole {
    /// Published `CURRENT`.
    Current,
    /// `CURRENT.create.tmp` or `CURRENT.compact.tmp`.
    CurrentTemporary,
    /// Published generation snapshot.
    Snapshot,
    /// Snapshot `.create.tmp` or `.compact.tmp` artifact.
    SnapshotTemporary,
    /// Published generation WAL.
    Wal,
    /// WAL `.create.tmp` or `.compact.tmp` artifact.
    WalTemporary,
}

/// Manifest entry for one copied checkpoint artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceArtifact {
    /// Recognized artifact role.
    pub role: EvidenceArtifactRole,
    /// Exact ASCII source file name.
    pub name: String,
    /// Generation encoded in snapshot/WAL names.
    pub generation: Option<u64>,
    /// Copied byte length.
    pub length: u64,
    /// Lowercase-hex SHA-256 over the copied bytes.
    pub sha256: String,
}

/// Machine-readable evidence-backup manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceBackupManifest {
    /// Manifest schema version.
    pub manifest_version: u16,
    /// Exact raw `checkpoint.id`.
    pub namespace_id: String,
    /// Bounded native representation of the source namespace held under the
    /// retained exclusive lock.
    pub source_namespace: NativePathReport,
    /// Copied recognized artifacts, ordered by file name.
    pub artifacts: Vec<EvidenceArtifact>,
    /// Valid authority summary or the bounded reason authority validation
    /// failed.
    pub authority: NamespaceAuthorityReport,
}

/// Bounded audit metadata attached to one administrative mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditMetadata {
    /// Nonempty operator-supplied reason, bounded by
    /// `AUDIT_REASON_MAX_BYTES`.
    pub reason: String,
    /// Operator action time in Unix nanoseconds.
    pub action_time_unix_nano: u64,
}

/// Compile-time restriction for APIs that only accept quarantined state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExpectedQuarantineState {
    /// The caller expects the locked record to remain quarantined.
    Quarantined,
}

/// Exact optimistic-concurrency evidence for one quarantined record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuarantinedFileTarget {
    /// Exactly 32 lowercase hexadecimal characters encoding 16 bytes.
    pub file_id: String,
    /// Explicit expected lifecycle.
    pub expected_lifecycle: ExpectedQuarantineState,
    /// Exact immutable quarantine epoch the caller inspected.
    pub expected_quarantine_epoch: u32,
}

/// Request to release a quarantine at source offset zero after validating
/// replacement-stream fingerprint evidence from an exact-locator handle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResetToBeginningRequest {
    /// Exact quarantined record and expected evidence.
    pub target: QuarantinedFileTarget,
    /// Exact source path selected by the operator for this call.
    pub source_path: PathBuf,
    /// Whether the final path component may be a symlink or reparse point.
    pub follow_symlinks: bool,
    /// Bounded operator audit metadata.
    pub audit: AuditMetadata,
}

/// Request to retain a quarantine while durably recording the decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeepFailedRequest {
    /// Exact quarantined record and expected evidence.
    pub target: QuarantinedFileTarget,
    /// Bounded operator audit metadata.
    pub audit: AuditMetadata,
}

/// Request to remove one exact quarantined record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoveQuarantinedRequest {
    /// Exact quarantined record and expected evidence.
    pub target: QuarantinedFileTarget,
    /// Nonzero opaque removal reason persisted in the WAL.
    pub removal_reason: u16,
    /// Explicit acknowledgement that later registration may duplicate
    /// already delivered bytes or exclude existing bytes.
    pub consequence: RemovalConsequence,
    /// Bounded operator audit metadata.
    pub audit: AuditMetadata,
}

/// Explicit operator acknowledgement required for quarantine removal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemovalConsequence {
    /// The operator accepts that later registration can either replay
    /// previously delivered bytes or skip existing bytes.
    AcknowledgeDuplicateOrLossPossible,
}

/// Request to release a quarantine at a handle-verified current EOF.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResetToEndRequest {
    /// Exact quarantined record and expected evidence.
    pub target: QuarantinedFileTarget,
    /// Exact source path selected by the operator for this call.
    pub source_path: PathBuf,
    /// Whether the final path component may be a symlink or reparse point.
    pub follow_symlinks: bool,
    /// Bounded operator audit metadata.
    pub audit: AuditMetadata,
}

/// Observable data consequence of one administrative mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataEffect {
    /// No operational checkpoint state changed.
    None,
    /// Previously delivered source bytes may be delivered again.
    DuplicatePossible,
    /// The operator explicitly accepted a possibility of excluding source
    /// bytes.
    LossAccepted,
    /// The operator explicitly accepted that later registration may either
    /// replay previously delivered bytes or exclude existing bytes.
    DuplicateOrLossPossible,
}

/// Lifecycle value used by mutation reports, including record absence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointLifecycleReport {
    /// No durable record exists.
    Absent,
    /// The durable record is active.
    Active,
    /// The durable record is rotation-finalized.
    RotatedFinalized,
    /// The durable record remains quarantined.
    Quarantined,
}

/// Administrative per-file action recorded by a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuarantineMutationAction {
    /// Release at offset zero.
    ResetToBeginning,
    /// Release at sampled EOF.
    ResetToEnd,
    /// Retain quarantine and append only audit history.
    KeepFailed,
    /// Delete the matching quarantined record.
    RemoveQuarantined,
}

/// Digest-only committed-frontier evidence safe for operator output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommittedFrontierGuardReport {
    /// Raw-source window width covered by the digest.
    pub window_len: u16,
    /// Lowercase-hex SHA-256 digest. Raw source bytes are never exposed.
    pub digest: String,
}

/// Bounded source evidence used by a successful reset-to-end.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResetToEndEvidenceReport {
    /// Exact operator-supplied source path in bounded native encoding.
    pub source_path: NativePathReport,
    /// Explicit path-following policy used for the open.
    pub follow_symlinks: bool,
    /// Handle-derived locator matched against immutable quarantine state.
    pub locator: LocatorReport,
    /// Stable EOF committed by the reset.
    pub eof_offset: u64,
    /// Digest-only real trailing source evidence committed with EOF.
    pub committed_frontier_guard: CommittedFrontierGuardReport,
}

/// Serializable result of one audited per-file mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMutationResult {
    /// Exact raw `checkpoint.id`.
    pub namespace_id: String,
    /// Exact version-1 namespace path in bounded native encoding.
    pub namespace_path: NativePathReport,
    /// Exact lowercase-hex file ID.
    pub file_id: String,
    /// Authoritative generation whose WAL carries the action.
    pub generation: u64,
    /// Administrative action that was durably applied.
    pub action: QuarantineMutationAction,
    /// Lifecycle before the action.
    pub old_lifecycle: CheckpointLifecycleReport,
    /// Lifecycle after the action.
    pub new_lifecycle: CheckpointLifecycleReport,
    /// File epoch before the action.
    pub old_epoch: u32,
    /// File epoch after the action, or `None` after removal.
    pub new_epoch: Option<u32>,
    /// Committed offset before the action.
    pub old_offset: u64,
    /// Committed offset after the action, or `None` after removal.
    pub new_offset: Option<u64>,
    /// Sequence of the synced WAL transaction carrying the action.
    pub wal_sequence: u64,
    /// Exact bounded audit metadata persisted by the action.
    pub audit: AuditMetadata,
    /// Classified data consequence.
    pub data_effect: DataEffect,
    /// Clear bounded explanation of the consequence.
    pub consequence: String,
    /// Handle-bound source evidence for reset-to-end only.
    pub reset_to_end_evidence: Option<ResetToEndEvidenceReport>,
}

/// Exclusive evidence session that remains operable when `CURRENT` or
/// its selected generation is corrupt.
///
/// The session never permits per-file mutations because invalid authority
/// cannot prove a file's current state or epoch. It can only report authority
/// and preserve a bounded create-only evidence backup.
#[derive(Debug)]
pub struct CheckpointEvidenceSession {
    options: StoreOptions,
    limits: StoreLimits,
    namespace: fsio::DirectoryPathBinding,
    lock: Option<NamespaceLock>,
    authority: NamespaceAuthorityReport,
}

/// Exclusive administration session for one existing namespace.
#[derive(Debug)]
pub struct CheckpointAdminSession {
    options: StoreOptions,
    limits: StoreLimits,
    namespace: fsio::DirectoryPathBinding,
    lock: Option<NamespaceLock>,
    loaded: Option<LoadedGeneration>,
    store: Option<CheckpointStore>,
    generation: u64,
    retired_generations: Vec<u64>,
    faults: FaultPlan,
    unusable: Option<&'static str>,
    inspection: CheckpointInspectionReport,
}

#[derive(Debug)]
struct LockedNamespace {
    options: StoreOptions,
    limits: StoreLimits,
    namespace: fsio::DirectoryPathBinding,
    lock: NamespaceLock,
}

fn open_locked_namespace(
    mut options: StoreOptions,
) -> Result<LockedNamespace, CheckpointAdminError> {
    let namespace_suffix =
        CheckpointNamespace::derive(Path::new(""), &options.namespace_id)?.into_directory();
    if !options.namespace_dir.ends_with(&namespace_suffix) {
        return Err(CheckpointAdminError::NamespacePathMismatch {
            path: options.namespace_dir.clone(),
            expected_suffix: namespace_suffix.clone(),
        });
    }
    let limits = options.limits()?;
    let namespace = fsio::DirectoryPathBinding::open_canonical(
        &options.namespace_dir,
        "resolve the existing checkpoint namespace directory",
    )?;
    options.namespace_dir = namespace.path().to_path_buf();
    if !options.namespace_dir.ends_with(&namespace_suffix) {
        return Err(CheckpointAdminError::NamespacePathMismatch {
            path: options.namespace_dir.clone(),
            expected_suffix: namespace_suffix,
        });
    }
    let lock = NamespaceLock::acquire_existing(
        &options.namespace_dir,
        options.ownership_timeout,
        options.ownership_retry_interval,
    )?;
    Ok(LockedNamespace {
        options,
        limits,
        namespace,
        lock,
    })
}

impl CheckpointEvidenceSession {
    /// Opens an existing namespace for bounded validation and evidence backup.
    ///
    /// Unlike [`CheckpointAdminSession::open`], this entry point retains the
    /// exclusive lock when `CURRENT` or its selected authority is invalid.
    /// The failure is reported as bounded evidence, and no per-file mutation
    /// API is exposed.
    pub fn open(options: StoreOptions) -> Result<Self, CheckpointAdminError> {
        Self::open_inner(options)
    }

    fn open_inner(options: StoreOptions) -> Result<Self, CheckpointAdminError> {
        let LockedNamespace {
            options,
            limits,
            namespace,
            lock,
        } = open_locked_namespace(options)?;
        let authority = probe_namespace_authority(&options, &limits, &namespace, &lock)?;
        verify_source_binding(&namespace, &lock)?;
        Ok(Self {
            options,
            limits,
            namespace,
            lock: Some(lock),
            authority,
        })
    }

    /// Valid authority summary or bounded validation failure observed at
    /// session open.
    #[must_use]
    pub fn authority(&self) -> &NamespaceAuthorityReport {
        &self.authority
    }

    /// Copies every canonical recognized bounded artifact to a new
    /// create-only destination and records either valid authority or the
    /// exact bounded validation failure.
    pub fn backup(
        &self,
        destination: impl AsRef<Path>,
    ) -> Result<EvidenceBackupManifest, CheckpointAdminError> {
        self.revalidate_authority()?;
        let sources = self.inventory_sources()?;
        let completed = create_evidence_backup(
            &self.options,
            &self.namespace,
            self.active_lock(),
            destination.as_ref(),
            sources,
            self.authority.clone(),
        )?;
        completed.verify()?;
        let _ = verify_source_matches_backup(
            &self.options,
            &self.limits,
            &self.namespace,
            self.active_lock(),
            &completed.manifest,
            None,
        )?;
        Ok(completed.manifest)
    }

    /// Releases the exclusive namespace lock and reports an unlock failure.
    pub fn release(mut self) -> Result<(), CheckpointAdminError> {
        self.lock
            .take()
            .expect("an evidence session retains its namespace lock")
            .release()
            .map_err(CheckpointAdminError::from)
    }

    fn active_lock(&self) -> &NamespaceLock {
        self.lock
            .as_ref()
            .expect("an evidence session retains its namespace lock")
    }

    fn revalidate_authority(&self) -> Result<(), CheckpointAdminError> {
        let current = probe_namespace_authority(
            &self.options,
            &self.limits,
            &self.namespace,
            self.active_lock(),
        )?;
        if current != self.authority {
            return Err(CheckpointAdminError::AuthorityStateChanged);
        }
        Ok(())
    }

    fn inventory_sources(&self) -> Result<Vec<BackupSourceArtifact>, CheckpointAdminError> {
        with_verified_source(&self.namespace, self.active_lock(), || {
            inventory_recognized_artifacts(&self.options.namespace_dir, &self.limits)
        })
    }
}

impl CheckpointAdminSession {
    /// Opens and boundedly validates an existing namespace without repairing
    /// or mutating any source artifact.
    pub fn open(options: StoreOptions) -> Result<Self, CheckpointAdminError> {
        Self::open_inner(options, FaultPlan::disabled())
    }

    #[cfg(test)]
    pub(crate) fn open_with_fault(
        options: StoreOptions,
        point: FaultPoint,
    ) -> Result<Self, CheckpointAdminError> {
        Self::open_inner(options, FaultPlan::armed(point))
    }

    fn open_inner(options: StoreOptions, faults: FaultPlan) -> Result<Self, CheckpointAdminError> {
        let LockedNamespace {
            options,
            limits,
            namespace,
            lock,
        } = open_locked_namespace(options)?;

        let marker_path = options.namespace_dir.join(CURRENT_FILE_NAME);
        let marker_bytes = with_verified_source(&namespace, &lock, || {
            fsio::read_file_bounded_read_only(
                &marker_path,
                "CURRENT marker",
                MARKER_READ_MAX_BYTES,
            )?
            .ok_or_else(|| CheckpointAdminError::RequiredArtifactMissing {
                artifact: "CURRENT marker",
                path: marker_path.clone(),
            })
        })?;
        let generation =
            decode_current_marker(&marker_bytes).map_err(|source| StoreError::Decode {
                artifact: "CURRENT marker",
                path: marker_path,
                source,
            })?;

        let loaded = with_verified_source(&namespace, &lock, || {
            Ok(CheckpointStore::load_generation_read_only(
                &options.namespace_dir,
                generation,
                &options.namespace_id,
                &limits,
                options.max_tracked_files,
                options.fingerprint_bytes,
            )?)
        })?;
        let generations = with_verified_source(&namespace, &lock, || {
            Ok(layout::scan_generations_read_only(&options.namespace_dir)?)
        })?;
        let retired_generations: Vec<u64> = generations
            .into_keys()
            .filter(|found| *found != generation)
            .collect();
        let inspection =
            inspection_report(&options, generation, &loaded, retired_generations.clone())?;
        verify_source_binding(&namespace, &lock)?;

        Ok(Self {
            options,
            limits,
            namespace,
            lock: Some(lock),
            loaded: Some(loaded),
            store: None,
            generation,
            retired_generations,
            faults,
            unusable: None,
            inspection,
        })
    }

    /// Namespace validation summary produced during session open.
    #[must_use]
    pub fn validation(&self) -> &NamespaceValidationReport {
        &self.inspection.validation
    }

    /// Complete bounded inspection report produced during session open.
    #[must_use]
    pub fn inspection(&self) -> &CheckpointInspectionReport {
        &self.inspection
    }

    /// Releases a matching quarantine at offset zero with a checked epoch
    /// increment and a forced durable WAL sync.
    pub fn reset_to_beginning(
        &mut self,
        request: ResetToBeginningRequest,
    ) -> Result<FileMutationResult, CheckpointAdminError> {
        validate_audit("reset_to_beginning", &request.audit)?;
        let (file_id, file_id_hex, old) =
            self.prepare_quarantined_target(&request.target, "reset_to_beginning")?;
        let new_epoch = old.file_epoch.checked_add(1).ok_or_else(|| {
            CheckpointAdminError::FileEpochOverflow {
                file_id: file_id_hex.clone(),
                epoch: old.file_epoch,
            }
        })?;
        let fingerprint_bytes = u16::try_from(self.options.fingerprint_bytes)
            .expect("validated checkpoint fingerprint_bytes fits u16");
        let source = open_stable_fingerprint(
            &request.source_path,
            request.follow_symlinks,
            old.locator,
            fingerprint_bytes,
            old.ignored_header_bytes,
        )
        .map_err(|error| map_reset_source_error(&file_id_hex, &request.source_path, error))?;
        let operation = ResetQuarantinedFile {
            file_id,
            expected_quarantine_epoch: request.target.expected_quarantine_epoch,
            action: ResetQuarantineAction::ResetToBeginning,
            resulting_epoch: new_epoch,
            resulting_offset: 0,
            new_committed_frontier_guard: CommittedFrontierGuard::empty(),
            new_framing_resume: FramingResume::Clean,
            new_fingerprint: source.fingerprint.clone(),
            action_time_unix_nano: request.audit.action_time_unix_nano,
            namespace_id: self.options.namespace_id.clone(),
            audit_reason: request.audit.reason.clone(),
        };
        let outcome = {
            let store = self.ensure_writable()?;
            let outcome = store.reset_quarantined_file(operation)?;
            store.sync()?;
            outcome
        };
        let loaded =
            self.refresh_live_authority("a completed audited append could not be revalidated")?;
        let new = loaded.table.get(&file_id).ok_or_else(|| {
            self.mark_unusable("a reset-to-beginning result disappeared after durable append");
            CheckpointAdminError::AuthorityStateChanged
        })?;
        if new.lifecycle_state != LifecycleState::Active
            || new.file_epoch != new_epoch
            || new.committed_offset != 0
            || new.committed_frontier_guard != CommittedFrontierGuard::empty()
            || new.framing_resume != FramingResume::Clean
            || new.fingerprint != source.fingerprint
        {
            self.mark_unusable("a reset-to-beginning result did not match its durable operation");
            return Err(CheckpointAdminError::AuthorityStateChanged);
        }
        Ok(self.file_mutation_result(
            file_id_hex,
            QuarantineMutationAction::ResetToBeginning,
            &old,
            Some(new),
            outcome.sequence,
            request.audit,
            DataEffect::DuplicatePossible,
            "Reading resumes at offset 0; source bytes delivered before quarantine may be delivered again.",
            None,
        ))
    }

    /// Appends an audited keep-failed decision while preserving every
    /// operational field and all quarantine evidence exactly.
    pub fn keep_failed(
        &mut self,
        request: KeepFailedRequest,
    ) -> Result<FileMutationResult, CheckpointAdminError> {
        validate_audit("keep_failed", &request.audit)?;
        let (file_id, file_id_hex, old) =
            self.prepare_quarantined_target(&request.target, "keep_failed")?;
        let operation = ResetQuarantinedFile {
            file_id,
            expected_quarantine_epoch: request.target.expected_quarantine_epoch,
            action: ResetQuarantineAction::KeepFailed,
            resulting_epoch: old.file_epoch,
            resulting_offset: old.committed_offset,
            new_committed_frontier_guard: old.committed_frontier_guard,
            new_framing_resume: old.framing_resume,
            new_fingerprint: old.fingerprint.clone(),
            action_time_unix_nano: request.audit.action_time_unix_nano,
            namespace_id: self.options.namespace_id.clone(),
            audit_reason: request.audit.reason.clone(),
        };
        let outcome = {
            let store = self.ensure_writable()?;
            let outcome = store.reset_quarantined_file(operation)?;
            store.sync()?;
            if store.table().get(&file_id) != Some(&old) {
                store.mark_unusable("keep-failed changed operational checkpoint state");
                return Err(CheckpointAdminError::AuthorityStateChanged);
            }
            outcome
        };
        let loaded =
            self.refresh_live_authority("a completed audited append could not be revalidated")?;
        let new = loaded.table.get(&file_id).ok_or_else(|| {
            self.mark_unusable("a keep-failed record disappeared after durable append");
            CheckpointAdminError::AuthorityStateChanged
        })?;
        if new != &old {
            self.mark_unusable("keep-failed changed reopened operational checkpoint state");
            return Err(CheckpointAdminError::AuthorityStateChanged);
        }
        Ok(self.file_mutation_result(
            file_id_hex,
            QuarantineMutationAction::KeepFailed,
            &old,
            Some(new),
            outcome.sequence,
            request.audit,
            DataEffect::None,
            "The record remains quarantined; every operational field and all quarantine evidence are unchanged.",
            None,
        ))
    }

    /// Removes an exact matching quarantined record through an internally
    /// constructed administrative WAL operation.
    pub fn remove_quarantined(
        &mut self,
        request: RemoveQuarantinedRequest,
    ) -> Result<FileMutationResult, CheckpointAdminError> {
        validate_audit("remove_quarantined", &request.audit)?;
        if request.removal_reason == REASON_CODE_RESERVED {
            return Err(StoreError::ReservedReasonCode {
                field: "remove_file.removal_reason",
                reason_code: request.removal_reason,
            }
            .into());
        }
        let (file_id, file_id_hex, old) =
            self.prepare_quarantined_target(&request.target, "remove_quarantined")?;
        let operation = RemoveFile {
            file_id,
            expected_file_epoch: request.target.expected_quarantine_epoch,
            expected_prior_state: LifecycleState::Quarantined,
            removal_reason: request.removal_reason,
            removal_time_unix_nano: request.audit.action_time_unix_nano,
            administrative: true,
            namespace_id: Some(self.options.namespace_id.clone()),
            audit_reason: Some(request.audit.reason.clone()),
        };
        let outcome = {
            let store = self.ensure_writable()?;
            let outcome = store.append(vec![Operation::RemoveFile(operation)])?;
            store.sync()?;
            outcome
        };
        let loaded =
            self.refresh_live_authority("a completed audited append could not be revalidated")?;
        if loaded.table.get(&file_id).is_some() {
            self.mark_unusable("an administrative removal remained present after durable append");
            return Err(CheckpointAdminError::AuthorityStateChanged);
        }
        Ok(self.file_mutation_result(
            file_id_hex,
            QuarantineMutationAction::RemoveQuarantined,
            &old,
            None,
            outcome.sequence,
            request.audit,
            DataEffect::DuplicateOrLossPossible,
            request.consequence.description(),
            None,
        ))
    }

    /// Releases a matching quarantine at a stable EOF sampled from the
    /// operator-supplied exact source path.
    pub fn reset_to_end(
        &mut self,
        request: ResetToEndRequest,
    ) -> Result<FileMutationResult, CheckpointAdminError> {
        self.reset_to_end_sampled(request, open_stable_eof)
    }

    #[cfg(test)]
    fn reset_to_end_with_hook(
        &mut self,
        request: ResetToEndRequest,
        after_first_sample: impl FnOnce(),
    ) -> Result<FileMutationResult, CheckpointAdminError> {
        self.reset_to_end_sampled(
            request,
            |path, follow_symlinks, locator, fingerprint_bytes, ignored_header_bytes| {
                crate::receivers::filelog_receiver::identity::platform::open_stable_eof_with_hook(
                    path,
                    follow_symlinks,
                    locator,
                    fingerprint_bytes,
                    ignored_header_bytes,
                    after_first_sample,
                )
            },
        )
    }

    fn reset_to_end_sampled(
        &mut self,
        request: ResetToEndRequest,
        sample: impl FnOnce(
            &Path,
            bool,
            super::primitives::Locator,
            u16,
            u32,
        ) -> Result<StableEofEvidence, IdentityError>,
    ) -> Result<FileMutationResult, CheckpointAdminError> {
        validate_audit("reset_to_end", &request.audit)?;
        let (file_id, file_id_hex, old) =
            self.prepare_quarantined_target(&request.target, "reset_to_end")?;
        let new_epoch = old.file_epoch.checked_add(1).ok_or_else(|| {
            CheckpointAdminError::FileEpochOverflow {
                file_id: file_id_hex.clone(),
                epoch: old.file_epoch,
            }
        })?;
        let source = sample(
            &request.source_path,
            request.follow_symlinks,
            old.locator,
            u16::try_from(self.options.fingerprint_bytes)
                .expect("validated checkpoint fingerprint_bytes fits u16"),
            old.ignored_header_bytes,
        )
        .map_err(|error| map_reset_source_error(&file_id_hex, &request.source_path, error))?;
        let source_report = ResetToEndEvidenceReport {
            source_path: native_path_report(&request.source_path)?,
            follow_symlinks: request.follow_symlinks,
            locator: source.locator.into(),
            eof_offset: source.offset,
            committed_frontier_guard: source.committed_frontier_guard.into(),
        };
        let operation = ResetQuarantinedFile {
            file_id,
            expected_quarantine_epoch: request.target.expected_quarantine_epoch,
            action: ResetQuarantineAction::ResetToEnd,
            resulting_epoch: new_epoch,
            resulting_offset: source.offset,
            new_committed_frontier_guard: source.committed_frontier_guard,
            new_framing_resume: FramingResume::Clean,
            new_fingerprint: source.fingerprint.clone(),
            action_time_unix_nano: request.audit.action_time_unix_nano,
            namespace_id: self.options.namespace_id.clone(),
            audit_reason: request.audit.reason.clone(),
        };
        let outcome = {
            let store = self.ensure_writable()?;
            let outcome = store.reset_quarantined_file(operation)?;
            store.sync()?;
            outcome
        };
        let loaded =
            self.refresh_live_authority("a completed audited append could not be revalidated")?;
        let new = loaded.table.get(&file_id).ok_or_else(|| {
            self.mark_unusable("a reset-to-end result disappeared after durable append");
            CheckpointAdminError::AuthorityStateChanged
        })?;
        if new.lifecycle_state != LifecycleState::Active
            || new.file_epoch != new_epoch
            || new.committed_offset != source.offset
            || new.committed_frontier_guard != source.committed_frontier_guard
            || new.framing_resume != FramingResume::Clean
            || new.fingerprint != source.fingerprint
        {
            self.mark_unusable("a reset-to-end result did not match its durable operation");
            return Err(CheckpointAdminError::AuthorityStateChanged);
        }
        Ok(self.file_mutation_result(
            file_id_hex,
            QuarantineMutationAction::ResetToEnd,
            &old,
            Some(new),
            outcome.sequence,
            request.audit,
            DataEffect::LossAccepted,
            "Reading resumes at the sampled EOF; any undelivered source bytes before that offset are intentionally skipped.",
            Some(source_report),
        ))
    }

    /// Copies recognized bounded checkpoint artifacts to a new destination
    /// and writes a synced machine-readable manifest.
    ///
    /// The retained namespace lock remains held for the complete inventory,
    /// copy, hashing, and manifest sequence. `ownership.lock` and unrelated
    /// directory entries are never copied.
    ///
    /// The completed destination directory is synced before its canonical
    /// parent. On Windows, both directory syncs retain the existing
    /// documented no-op limitation because `std::fs` exposes no supported
    /// directory-sync operation there; every copied file is still synced.
    pub fn backup(
        &self,
        destination: impl AsRef<Path>,
    ) -> Result<EvidenceBackupManifest, CheckpointAdminError> {
        let completed = self.create_evidence_backup(destination.as_ref())?;
        completed.verify()?;
        let _ = self.verify_source_matches_backup(&completed.manifest)?;
        Ok(completed.manifest)
    }

    fn create_evidence_backup(
        &self,
        destination: &Path,
    ) -> Result<CompletedEvidenceBackup, CheckpointAdminError> {
        self.ensure_usable("back up checkpoint evidence")?;
        let lock = self.active_lock();
        let sources = with_verified_source(&self.namespace, lock, || {
            inventory_backup_artifacts(&self.options.namespace_dir, &self.limits, self.generation)
        })?;
        create_evidence_backup(
            &self.options,
            &self.namespace,
            lock,
            destination,
            sources,
            NamespaceAuthorityReport::Valid {
                validation: self.inspection.validation.clone(),
            },
        )
    }

    /// Releases the exclusive namespace lock and reports an unlock failure.
    pub fn release(mut self) -> Result<(), CheckpointAdminError> {
        if let Some(store) = self.store.take() {
            store.release_admin().map_err(CheckpointAdminError::from)
        } else {
            self.lock
                .take()
                .expect("a read-only admin session retains its namespace lock")
                .release()
                .map_err(CheckpointAdminError::from)
        }
    }

    fn active_lock(&self) -> &NamespaceLock {
        self.store.as_ref().map_or_else(
            || {
                self.lock
                    .as_ref()
                    .expect("a read-only admin session retains its namespace lock")
            },
            CheckpointStore::admin_lock,
        )
    }

    fn ensure_usable(&self, operation: &'static str) -> Result<(), CheckpointAdminError> {
        if let Some(store) = &self.store {
            store.ensure_usable(operation)?;
        }
        if let Some(reason) = self.unusable {
            return Err(StoreError::Unusable {
                dir: self.options.namespace_dir.clone(),
                operation,
                reason,
            }
            .into());
        }
        Ok(())
    }

    fn mark_unusable(&mut self, reason: &'static str) {
        if let Some(store) = self.store.as_mut() {
            store.mark_unusable(reason);
        } else {
            self.unusable = Some(reason);
        }
    }

    fn ensure_writable(&mut self) -> Result<&mut CheckpointStore, CheckpointAdminError> {
        if let Some(store) = &self.store {
            store.ensure_not_unusable("prepare an audited checkpoint mutation")?;
            if let Some(reason) = self.unusable {
                return Err(StoreError::Unusable {
                    dir: self.options.namespace_dir.clone(),
                    operation: "prepare an audited checkpoint mutation",
                    reason,
                }
                .into());
            }
        } else {
            self.ensure_usable("prepare an audited checkpoint mutation")?;
        }
        if self.store.is_none() {
            let transition = CheckpointStore::from_admin_session(
                self.options.clone(),
                self.limits,
                &mut self.lock,
                self.generation,
                &mut self.loaded,
                self.retired_generations.clone(),
                self.faults,
            );
            self.faults = FaultPlan::disabled();
            match transition {
                Ok(store) => self.store = Some(store),
                Err(error) => {
                    self.mark_unusable(
                        "the append-capable transition may have changed checkpoint artifacts",
                    );
                    return Err(error.into());
                }
            }
            let _ = self.refresh_live_authority(
                "the append-capable transition could not revalidate its checkpoint authority",
            )?;
        }
        Ok(self
            .store
            .as_mut()
            .expect("a successful admin transition installs its checkpoint store"))
    }

    fn load_locked_authority(
        &self,
    ) -> Result<(u64, LoadedGeneration, Vec<u64>), CheckpointAdminError> {
        self.ensure_usable("revalidate the locked checkpoint authority")?;
        let lock = self.active_lock();
        let marker_path = self.options.namespace_dir.join(CURRENT_FILE_NAME);
        let marker_bytes = with_verified_source(&self.namespace, lock, || {
            fsio::read_file_bounded_read_only(
                &marker_path,
                "CURRENT marker",
                MARKER_READ_MAX_BYTES,
            )?
            .ok_or_else(|| CheckpointAdminError::RequiredArtifactMissing {
                artifact: "CURRENT marker",
                path: marker_path.clone(),
            })
        })?;
        let generation =
            decode_current_marker(&marker_bytes).map_err(|source| StoreError::Decode {
                artifact: "CURRENT marker",
                path: marker_path,
                source,
            })?;
        let loaded = with_verified_source(&self.namespace, lock, || {
            Ok(CheckpointStore::load_generation_read_only(
                &self.options.namespace_dir,
                generation,
                &self.options.namespace_id,
                &self.limits,
                self.options.max_tracked_files,
                self.options.fingerprint_bytes,
            )?)
        })?;
        let generations = with_verified_source(&self.namespace, lock, || {
            Ok(layout::scan_generations_read_only(
                &self.options.namespace_dir,
            )?)
        })?;
        let retired_generations = generations
            .into_keys()
            .filter(|recognized| *recognized != generation)
            .collect();
        Ok((generation, loaded, retired_generations))
    }

    fn revalidate_authority(&mut self) -> Result<(), CheckpointAdminError> {
        let (generation, loaded, retired_generations) = self.load_locked_authority()?;
        if generation != self.generation {
            let expected = self.generation;
            self.mark_unusable("CURRENT changed while the administration lock was retained");
            return Err(CheckpointAdminError::AuthorityChanged {
                expected,
                found: generation,
            });
        }
        let authority_records = self.authority_records();
        let expected_validation = &self.inspection.validation;
        let state_matches = authority_records == loaded.table.snapshot_records()
            && expected_validation.snapshot_record_count
                == report_count(loaded.snapshot_records, "snapshot record")?
            && expected_validation.wal_transaction_count
                == report_count(loaded.transactions_replayed, "WAL transaction")?
            && expected_validation.torn_wal_tail_bytes
                == report_count(loaded.torn_tail_bytes, "torn WAL tail byte")?;
        if !state_matches {
            self.mark_unusable(
                "checkpoint artifacts changed while the administration lock was retained",
            );
            return Err(CheckpointAdminError::AuthorityStateChanged);
        }
        if self.store.is_none() {
            self.loaded = Some(loaded);
        }
        self.inspection.validation.retired_generations = retired_generations.clone();
        self.retired_generations = retired_generations;
        Ok(())
    }

    fn refresh_live_authority(
        &mut self,
        load_failure_reason: &'static str,
    ) -> Result<LoadedGeneration, CheckpointAdminError> {
        let (generation, loaded, retired_generations) = match self.load_locked_authority() {
            Ok(authority) => authority,
            Err(error) => {
                self.mark_unusable(load_failure_reason);
                return Err(error);
            }
        };
        if generation != self.generation {
            let expected = self.generation;
            self.mark_unusable("CURRENT changed after an audited checkpoint append");
            return Err(CheckpointAdminError::AuthorityChanged {
                expected,
                found: generation,
            });
        }
        let Some(store) = &self.store else {
            self.mark_unusable("an audited mutation completed without a live checkpoint store");
            return Err(CheckpointAdminError::AuthorityStateChanged);
        };
        let stats = store.stats();
        if store.table().snapshot_records() != loaded.table.snapshot_records()
            || stats.wal_bytes != loaded.wal_valid_len
            || stats.wal_transactions
                != u64::try_from(loaded.transactions_replayed).unwrap_or(u64::MAX)
        {
            self.mark_unusable("reopened checkpoint state disagreed with the completed append");
            return Err(CheckpointAdminError::AuthorityStateChanged);
        }
        self.inspection = match inspection_report(
            &self.options,
            generation,
            &loaded,
            retired_generations.clone(),
        ) {
            Ok(inspection) => inspection,
            Err(error) => {
                self.mark_unusable("a completed audited append report could not be built");
                return Err(error);
            }
        };
        self.retired_generations = retired_generations;
        Ok(loaded)
    }

    fn authority_records(&self) -> Vec<SnapshotRecord> {
        self.store.as_ref().map_or_else(
            || {
                self.loaded
                    .as_ref()
                    .expect("a read-only admin session retains its loaded authority")
                    .table
                    .snapshot_records()
            },
            |store| store.table().snapshot_records(),
        )
    }

    fn prepare_quarantined_target(
        &mut self,
        target: &QuarantinedFileTarget,
        operation: &'static str,
    ) -> Result<(FileId, String, SnapshotRecord), CheckpointAdminError> {
        let file_id = parse_file_id(&target.file_id)?;
        if !self
            .store
            .as_ref()
            .is_some_and(CheckpointStore::has_pending_wal_append)
        {
            self.revalidate_authority()?;
        }
        let record = self
            .store
            .as_ref()
            .map_or_else(
                || {
                    self.loaded
                        .as_ref()
                        .expect("a read-only admin session retains its loaded authority")
                        .table
                        .get(&file_id)
                },
                |store| store.table().get(&file_id),
            )
            .cloned()
            .ok_or_else(|| CheckpointAdminError::FileNotFound {
                file_id: target.file_id.clone(),
            })?;
        if record.lifecycle_state != LifecycleState::Quarantined {
            return Err(CheckpointAdminError::ExpectedQuarantine {
                operation,
                file_id: target.file_id.clone(),
                state: record.lifecycle_state.into(),
            });
        }
        let evidence = record.quarantine_evidence.as_ref().ok_or_else(|| {
            CheckpointAdminError::MissingQuarantineEvidence {
                file_id: target.file_id.clone(),
            }
        })?;
        if evidence.quarantine_epoch != target.expected_quarantine_epoch
            || record.file_epoch != target.expected_quarantine_epoch
        {
            return Err(CheckpointAdminError::QuarantineEpochMismatch {
                file_id: target.file_id.clone(),
                expected: target.expected_quarantine_epoch,
                actual: evidence.quarantine_epoch,
            });
        }
        Ok((file_id, target.file_id.clone(), record))
    }

    #[allow(clippy::too_many_arguments)]
    fn file_mutation_result(
        &self,
        file_id: String,
        action: QuarantineMutationAction,
        old: &SnapshotRecord,
        new: Option<&SnapshotRecord>,
        wal_sequence: u64,
        audit: AuditMetadata,
        data_effect: DataEffect,
        consequence: &'static str,
        reset_to_end_evidence: Option<ResetToEndEvidenceReport>,
    ) -> FileMutationResult {
        FileMutationResult {
            namespace_id: self.options.namespace_id.clone(),
            namespace_path: self.inspection.validation.derived_namespace_path.clone(),
            file_id,
            generation: self.generation,
            action,
            old_lifecycle: old.lifecycle_state.into(),
            new_lifecycle: new.map_or(CheckpointLifecycleReport::Absent, |record| {
                record.lifecycle_state.into()
            }),
            old_epoch: old.file_epoch,
            new_epoch: new.map(|record| record.file_epoch),
            old_offset: old.committed_offset,
            new_offset: new.map(|record| record.committed_offset),
            wal_sequence,
            audit,
            data_effect,
            consequence: consequence.to_owned(),
            reset_to_end_evidence,
        }
    }

    fn verify_source_matches_backup(
        &self,
        manifest: &EvidenceBackupManifest,
    ) -> Result<Vec<BackupSourceArtifact>, CheckpointAdminError> {
        verify_source_matches_backup(
            &self.options,
            &self.limits,
            &self.namespace,
            self.active_lock(),
            manifest,
            Some(self.generation),
        )
    }
}

fn inspection_report(
    options: &StoreOptions,
    generation: u64,
    loaded: &LoadedGeneration,
    retired_generations: Vec<u64>,
) -> Result<CheckpointInspectionReport, CheckpointAdminError> {
    Ok(CheckpointInspectionReport {
        validation: NamespaceValidationReport {
            namespace_id: options.namespace_id.clone(),
            derived_namespace_path: native_path_report(&options.namespace_dir)?,
            selected_generation: generation,
            snapshot_record_count: report_count(loaded.snapshot_records, "snapshot record")?,
            wal_transaction_count: report_count(loaded.transactions_replayed, "WAL transaction")?,
            tracked_file_count: report_count(loaded.table.len(), "tracked file")?,
            quarantine_count: report_count(loaded.table.quarantined_len(), "quarantine")?,
            torn_wal_tail_bytes: report_count(loaded.torn_tail_bytes, "torn WAL tail byte")?,
            retired_generations,
        },
        quarantines: quarantine_reports(&loaded.table)?,
    })
}

fn validate_audit(
    operation: &'static str,
    audit: &AuditMetadata,
) -> Result<(), CheckpointAdminError> {
    if audit.reason.is_empty() {
        return Err(CheckpointAdminError::AuditReasonRequired { operation });
    }
    if audit.reason.len() > AUDIT_REASON_MAX_BYTES {
        return Err(CheckpointAdminError::AuditReasonTooLong {
            operation,
            len: audit.reason.len(),
            max: AUDIT_REASON_MAX_BYTES,
        });
    }
    Ok(())
}

fn parse_file_id(value: &str) -> Result<FileId, CheckpointAdminError> {
    if value.len() != 32
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(CheckpointAdminError::InvalidFileId);
    }
    let mut bytes = [0u8; 16];
    hex::decode_to_slice(value, &mut bytes).map_err(|_| CheckpointAdminError::InvalidFileId)?;
    Ok(FileId(bytes))
}

fn map_reset_source_error(
    file_id: &str,
    source_path: &Path,
    error: IdentityError,
) -> CheckpointAdminError {
    match error {
        IdentityError::Io {
            operation, source, ..
        } => CheckpointAdminError::ResetSourceIo {
            operation,
            path: source_path.to_path_buf(),
            source,
        },
        IdentityError::NotRegularFile { .. } => CheckpointAdminError::ResetSourceNotRegular {
            path: source_path.to_path_buf(),
        },
        IdentityError::SymlinkOrReparsePoint { .. } => {
            CheckpointAdminError::ResetSourceSymlinkOrReparse {
                path: source_path.to_path_buf(),
            }
        }
        IdentityError::ReopenLocatorMismatch {
            expected, found, ..
        } => CheckpointAdminError::ResetSourceLocatorMismatch {
            file_id: file_id.to_owned(),
            expected: expected.into(),
            found: found.into(),
        },
        IdentityError::CandidateChangedDuringIdentity { .. } => {
            CheckpointAdminError::ResetSourceChanged {
                path: source_path.to_path_buf(),
            }
        }
        IdentityError::UnsupportedPlatform { .. } => CheckpointAdminError::ResetSourceUnsupported {
            path: source_path.to_path_buf(),
        },
        IdentityError::InvalidEvidence { .. }
        | IdentityError::InvalidAdvisoryPath { .. }
        | IdentityError::ReopenFingerprintMismatch { .. }
        | IdentityError::ReopenOffsetBeyondSize { .. }
        | IdentityError::ReopenFrontierGuardMismatch { .. }
        | IdentityError::DuplicateCandidateLocator { .. }
        | IdentityError::AmbiguousQuarantinedLocator { .. }
        | IdentityError::IncompatibleProfile { .. }
        | IdentityError::FileIdCollisionLimit { .. }
        | IdentityError::Store(_) => CheckpointAdminError::ResetSourceValidation {
            path: source_path.to_path_buf(),
            reason: "unexpected identity-layer failure",
        },
    }
}

impl RemovalConsequence {
    fn description(self) -> &'static str {
        match self {
            Self::AcknowledgeDuplicateOrLossPossible => {
                "The checkpoint record is deleted; later registration may replay previously delivered bytes or skip existing bytes according to registration policy."
            }
        }
    }
}

impl From<LifecycleState> for CheckpointLifecycleReport {
    fn from(value: LifecycleState) -> Self {
        match value {
            LifecycleState::Active => Self::Active,
            LifecycleState::RotatedFinalized => Self::RotatedFinalized,
            LifecycleState::Quarantined => Self::Quarantined,
        }
    }
}

impl From<CommittedFrontierGuard> for CommittedFrontierGuardReport {
    fn from(value: CommittedFrontierGuard) -> Self {
        Self {
            window_len: value.window_len,
            digest: hex::encode(value.digest),
        }
    }
}

fn verify_source_binding(
    namespace: &fsio::DirectoryPathBinding,
    lock: &NamespaceLock,
) -> Result<(), CheckpointAdminError> {
    namespace.verify("verify the checkpoint namespace path binding")?;
    lock.verify_path_binding()?;
    namespace.verify("reverify the checkpoint namespace path binding")?;
    Ok(())
}

fn with_verified_source<T>(
    namespace: &fsio::DirectoryPathBinding,
    lock: &NamespaceLock,
    operation: impl FnOnce() -> Result<T, CheckpointAdminError>,
) -> Result<T, CheckpointAdminError> {
    verify_source_binding(namespace, lock)?;
    let result = operation();
    verify_source_binding(namespace, lock)?;
    result
}

impl NamespaceAuthorityReport {
    fn selected_generation(&self) -> Option<u64> {
        match self {
            Self::Valid { validation } => Some(validation.selected_generation),
            Self::Invalid { failure } => failure.selected_generation,
        }
    }

    fn tracked_file_count(&self) -> Option<u64> {
        match self {
            Self::Valid { validation } => Some(validation.tracked_file_count),
            Self::Invalid { .. } => None,
        }
    }

    fn quarantine_count(&self) -> Option<u64> {
        match self {
            Self::Valid { validation } => Some(validation.quarantine_count),
            Self::Invalid { .. } => None,
        }
    }
}

fn probe_namespace_authority(
    options: &StoreOptions,
    limits: &StoreLimits,
    namespace: &fsio::DirectoryPathBinding,
    lock: &NamespaceLock,
) -> Result<NamespaceAuthorityReport, CheckpointAdminError> {
    let marker_path = options.namespace_dir.join(CURRENT_FILE_NAME);
    verify_source_binding(namespace, lock)?;
    let marker_result =
        fsio::read_file_bounded_read_only(&marker_path, "CURRENT marker", MARKER_READ_MAX_BYTES);
    verify_source_binding(namespace, lock)?;
    let marker_bytes = match marker_result {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            return Ok(invalid_authority_report(
                NamespaceAuthorityFailureKind::MissingCurrent,
                None,
                "the canonical CURRENT marker is absent",
            ));
        }
        Err(error) => {
            return Ok(invalid_authority_report(
                NamespaceAuthorityFailureKind::CurrentUnreadable,
                None,
                error,
            ));
        }
    };
    let generation = match decode_current_marker(&marker_bytes) {
        Ok(generation) => generation,
        Err(error) => {
            return Ok(invalid_authority_report(
                NamespaceAuthorityFailureKind::CurrentInvalid,
                None,
                error,
            ));
        }
    };

    verify_source_binding(namespace, lock)?;
    let loaded_result = CheckpointStore::load_generation_read_only(
        &options.namespace_dir,
        generation,
        &options.namespace_id,
        limits,
        options.max_tracked_files,
        options.fingerprint_bytes,
    );
    verify_source_binding(namespace, lock)?;
    let loaded = match loaded_result {
        Ok(loaded) => loaded,
        Err(error) => {
            return Ok(invalid_authority_report(
                NamespaceAuthorityFailureKind::SelectedGenerationInvalid,
                Some(generation),
                error,
            ));
        }
    };

    verify_source_binding(namespace, lock)?;
    let generations_result = layout::scan_generations_read_only(&options.namespace_dir);
    verify_source_binding(namespace, lock)?;
    let generations = match generations_result {
        Ok(generations) => generations,
        Err(error) => {
            return Ok(invalid_authority_report(
                NamespaceAuthorityFailureKind::GenerationInventoryInvalid,
                Some(generation),
                error,
            ));
        }
    };
    let retired_generations = generations
        .into_keys()
        .filter(|recognized| *recognized != generation)
        .collect();
    match inspection_report(options, generation, &loaded, retired_generations) {
        Ok(inspection) => Ok(NamespaceAuthorityReport::Valid {
            validation: inspection.validation,
        }),
        Err(error) => Ok(invalid_authority_report(
            NamespaceAuthorityFailureKind::ReportInvalid,
            Some(generation),
            error,
        )),
    }
}

fn invalid_authority_report(
    kind: NamespaceAuthorityFailureKind,
    selected_generation: Option<u64>,
    error: impl std::fmt::Display,
) -> NamespaceAuthorityReport {
    let detail = error.to_string();
    let (detail, detail_truncated) = bounded_validation_detail(detail);
    NamespaceAuthorityReport::Invalid {
        failure: NamespaceAuthorityFailureReport {
            kind,
            selected_generation,
            detail,
            detail_truncated,
        },
    }
}

fn bounded_validation_detail(mut detail: String) -> (String, bool) {
    if detail.len() <= NAMESPACE_VALIDATION_DETAIL_MAX_BYTES {
        return (detail, false);
    }
    let mut end = NAMESPACE_VALIDATION_DETAIL_MAX_BYTES;
    while !detail.is_char_boundary(end) {
        end -= 1;
    }
    detail.truncate(end);
    (detail, true)
}

fn create_evidence_backup(
    options: &StoreOptions,
    namespace: &fsio::DirectoryPathBinding,
    lock: &NamespaceLock,
    requested_destination: &Path,
    sources: Vec<BackupSourceArtifact>,
    authority: NamespaceAuthorityReport,
) -> Result<CompletedEvidenceBackup, CheckpointAdminError> {
    let destination = with_verified_source(namespace, lock, || {
        PreparedBackupDestination::create(&options.namespace_dir, requested_destination)
    })?;

    let mut artifacts = Vec::with_capacity(sources.len());
    for source in sources {
        let source_path = options.namespace_dir.join(&source.name);
        let bytes = with_verified_source(namespace, lock, || {
            fsio::read_file_bounded_read_only(&source_path, source.artifact, source.max_bytes)?
                .ok_or_else(|| CheckpointAdminError::BackupArtifactDisappeared {
                    path: source_path.clone(),
                })
        })?;
        destination.write_file(&source.name, &bytes)?;
        artifacts.push(EvidenceArtifact {
            role: source.role,
            name: source.name,
            generation: source.generation,
            length: u64::try_from(bytes.len()).map_err(|_| {
                CheckpointAdminError::CountOverflow {
                    field: "backup artifact byte",
                }
            })?,
            sha256: hex::encode(Sha256::digest(&bytes)),
        });
    }

    verify_source_binding(namespace, lock)?;
    let manifest = EvidenceBackupManifest {
        manifest_version: EVIDENCE_BACKUP_MANIFEST_VERSION,
        namespace_id: options.namespace_id.clone(),
        source_namespace: native_path_report(&options.namespace_dir)?,
        artifacts,
        authority,
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|source| CheckpointAdminError::ManifestEncode { source })?;
    destination.write_file(EVIDENCE_BACKUP_MANIFEST_FILE_NAME, &manifest_bytes)?;
    sync_backup_directories(&destination)?;
    verify_source_binding(namespace, lock)?;
    Ok(CompletedEvidenceBackup {
        destination,
        manifest,
        manifest_bytes,
    })
}

fn verify_source_matches_backup(
    options: &StoreOptions,
    limits: &StoreLimits,
    namespace: &fsio::DirectoryPathBinding,
    lock: &NamespaceLock,
    manifest: &EvidenceBackupManifest,
    required_generation: Option<u64>,
) -> Result<Vec<BackupSourceArtifact>, CheckpointAdminError> {
    if manifest.manifest_version != EVIDENCE_BACKUP_MANIFEST_VERSION
        || manifest.namespace_id != options.namespace_id
        || manifest.source_namespace != native_path_report(&options.namespace_dir)?
    {
        return Err(CheckpointAdminError::BackupVerification {
            path: options.namespace_dir.clone(),
            reason: "backup manifest does not identify the locked namespace",
        });
    }
    let sources = with_verified_source(namespace, lock, || match required_generation {
        Some(generation) => inventory_backup_artifacts(&options.namespace_dir, limits, generation),
        None => inventory_recognized_artifacts(&options.namespace_dir, limits),
    })?;
    if sources.len() != manifest.artifacts.len() {
        return Err(CheckpointAdminError::BackupVerification {
            path: options.namespace_dir.clone(),
            reason: "source artifact inventory changed after backup",
        });
    }
    for (source, artifact) in sources.iter().zip(&manifest.artifacts) {
        if source.name != artifact.name
            || source.role != artifact.role
            || source.generation != artifact.generation
        {
            return Err(CheckpointAdminError::BackupVerification {
                path: options.namespace_dir.join(&source.name),
                reason: "source artifact identity changed after backup",
            });
        }
        let source_path = options.namespace_dir.join(&source.name);
        let bytes = with_verified_source(namespace, lock, || {
            fsio::read_file_bounded_read_only(&source_path, source.artifact, source.max_bytes)?
                .ok_or_else(|| CheckpointAdminError::BackupArtifactDisappeared {
                    path: source_path.clone(),
                })
        })?;
        if u64::try_from(bytes.len()).ok() != Some(artifact.length)
            || hex::encode(Sha256::digest(&bytes)) != artifact.sha256
        {
            return Err(CheckpointAdminError::BackupVerification {
                path: source_path,
                reason: "source artifact bytes changed after backup",
            });
        }
    }
    let authority = probe_namespace_authority(options, limits, namespace, lock)?;
    if authority != manifest.authority {
        return Err(CheckpointAdminError::BackupVerification {
            path: options.namespace_dir.clone(),
            reason: "source authority changed after backup",
        });
    }
    Ok(sources)
}

fn native_path_report(path: &Path) -> Result<NativePathReport, CheckpointAdminError> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;

        let evidence =
            AdvisoryPath::from_unix_bytes(path.as_os_str().as_bytes()).map_err(|source| {
                CheckpointAdminError::NativePathEncode {
                    path: path.to_path_buf(),
                    source,
                }
            })?;
        Ok(NativePathReport {
            kind: NativePathKindReport::UnixBytes,
            truncated: evidence.is_truncated(),
            full_path_len: evidence.full_path_len(),
            stored_path_hex: hex::encode(evidence.stored_path_bytes()),
            full_path_digest: hex::encode(evidence.full_path_digest()),
        })
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt as _;

        let units: Vec<u16> = path.as_os_str().encode_wide().collect();
        let evidence = AdvisoryPath::from_windows_utf16_units(&units).map_err(|source| {
            CheckpointAdminError::NativePathEncode {
                path: path.to_path_buf(),
                source,
            }
        })?;
        Ok(NativePathReport {
            kind: NativePathKindReport::WindowsUtf16Le,
            truncated: evidence.is_truncated(),
            full_path_len: evidence.full_path_len(),
            stored_path_hex: hex::encode(evidence.stored_path_bytes()),
            full_path_digest: hex::encode(evidence.full_path_digest()),
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(CheckpointAdminError::NativePathUnsupported {
            path: path.to_path_buf(),
        })
    }
}

#[derive(Debug)]
struct PreparedBackupDestination {
    parent: fsio::DirectoryPathBinding,
    directory: fsio::DirectoryPathBinding,
}

impl PreparedBackupDestination {
    fn create(source_namespace: &Path, requested: &Path) -> Result<Self, CheckpointAdminError> {
        let Some(file_name) = requested.file_name() else {
            let resolved = std::fs::canonicalize(requested).map_err(|source| {
                CheckpointAdminError::BackupIo {
                    operation: "resolve an existing checkpoint evidence-backup destination",
                    path: requested.to_path_buf(),
                    source,
                }
            })?;
            return Err(CheckpointAdminError::BackupDestinationExists { path: resolved });
        };
        let requested_parent = requested
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let parent = fsio::DirectoryPathBinding::open_canonical_resolving(
            requested_parent,
            "resolve the checkpoint evidence-backup destination parent",
        )?;
        let resolved = parent.path().join(file_name);
        if resolved.starts_with(source_namespace) {
            return Err(CheckpointAdminError::BackupDestinationInsideNamespace {
                source_namespace: source_namespace.to_path_buf(),
                destination: resolved,
            });
        }

        parent.verify("verify the checkpoint evidence-backup destination parent")?;
        let existing = std::fs::symlink_metadata(&resolved);
        parent.verify("reverify the checkpoint evidence-backup destination parent")?;
        match existing {
            Ok(_) => {
                return Err(CheckpointAdminError::BackupDestinationExists { path: resolved });
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(CheckpointAdminError::BackupIo {
                    operation: "inspect the checkpoint evidence-backup destination",
                    path: resolved,
                    source,
                });
            }
        }

        parent.verify("verify the checkpoint evidence-backup parent before creation")?;
        create_backup_directory(&resolved)?;
        parent.verify("verify the checkpoint evidence-backup parent after creation")?;
        let directory = fsio::DirectoryPathBinding::open_canonical(
            &resolved,
            "open the new checkpoint evidence-backup destination",
        )?;
        parent.verify("reverify the checkpoint evidence-backup parent after opening")?;
        directory.verify("verify the new checkpoint evidence-backup destination")?;
        if directory.path().starts_with(source_namespace) {
            return Err(CheckpointAdminError::BackupDestinationInsideNamespace {
                source_namespace: source_namespace.to_path_buf(),
                destination: directory.path().to_path_buf(),
            });
        }
        Ok(Self { parent, directory })
    }

    fn verify(&self) -> Result<(), CheckpointAdminError> {
        self.parent
            .verify("verify the checkpoint evidence-backup destination parent")?;
        self.directory
            .verify("verify the checkpoint evidence-backup destination directory")?;
        self.parent
            .verify("reverify the checkpoint evidence-backup destination parent")?;
        Ok(())
    }

    fn write_file(&self, name: &str, bytes: &[u8]) -> Result<(), CheckpointAdminError> {
        self.verify()?;
        let result = write_backup_file(&self.directory.path().join(name), bytes);
        self.verify()?;
        result
    }
}

#[derive(Debug)]
struct CompletedEvidenceBackup {
    destination: PreparedBackupDestination,
    manifest: EvidenceBackupManifest,
    manifest_bytes: Vec<u8>,
}

impl CompletedEvidenceBackup {
    fn verify(&self) -> Result<(), CheckpointAdminError> {
        self.destination.verify()?;
        let mut expected_names: BTreeSet<String> = self
            .manifest
            .artifacts
            .iter()
            .map(|artifact| artifact.name.clone())
            .collect();
        let _ = expected_names.insert(EVIDENCE_BACKUP_MANIFEST_FILE_NAME.to_owned());
        let actual_names: BTreeSet<String> = std::fs::read_dir(self.destination.directory.path())
            .map_err(|source| CheckpointAdminError::BackupIo {
                operation: "list the completed checkpoint evidence backup",
                path: self.destination.directory.path().to_path_buf(),
                source,
            })?
            .map(|entry| {
                entry
                    .map_err(|source| CheckpointAdminError::BackupIo {
                        operation: "read a completed checkpoint evidence-backup entry",
                        path: self.destination.directory.path().to_path_buf(),
                        source,
                    })?
                    .file_name()
                    .into_string()
                    .map_err(|_| CheckpointAdminError::BackupVerification {
                        path: self.destination.directory.path().to_path_buf(),
                        reason: "backup contains a non-UTF-8 artifact name",
                    })
            })
            .collect::<Result<_, _>>()?;
        self.destination.verify()?;
        if actual_names != expected_names {
            return Err(CheckpointAdminError::BackupVerification {
                path: self.destination.directory.path().to_path_buf(),
                reason: "backup artifact inventory does not match the manifest",
            });
        }

        for artifact in &self.manifest.artifacts {
            let path = self.destination.directory.path().join(&artifact.name);
            self.destination.verify()?;
            let bytes = fsio::read_file_bounded_read_only(
                &path,
                "checkpoint evidence-backup artifact",
                artifact.length,
            )?
            .ok_or_else(|| CheckpointAdminError::BackupVerification {
                path: path.clone(),
                reason: "manifest artifact is absent",
            })?;
            self.destination.verify()?;
            if u64::try_from(bytes.len()).ok() != Some(artifact.length)
                || hex::encode(Sha256::digest(&bytes)) != artifact.sha256
            {
                return Err(CheckpointAdminError::BackupVerification {
                    path,
                    reason: "manifest artifact length or digest does not match",
                });
            }
        }

        let manifest_path = self
            .destination
            .directory
            .path()
            .join(EVIDENCE_BACKUP_MANIFEST_FILE_NAME);
        let manifest_bytes = fsio::read_file_bounded_read_only(
            &manifest_path,
            "checkpoint evidence-backup manifest",
            self.manifest_bytes.len() as u64,
        )?
        .ok_or_else(|| CheckpointAdminError::BackupVerification {
            path: manifest_path.clone(),
            reason: "backup manifest is absent",
        })?;
        self.destination.verify()?;
        if manifest_bytes != self.manifest_bytes {
            return Err(CheckpointAdminError::BackupVerification {
                path: manifest_path,
                reason: "backup manifest bytes changed after sync",
            });
        }
        Ok(())
    }
}

fn sync_backup_directories(
    destination: &PreparedBackupDestination,
) -> Result<(), CheckpointAdminError> {
    sync_backup_directories_with(destination, |directory, operation| {
        directory
            .sync(operation)
            .map_err(CheckpointAdminError::from)
    })
}

fn sync_backup_directories_with(
    destination: &PreparedBackupDestination,
    mut sync: impl FnMut(&fsio::DirectoryPathBinding, &'static str) -> Result<(), CheckpointAdminError>,
) -> Result<(), CheckpointAdminError> {
    destination.verify()?;
    let destination_sync = sync(
        &destination.directory,
        "sync the completed checkpoint evidence-backup destination",
    );
    destination.verify()?;
    destination_sync?;

    let parent_sync = sync(
        &destination.parent,
        "sync the checkpoint evidence-backup destination parent",
    );
    destination.verify()?;
    parent_sync
}

fn report_count(value: usize, field: &'static str) -> Result<u64, CheckpointAdminError> {
    u64::try_from(value).map_err(|_| CheckpointAdminError::CountOverflow { field })
}

fn quarantine_reports(
    table: &super::apply::CheckpointTable,
) -> Result<Vec<QuarantineInspectionReport>, CheckpointAdminError> {
    let mut reports = Vec::with_capacity(table.quarantined_len());
    for (file_id, record) in table.iter() {
        if record.lifecycle_state != LifecycleState::Quarantined {
            continue;
        }
        let file_id = hex::encode(file_id.0);
        let evidence = record.quarantine_evidence.as_ref().ok_or_else(|| {
            CheckpointAdminError::MissingQuarantineEvidence {
                file_id: file_id.clone(),
            }
        })?;
        reports.push(QuarantineInspectionReport {
            file_id,
            epoch: record.file_epoch,
            committed_offset: record.committed_offset,
            framing_resume: record.framing_resume.into(),
            locator: record.locator.into(),
            reason_code: evidence.reason_code,
            observed_size: evidence.observed_size,
            quarantine_epoch: evidence.quarantine_epoch,
            quarantine_time_unix_nano: evidence.quarantine_time_unix_nano,
            advisory_path: (&record.advisory_path).into(),
        });
    }
    reports.sort_by(|left, right| left.file_id.cmp(&right.file_id));
    Ok(reports)
}

impl From<FramingResume> for FramingResumeReport {
    fn from(value: FramingResume) -> Self {
        match value {
            FramingResume::Clean => Self::Clean,
            FramingResume::Continuation {
                record_start_offset,
                record_end_offset,
                next_fragment_index,
            } => Self::Continuation {
                record_start_offset,
                record_end_offset,
                next_fragment_index,
            },
        }
    }
}

impl From<super::primitives::Locator> for LocatorReport {
    fn from(value: super::primitives::Locator) -> Self {
        match value {
            super::primitives::Locator::Unspecified => Self::Unspecified,
            super::primitives::Locator::PosixDevIno { dev, ino } => Self::PosixDevIno { dev, ino },
            super::primitives::Locator::WindowsVolumeFileId {
                volume_serial,
                file_id,
            } => Self::WindowsVolumeFileId {
                volume_serial,
                file_id: hex::encode(file_id),
            },
        }
    }
}

impl From<&AdvisoryPath> for AdvisoryPathReport {
    fn from(value: &AdvisoryPath) -> Self {
        let kind = match value.kind() {
            AdvisoryPathKind::Unavailable => AdvisoryPathKindReport::Unavailable,
            AdvisoryPathKind::UnixBytes => AdvisoryPathKindReport::UnixBytes,
            AdvisoryPathKind::WindowsUtf16Le => AdvisoryPathKindReport::WindowsUtf16Le,
        };
        Self {
            kind,
            truncated: value.is_truncated(),
            full_path_len: value.full_path_len(),
            stored_path_hex: hex::encode(value.stored_path_bytes()),
            full_path_digest: hex::encode(value.full_path_digest()),
        }
    }
}

#[derive(Debug)]
struct BackupSourceArtifact {
    name: String,
    role: EvidenceArtifactRole,
    generation: Option<u64>,
    artifact: &'static str,
    max_bytes: u64,
}

fn inventory_recognized_artifacts(
    namespace_dir: &Path,
    limits: &StoreLimits,
) -> Result<Vec<BackupSourceArtifact>, CheckpointAdminError> {
    let entries =
        std::fs::read_dir(namespace_dir).map_err(|source| CheckpointAdminError::BackupIo {
            operation: "list the checkpoint namespace for evidence backup",
            path: namespace_dir.to_path_buf(),
            source,
        })?;
    let mut sources = Vec::new();
    let mut temporary_count = 0usize;
    let mut final_generations = BTreeSet::new();
    for entry in entries {
        let entry = entry.map_err(|source| CheckpointAdminError::BackupIo {
            operation: "read a checkpoint namespace entry for evidence backup",
            path: namespace_dir.to_path_buf(),
            source,
        })?;
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        let classification = match classify_namespace_artifact(name) {
            Some(classification) => classification,
            None => {
                if let Some(canonical_name) = canonical_artifact_name_ignoring_ascii_case(name) {
                    return Err(CheckpointAdminError::NonCanonicalArtifactName {
                        path: namespace_dir.join(name),
                        canonical_name,
                    });
                }
                continue;
            }
        };
        if classification.kind == NamespaceArtifactKind::OwnershipLock {
            continue;
        }
        if classification.form != ArtifactForm::Final {
            if temporary_count >= MAX_TEMP_FILES {
                return Err(StoreError::TooManyTemporaryFiles {
                    dir: namespace_dir.to_path_buf(),
                    max: MAX_TEMP_FILES,
                }
                .into());
            }
            temporary_count += 1;
        }
        if classification.form == ArtifactForm::Final
            && let Some(generation) = classification.generation
            && final_generations.insert(generation)
            && final_generations.len() > MAX_GENERATIONS_ON_DISK
        {
            return Err(StoreError::TooManyGenerations {
                dir: namespace_dir.to_path_buf(),
                max: MAX_GENERATIONS_ON_DISK,
            }
            .into());
        }

        let (role, artifact, max_bytes) = match (classification.kind, classification.form) {
            (NamespaceArtifactKind::Current, ArtifactForm::Final) => (
                EvidenceArtifactRole::Current,
                "CURRENT marker",
                MARKER_READ_MAX_BYTES,
            ),
            (
                NamespaceArtifactKind::Current,
                ArtifactForm::CreateTemporary | ArtifactForm::CompactTemporary,
            ) => (
                EvidenceArtifactRole::CurrentTemporary,
                "CURRENT temporary marker",
                MARKER_READ_MAX_BYTES,
            ),
            (NamespaceArtifactKind::Snapshot, ArtifactForm::Final) => (
                EvidenceArtifactRole::Snapshot,
                "snapshot",
                limits.max_snapshot_bytes,
            ),
            (
                NamespaceArtifactKind::Snapshot,
                ArtifactForm::CreateTemporary | ArtifactForm::CompactTemporary,
            ) => (
                EvidenceArtifactRole::SnapshotTemporary,
                "snapshot temporary",
                limits.max_snapshot_bytes,
            ),
            (NamespaceArtifactKind::Wal, ArtifactForm::Final) => {
                (EvidenceArtifactRole::Wal, "WAL", limits.max_wal_bytes)
            }
            (
                NamespaceArtifactKind::Wal,
                ArtifactForm::CreateTemporary | ArtifactForm::CompactTemporary,
            ) => (
                EvidenceArtifactRole::WalTemporary,
                "WAL temporary",
                limits.max_wal_bytes,
            ),
            (NamespaceArtifactKind::OwnershipLock, _) => continue,
        };
        sources.push(BackupSourceArtifact {
            name: name.to_owned(),
            role,
            generation: classification.generation,
            artifact,
            max_bytes,
        });
    }
    sources.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(sources)
}

fn inventory_backup_artifacts(
    namespace_dir: &Path,
    limits: &StoreLimits,
    selected_generation: u64,
) -> Result<Vec<BackupSourceArtifact>, CheckpointAdminError> {
    let sources = inventory_recognized_artifacts(namespace_dir, limits)?;
    let has_current = sources
        .iter()
        .any(|source| source.role == EvidenceArtifactRole::Current && source.generation.is_none());
    let has_selected_snapshot = sources.iter().any(|source| {
        source.role == EvidenceArtifactRole::Snapshot
            && source.generation == Some(selected_generation)
    });
    let has_selected_wal = sources.iter().any(|source| {
        source.role == EvidenceArtifactRole::Wal && source.generation == Some(selected_generation)
    });
    if !has_current {
        return Err(CheckpointAdminError::RequiredArtifactMissing {
            artifact: "canonical CURRENT marker",
            path: namespace_dir.join(CURRENT_FILE_NAME),
        });
    }
    if !has_selected_snapshot {
        return Err(CheckpointAdminError::RequiredArtifactMissing {
            artifact: "selected canonical snapshot",
            path: namespace_dir.join(snapshot_file_name(selected_generation)),
        });
    }
    if !has_selected_wal {
        return Err(CheckpointAdminError::RequiredArtifactMissing {
            artifact: "selected canonical WAL",
            path: namespace_dir.join(wal_file_name(selected_generation)),
        });
    }
    Ok(sources)
}

fn create_backup_directory(path: &Path) -> Result<(), CheckpointAdminError> {
    #[allow(unused_mut)]
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        let _ = builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(CheckpointAdminError::BackupDestinationExists {
                path: path.to_path_buf(),
            })
        }
        Err(source) => Err(CheckpointAdminError::BackupIo {
            operation: "create the checkpoint evidence-backup destination",
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn write_backup_file(path: &Path, bytes: &[u8]) -> Result<(), CheckpointAdminError> {
    let mut options = OpenOptions::new();
    let _ = options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        let _ = options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        let _ = options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let mut file = options
        .open(path)
        .map_err(|source| CheckpointAdminError::BackupIo {
            operation: "create a checkpoint evidence-backup file",
            path: path.to_path_buf(),
            source,
        })?;
    file.write_all(bytes)
        .map_err(|source| CheckpointAdminError::BackupIo {
            operation: "write a checkpoint evidence-backup file",
            path: path.to_path_buf(),
            source,
        })?;
    file.sync_all()
        .map_err(|source| CheckpointAdminError::BackupIo {
            operation: "sync a checkpoint evidence-backup file",
            path: path.to_path_buf(),
            source,
        })
}

#[cfg(test)]
mod tests;
