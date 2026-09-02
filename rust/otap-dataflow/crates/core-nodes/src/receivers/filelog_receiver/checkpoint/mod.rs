// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Filelog checkpoint replay, storage, and administration.
//!
//! [`otel_arrow_dfe_filelog_checkpoint`] is the sole owner of version-1
//! checkpoint values and bytes. This module owns stateful replay, filesystem
//! namespace and storage, publication, retention, and administration.

pub mod admin;
pub mod apply;
pub mod error;
pub mod namespace;
pub(crate) mod path;
pub mod store;

#[cfg(test)]
mod test_vectors;

/// Narrow re-export grouping for the standalone CURRENT codec.
pub mod current_marker {
    pub use otel_arrow_dfe_filelog_checkpoint::{
        CURRENT_BYTES as CURRENT_MARKER_LEN, decode_current as decode_current_marker,
        encode_current as encode_current_marker,
    };
}

/// Narrow re-export grouping for standalone framing-profile values.
pub mod framing_profile {
    pub use otel_arrow_dfe_filelog_checkpoint::{
        FramingEncoding, FramingOnDecodeError, FramingProfileParams, MaxLogSizeBehavior,
        MultilineMode,
    };
}

/// Standalone durable values plus receiver-owned behavioral and sizing terms.
pub mod primitives {
    pub use otel_arrow_dfe_filelog_checkpoint::{
        ADVISORY_PATH_STORED_MAX_BYTES, AUDIT_REASON_MAX_BYTES, AdvisoryPath, AdvisoryPathKind,
        COMMITTED_FRONTIER_GUARD_WINDOW_BYTES, CommittedFrontierGuard, CommittedFrontierWindow,
        FINGERPRINT_MAX_BYTES, FRAMING_PROFILE_VERSION, FileId, FramingResume, LifecycleState,
        Locator, MAX_OPERATION_PAYLOAD_BYTES, MAX_PROGRESS_TX_BODY_BYTES,
        MAX_PROGRESS_TX_FRAME_BYTES, MAX_VALID_UPDATE_FINGERPRINT_PAYLOAD_BYTES,
        NAMESPACE_ID_MAX_BYTES, SNAPSHOT_MAX_RECORD_FRAME_BYTES, TX_HEADER_BYTES,
        TX_MIN_BODY_BYTES, TX_MIN_FRAME_BYTES, WAL_MAX_NON_PROGRESS_OPS_PER_TX, WAL_MAX_OPS_PER_TX,
        WAL_MAX_TX_BODY_BYTES, WAL_MAX_TX_FRAME_BYTES, crc32c, namespace_digest,
    };

    /// Current fingerprint recipe version used in framing-profile digests.
    pub const FINGERPRINT_PROFILE_VERSION: u16 = 1;
    /// Fixed fields retained by an encoded advisory path.
    pub const ADVISORY_PATH_FIXED_BYTES: usize = 1 + 1 + 8 + 2 + 32;
    /// Maximum multiline pattern bytes accepted by receiver configuration.
    pub const FRAMING_PATTERN_MAX_BYTES: usize = 4096;
    /// Trailing CRC width of one transaction frame.
    pub const TX_FRAME_CRC_BYTES: usize = 4;
    /// Encoded width of one committed-frontier guard.
    pub const COMMITTED_FRONTIER_GUARD_LEN: usize = 2 + 32;
    /// Maximum snapshot record payload derived from the codec frame bound.
    pub const SNAPSHOT_MAX_RECORD_PAYLOAD_BYTES: u64 = SNAPSHOT_MAX_RECORD_FRAME_BYTES - 8;
    /// Maximum operation frame derived from the codec payload bound.
    pub const MAX_OPERATION_FRAME_BYTES: u64 = MAX_OPERATION_PAYLOAD_BYTES + 8;
    /// Maximum valid fingerprint-update frame.
    pub const MAX_VALID_UPDATE_FINGERPRINT_FRAME_BYTES: u64 =
        MAX_VALID_UPDATE_FINGERPRINT_PAYLOAD_BYTES + 8;
    /// Maximum `register_file` payload used by resource admission.
    pub const REGISTER_FILE_MAX_OP_PAYLOAD_BYTES: u64 = 69_812;
    /// Maximum `update_progress` payload used by resource admission.
    pub const UPDATE_PROGRESS_MAX_OP_PAYLOAD_BYTES: u64 = 101;
    /// Maximum `update_progress` frame used by resource admission.
    pub const UPDATE_PROGRESS_MAX_OP_FRAME_BYTES: u64 = UPDATE_PROGRESS_MAX_OP_PAYLOAD_BYTES + 8;

    /// Current producer reason for a read-new truncate reset.
    pub const TRUNCATE_RESET_REASON_READ_NEW: u16 = 0x0001;
    /// Reserved zero reason code.
    pub const REASON_CODE_RESERVED: u16 = 0x0000;
    const QUARANTINE_REASON_RESERVED_V1: u16 = 0x0004;
    /// Quarantine reason for source decode failure.
    pub const QUARANTINE_REASON_DECODE: u16 = 0x0001;
    /// Quarantine reason for copy-truncate refusal.
    pub const QUARANTINE_REASON_TRUNCATE: u16 = 0x0002;
    /// Quarantine reason for recovery evidence mismatch.
    pub const QUARANTINE_REASON_RECOVERY_MISMATCH: u16 = 0x0003;
    /// Non-administrative removal reason for exact-locator supersession.
    pub const REMOVAL_REASON_LOCATOR_SUPERSEDED: u16 = 0x0001;
    /// Presence bit for an advisory-path metadata update.
    pub const METADATA_PATH_PRESENT: u8 = 0x01;
    /// Reserved metadata presence bits.
    pub const METADATA_PRESENCE_RESERVED_MASK: u8 = !METADATA_PATH_PRESENT;

