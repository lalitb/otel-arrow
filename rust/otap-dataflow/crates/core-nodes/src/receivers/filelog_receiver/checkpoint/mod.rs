// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Version-1 filelog durable checkpoint codec.
//!
//! This module implements exactly the byte format specified in
//! [`docs/filelog-checkpoint-format.md`](../../../../../../../docs/filelog-checkpoint-format.md):
//! the `CURRENT` marker, the snapshot file, the append-only WAL, the eight
//! logical WAL operations (`register_file`, `update_progress`,
//! `reset_after_truncate`, `update_fingerprint`, `update_metadata`,
//! `quarantine_file`, `reset_quarantined_file`, `remove_file`), and the
//! framing-profile canonical serialization and digest.
//!
//! Scope note: the codec modules here ([`primitives`], [`current_marker`],
//! [`snapshot`], [`wal`], [`apply`], [`framing_profile`]) only encode,
//! decode, and replay checkpoint bytes against an in-memory table. They
//! perform no filesystem I/O, no namespace locking, no atomic file
//! replacement, and no OS-specific locator lookups.
//!
//! [`store`] is the durable half built on top of them: it owns the
//! namespace directory, the ownership lock, generation selection, recovery,
//! WAL appends, sync policy, compaction, and retention. Everything in
//! [`store`] blocks and must run on the receiver's dedicated
//! read/checkpoint OS thread, never in async code.
//!
//! [`admin`] provides exclusive synchronous inspection and evidence backup
//! for an existing namespace. It reuses the store's bounded recovery decoder
//! through non-mutating filesystem paths and never repairs durable state.
//!
//! Locators are represented purely as normalized data ([`primitives::Locator`])
//! with no OS FFI, so this module and its tests compile and run identically
//! on Unix and non-Unix targets.

pub mod admin;
pub mod apply;
pub mod current_marker;
pub mod error;
pub mod framing_profile;
pub mod namespace;
pub mod primitives;
pub mod snapshot;
pub mod store;
pub mod wal;

#[cfg(test)]
mod test_vectors;
#[cfg(test)]
mod tests;

pub use admin::{
    AdvisoryPathKindReport, AdvisoryPathReport, CheckpointAdminError, CheckpointAdminSession,
    CheckpointInspectionReport, EVIDENCE_BACKUP_MANIFEST_FILE_NAME,
    EVIDENCE_BACKUP_MANIFEST_VERSION, EvidenceArtifact, EvidenceArtifactRole,
    EvidenceBackupManifest, FramingResumeReport, LocatorReport, NamespaceValidationReport,
    NativePathKindReport, NativePathReport, QuarantineInspectionReport,
};
pub use apply::{CheckpointTable, TableRecord};
pub use error::{ApplyError, DecodeError, EncodeError};
pub use namespace::{
    CHECKPOINT_NAMESPACE_COMPONENT_MAX_BYTES, CHECKPOINT_NAMESPACE_ID_MAX_BYTES,
    CHECKPOINT_NAMESPACE_VERSION, CheckpointNamespace, CheckpointNamespaceError,
    FILELOG_NAMESPACE_DIRECTORY,
};
pub use primitives::{
    AdvisoryPath, AdvisoryPathKind, CommittedFrontierGuard, CommittedFrontierWindow, FileId,
    FramingResume, LifecycleState, Locator,
};
pub use snapshot::{QuarantineEvidence, SnapshotContents, SnapshotRecord};
pub use store::error::StoreError;
pub use store::limits::StoreLimits;
pub use store::{AppendOutcome, CheckpointStore, RecoveryReport, StoreOptions, StoreStats};
pub use wal::WalContents;
