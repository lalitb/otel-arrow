// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Secure handle-based file identity evidence and durable recovery matching.

pub(crate) mod matcher;
pub(crate) mod platform;

use std::io;
use std::path::PathBuf;

use thiserror::Error;

use super::checkpoint::{FileId, Locator, StoreError};

/// Bounded identity evidence collected from one open regular-file handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CandidateEvidence {
    /// Handle-derived runtime locator.
    pub(crate) locator: Locator,
    /// File size observed from the handle after fingerprint collection.
    pub(crate) size: u64,
    /// Up to the configured number of evidence bytes after the ignored
    /// prefix.
    pub(crate) fingerprint: Vec<u8>,
    /// Reversible, platform-specific advisory path bytes.
    pub(crate) advisory_path: Vec<u8>,
}

/// Identity collection, matching, or durable-registration failure.
#[derive(Debug, Error)]
pub(crate) enum IdentityError {
    /// A filesystem operation failed.
    #[error("could not {operation} at {path}: {source}")]
    Io {
        /// Stable operation description.
        operation: &'static str,
        /// Candidate path.
        path: PathBuf,
        /// OS error.
        #[source]
        source: io::Error,
    },
    /// The opened object was not a regular file.
    #[error("filelog candidate is not a regular file: {path}")]
    NotRegularFile {
        /// Candidate path.
        path: PathBuf,
    },
    /// A no-follow open reached a Windows reparse point.
    #[error("filelog candidate is a symlink or reparse point: {path}")]
    SymlinkOrReparsePoint {
        /// Candidate path.
        path: PathBuf,
    },
    /// The file changed while its bounded evidence window was collected.
    #[error("filelog candidate changed while identity evidence was read: {path}")]
    CandidateChangedDuringIdentity {
        /// Candidate path.
        path: PathBuf,
    },
    /// Reopening a logical reader reached a different native file identity.
    #[error("filelog reader reopen at {path} found locator {found:?}, expected {expected:?}")]
    ReopenLocatorMismatch {
        /// Reader path used for the reopen.
        path: PathBuf,
        /// Locator owned by the logical reader and its runtime lease.
        expected: Locator,
        /// Locator extracted from the reopened handle.
        found: Locator,
    },
    /// Reopening a logical reader found matching evidence that no longer
    /// extends the durable prefix.
    #[error("filelog reader reopen fingerprint no longer extends durable evidence at {path}")]
    ReopenFingerprintMismatch {
        /// Reader path used for the reopen.
        path: PathBuf,
    },
    /// Reopening a logical reader found a source shorter than its durable
    /// checkpoint frontier.
    #[error(
        "filelog reader reopen at {path} found size {size}, below committed offset {committed_offset}"
    )]
    ReopenOffsetBeyondSize {
        /// Reader path used for the reopen.
        path: PathBuf,
        /// Durable source-byte frontier.
        committed_offset: u64,
        /// Size observed from the reopened handle.
        size: u64,
    },
    /// The current target cannot provide a supported native locator.
    #[error("filelog handle identity is unsupported on this platform: {path}")]
    UnsupportedPlatform {
        /// Candidate path.
        path: PathBuf,
    },
    /// Reversible advisory path bytes exceed the durable bound.
    #[error(
        "filelog advisory path {path} encodes to {bytes} bytes, exceeding the {maximum}-byte maximum"
    )]
    AdvisoryPathTooLong {
        /// Candidate path.
        path: PathBuf,
        /// Encoded byte count.
        bytes: usize,
        /// Durable maximum.
        maximum: usize,
    },
    /// Candidate evidence violates the validated identity configuration.
    #[error("invalid filelog identity evidence: {reason}")]
    InvalidEvidence {
        /// Exact invariant violation.
        reason: &'static str,
    },
    /// One bounded resolution batch repeated a runtime locator.
    #[error("filelog candidate batch contains duplicate runtime locator {locator:?}")]
    DuplicateCandidateLocator {
        /// Duplicated locator.
        locator: Locator,
    },
    /// Corrupt or conflicting durable records claim one quarantined locator.
    #[error(
        "multiple checkpoint records claim quarantined runtime locator {locator:?}; recovery cannot safely select one"
    )]
    AmbiguousQuarantinedLocator {
        /// Conflicting locator.
        locator: Locator,
    },
    /// A matching checkpoint uses an incompatible resumption profile.
    #[error(
        "checkpoint file {file_id:?} uses framing profile version {stored_version} and digest \
         {stored_digest:02x?}, but the receiver requires version {configured_version} and digest \
         {configured_digest:02x?}; explicit migration or state reset is required"
    )]
    IncompatibleProfile {
        /// Durable identity whose state cannot safely resume.
        file_id: FileId,
        /// Stored recipe version.
        stored_version: u16,
        /// Stored digest.
        stored_digest: [u8; 32],
        /// Configured recipe version.
        configured_version: u16,
        /// Configured digest.
        configured_digest: [u8; 32],
    },
    /// Random identity generation repeatedly collided with live durable IDs.
    #[error("could not generate a unique opaque filelog file_id after {attempts} attempts")]
    FileIdCollisionLimit {
        /// Bounded attempt count.
        attempts: usize,
    },
    /// Durable checkpoint operation failed.
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[cfg(test)]
mod tests;
