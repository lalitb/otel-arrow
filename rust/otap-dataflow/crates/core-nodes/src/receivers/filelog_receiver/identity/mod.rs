// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Secure handle-based file identity evidence and durable recovery matching.

pub(crate) mod matcher;
pub(crate) mod platform;

use std::io;
use std::path::PathBuf;

use thiserror::Error;

use super::checkpoint::{
    AdvisoryPath, CommittedFrontierWindow, EncodeError, FileId, Locator, StoreError,
};

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
    /// Reversible, platform-specific advisory path.
    pub(crate) advisory_path: AdvisoryPath,
    /// The exact real committed-frontier window ending at `size`, read from
    /// the same validated handle before registration. Never fabricated:
    /// registering at offset `0` uses the empty window; a `start_at: end`
    /// (or recovery-mismatch skip-to-end) registration at offset `size`
    /// uses this real trailing evidence.
    pub(crate) committed_frontier_window: CommittedFrontierWindow,
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
    /// Reopening a logical reader read a committed-frontier window that
    /// does not match the durably recorded guard: the size and fingerprint
    /// prefix are unchanged, but the raw bytes immediately preceding the
    /// committed offset are not the same evidence the checkpoint recorded.
    #[error(
        "filelog reader reopen at {path} committed-frontier window does not match the durable guard"
    )]
    ReopenFrontierGuardMismatch {
        /// Reader path used for the reopen.
        path: PathBuf,
    },
    /// The current target cannot provide a supported native locator.
    #[cfg_attr(
        any(unix, windows),
        allow(
            dead_code,
            reason = "constructed only by unsupported-platform cfg implementations"
        )
    )]
    #[error("filelog handle identity is unsupported on this platform: {path}")]
    UnsupportedPlatform {
        /// Candidate path.
        path: PathBuf,
    },
    /// A candidate's native path could not be turned into a durable
    /// `AdvisoryPath` value (a genuine construction error, for example an
    /// empty native representation); never raised merely because a path is
    /// long, since an oversized path is durably truncated evidence, not an
    /// error.
    #[error("filelog advisory path {path} is invalid: {source}")]
    InvalidAdvisoryPath {
        /// Candidate path.
        path: PathBuf,
        /// The underlying codec construction failure.
        #[source]
        source: EncodeError,
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
    /// A matching checkpoint uses an incompatible resumption profile.
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "the non-admission test resolver preserves its historical structured error"
        )
    )]
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