    pub(crate) const fn quarantine_reason_is_reserved(reason_code: u16) -> bool {
        reason_code == REASON_CODE_RESERVED || reason_code == QUARANTINE_REASON_RESERVED_V1
    }
}

/// Narrow re-export grouping for standalone snapshot values and functions.
pub mod snapshot {
    pub use otel_arrow_dfe_filelog_checkpoint::{
        QuarantineEvidence, SNAPSHOT_FOOTER_BYTES as SNAPSHOT_FOOTER_LEN,
        SNAPSHOT_HEADER_BYTES as SNAPSHOT_HEADER_LEN, SnapshotRecord,
        decode_snapshot as decode_snapshot_with_limit, encode_snapshot,
    };
}

/// Standalone WAL values/functions plus receiver-owned transaction grouping.
pub mod wal {
    pub use otel_arrow_dfe_filelog_checkpoint::{
        MAX_OPERATION_PAYLOAD_BYTES, MAX_PROGRESS_TX_BODY_BYTES, MAX_PROGRESS_TX_FRAME_BYTES,
        MAX_VALID_UPDATE_FINGERPRINT_PAYLOAD_BYTES, Operation, QuarantineFile, RegisterFile,
        RemoveFile, ResetAfterTruncate, ResetQuarantineAction, ResetQuarantinedFile,
        TX_HEADER_BYTES, TX_MIN_BODY_BYTES, TX_MIN_FRAME_BYTES, Transaction, TransactionClass,
        TransactionScan, UpdateFingerprint, UpdateMetadata, UpdateProgress,
        WAL_HEADER_BYTES as WAL_HEADER_LEN, WAL_MAX_NON_PROGRESS_OPS_PER_TX, WAL_MAX_OPS_PER_TX,
        WAL_MAX_TX_BODY_BYTES, WAL_MAX_TX_FRAME_BYTES, decode_wal_header, encode_operation,
        encode_transaction, encode_wal_header, scan_next_transaction,
    };

    /// Outcome of classifying a receiver-owned operation group.
    pub(crate) enum ClassifyOutcome {
        Class(TransactionClass),
        Empty,
        Mixed,
    }

    /// Classifies an operation group without encoding it.
    pub(crate) fn classify_operations(operations: &[Operation]) -> ClassifyOutcome {
        if operations.is_empty() {
            return ClassifyOutcome::Empty;
        }
        let progress = operations
            .iter()
            .filter(|operation| matches!(operation, Operation::UpdateProgress(_)))
            .count();
        if progress == operations.len() {
            ClassifyOutcome::Class(TransactionClass::ProgressOnly)
        } else if progress == 0 {
            ClassifyOutcome::Class(TransactionClass::NonProgress)
        } else {
            ClassifyOutcome::Mixed
        }
    }
}

#[cfg(test)]
mod tests;

pub use admin::{
    AdvisoryPathKindReport, AdvisoryPathReport, AuditMetadata, CheckpointAdminError,
    CheckpointAdminSession, CheckpointEvidenceSession, CheckpointInspectionReport,
    CheckpointLifecycleReport, CommittedFrontierGuardReport, DataEffect,
    EVIDENCE_BACKUP_MANIFEST_FILE_NAME, EVIDENCE_BACKUP_MANIFEST_VERSION, EvidenceArtifact,
    EvidenceArtifactRole, EvidenceBackupManifest, ExpectedQuarantineState, FileMutationResult,
    FramingResumeReport, KeepFailedRequest, LocatorReport, NAMESPACE_VALIDATION_DETAIL_MAX_BYTES,
    NamespaceAuthorityFailureKind, NamespaceAuthorityFailureReport, NamespaceAuthorityReport,
    NamespaceValidationReport, NativePathKindReport, NativePathReport, QuarantineInspectionReport,
    QuarantineMutationAction, QuarantinedFileTarget, RemovalConsequence, RemoveQuarantinedRequest,
    ResetToBeginningRequest, ResetToEndEvidenceReport, ResetToEndRequest,
};
pub use apply::{CheckpointTable, TableRecord};
pub use error::ApplyError;
pub use namespace::{
    CHECKPOINT_NAMESPACE_COMPONENT_MAX_BYTES, CHECKPOINT_NAMESPACE_ID_MAX_BYTES,
    CHECKPOINT_NAMESPACE_VERSION, CheckpointNamespace, CheckpointNamespaceError,
    FILELOG_NAMESPACE_DIRECTORY,
};
pub use otel_arrow_dfe_filelog_checkpoint::{
    AdvisoryPath, AdvisoryPathKind, CommittedFrontierGuard, CommittedFrontierWindow, DecodeError,
    EncodeError, FileId, FramingResume, LifecycleState, Locator, QuarantineEvidence, Snapshot,
    SnapshotRecord,
};
pub use store::error::StoreError;
pub use store::limits::StoreLimits;
pub use store::{AppendOutcome, CheckpointStore, RecoveryReport, StoreOptions, StoreStats};
