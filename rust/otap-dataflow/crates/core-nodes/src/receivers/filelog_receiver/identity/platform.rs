// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Handle-based file opening, locator extraction, and bounded identity
//! evidence collection.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

use sha2::{Digest, Sha256};

use super::{CandidateEvidence, IdentityError};
use crate::receivers::filelog_receiver::checkpoint::primitives::{
    ADVISORY_PATH_STORED_MAX_BYTES, AdvisoryPath, COMMITTED_FRONTIER_GUARD_WINDOW_BYTES,
    CommittedFrontierGuard, CommittedFrontierWindow, Locator,
};

/// An opened regular-file candidate and the evidence collected from that
/// same handle.
#[derive(Debug)]
pub(crate) struct OpenedCandidate {
    pub(crate) file: File,
    pub(crate) evidence: CandidateEvidence,
}

/// Stable reset evidence sampled from one exact-locator regular-file handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StableEofEvidence {
    pub(crate) locator: Locator,
    pub(crate) offset: u64,
    pub(crate) committed_frontier_guard: CommittedFrontierGuard,
    pub(crate) fingerprint: Vec<u8>,
}

/// Replacement-stream fingerprint sampled from one exact-locator handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StableFingerprintEvidence {
    pub(crate) fingerprint: Vec<u8>,
}

struct OpenedLocator {
    file: File,
    locator: Locator,
}

/// Opens one candidate without write access and collects all identity
/// evidence from the resulting handle.
pub(crate) fn open_candidate(
    path: &Path,
    follow_symlinks: bool,
    fingerprint_bytes: u16,
    ignored_header_bytes: u32,
) -> Result<OpenedCandidate, IdentityError> {
    let expected_resolved_path =
        std::fs::canonicalize(path).map_err(|source| IdentityError::Io {
            operation: "resolve candidate before handle-bound open",
            path: path.to_path_buf(),
            source,
        })?;
    open_candidate_at_expected(
        path,
        &expected_resolved_path,
        path,
        follow_symlinks,
        fingerprint_bytes,
        ignored_header_bytes,
    )
}

/// Opens a resolved target while retaining the matched path as advisory
/// evidence.
pub(crate) fn open_candidate_at(
    open_path: &Path,
    advisory_path: &Path,
    follow_symlinks: bool,
    fingerprint_bytes: u16,
    ignored_header_bytes: u32,
) -> Result<OpenedCandidate, IdentityError> {
    open_candidate_at_cancellable(
        open_path,
        advisory_path,
        follow_symlinks,
        fingerprint_bytes,
        ignored_header_bytes,
        || false,
    )
    .map(|opened| opened.expect("non-cancellable candidate open cannot be cancelled"))
}

pub(crate) fn open_candidate_at_cancellable(
    open_path: &Path,
    advisory_path: &Path,
    follow_symlinks: bool,
    fingerprint_bytes: u16,
    ignored_header_bytes: u32,
    mut cancelled: impl FnMut() -> bool,
) -> Result<Option<OpenedCandidate>, IdentityError> {
    open_candidate_at_expected_cancellable(
        open_path,
        open_path,
        advisory_path,
        follow_symlinks,
        fingerprint_bytes,
        ignored_header_bytes,
        &mut cancelled,
    )
}

fn open_candidate_at_expected(
    open_path: &Path,
    expected_resolved_path: &Path,
    advisory_path: &Path,
    follow_symlinks: bool,
    fingerprint_bytes: u16,
    ignored_header_bytes: u32,
) -> Result<OpenedCandidate, IdentityError> {
    open_candidate_at_expected_cancellable(
        open_path,
        expected_resolved_path,
        advisory_path,
        follow_symlinks,
        fingerprint_bytes,
        ignored_header_bytes,
        &mut || false,
    )
    .map(|opened| opened.expect("non-cancellable candidate open cannot be cancelled"))
}

#[allow(clippy::too_many_arguments)]
fn open_candidate_at_expected_cancellable(
    open_path: &Path,
    expected_resolved_path: &Path,
    advisory_path: &Path,
    follow_symlinks: bool,
    fingerprint_bytes: u16,
    ignored_header_bytes: u32,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<Option<OpenedCandidate>, IdentityError> {
    let Some(opened) = open_verified_locator_at_expected_cancellable(
        open_path,
        expected_resolved_path,
        advisory_path,
        follow_symlinks,
        &mut *cancelled,
    )?
    else {
        return Ok(None);
    };
    let OpenedLocator { file, locator } = opened;
    let path_before_evidence = expected_resolved_path;
    let Some((fingerprint, size)) = collect_consistent_fingerprint_cancellable(
        &file,
        open_path,
        fingerprint_bytes,
        ignored_header_bytes,
        &mut *cancelled,
    )?
    else {
        return Ok(None);
    };
    let Some(committed_frontier_window) =
        read_committed_frontier_window_cancellable(&file, open_path, size, &mut *cancelled)?
    else {
        return Ok(None);
    };
    let Some(path_after_evidence) =
        resolved_path_from_handle_cancellable(&file, open_path, &mut *cancelled)?
    else {
        return Ok(None);
    };
    if !resolved_paths_equal(path_before_evidence, &path_after_evidence) {
        return Err(IdentityError::CandidateChangedDuringIdentity {
            path: advisory_path.to_path_buf(),
        });
    }
    if cancelled() {
        return Ok(None);
    }
    let advisory_path = encode_advisory_path(advisory_path)?;

    Ok(Some(OpenedCandidate {
        file,
        evidence: CandidateEvidence {
            locator,
            size,
            fingerprint,
            advisory_path,
            committed_frontier_window,
        },
    }))
}

/// Opens one path and returns only its handle-derived locator for a caller
/// that validates stability with another open. No fingerprint or
/// source-content bytes are read.
///
/// This intentionally does not compare a handle-derived path: some platforms
/// report another hard-link name for the same object. Callers must bracket
/// path-policy revalidation with two calls and require equal locators.
pub(crate) fn open_locator_for_stability_check_cancellable(
    open_path: &Path,
    follow_symlinks: bool,
    cancelled: impl FnMut() -> bool,
) -> Result<Option<Locator>, IdentityError> {
    open_locator_and_size_for_stability_check_cancellable(open_path, follow_symlinks, cancelled)
        .map(|observation| observation.map(|(locator, _size)| locator))
}

fn open_locator_and_size_for_stability_check_cancellable(
    open_path: &Path,
    follow_symlinks: bool,
    mut cancelled: impl FnMut() -> bool,
) -> Result<Option<(Locator, u64)>, IdentityError> {
    if cancelled() {
        return Ok(None);
    }
    let file = open_read_only(open_path, follow_symlinks);
    if cancelled() {
        return Ok(None);
    }
    let file = file.map_err(|source| IdentityError::Io {
        operation: "open candidate for locator stability",
        path: open_path.to_path_buf(),
        source,
    })?;
    let metadata = file.metadata();
    if cancelled() {
        return Ok(None);
    }
    let metadata = metadata.map_err(|source| IdentityError::Io {
        operation: "read locator-stability candidate metadata",
        path: open_path.to_path_buf(),
        source,
    })?;
    if !metadata.is_file() {
        return Err(IdentityError::NotRegularFile {
            path: open_path.to_path_buf(),
        });
    }
    let Some(locator) =
        locator_from_handle_cancellable(&file, open_path, follow_symlinks, &mut cancelled)?
    else {
        return Ok(None);
    };
    Ok(Some((locator, metadata.len())))
}

/// Opens an operator-supplied path, proves the immutable locator, and
/// samples a stable EOF plus its real trailing committed-frontier guard.
pub(crate) fn open_stable_eof(
    open_path: &Path,
    follow_symlinks: bool,
    expected_locator: Locator,
    fingerprint_bytes: u16,
    ignored_header_bytes: u32,
) -> Result<StableEofEvidence, IdentityError> {
    open_stable_eof_inner(
        open_path,
        follow_symlinks,
        expected_locator,
        fingerprint_bytes,
        ignored_header_bytes,
        || {},
    )
}

/// Opens an operator-selected reset-to-beginning source, proves its exact
/// locator, and collects bounded append-compatible fingerprint evidence.
pub(crate) fn open_stable_fingerprint(
    open_path: &Path,
    follow_symlinks: bool,
    expected_locator: Locator,
    fingerprint_bytes: u16,
    ignored_header_bytes: u32,
) -> Result<StableFingerprintEvidence, IdentityError> {
    let opened = open_candidate(
        open_path,
        follow_symlinks,
        fingerprint_bytes,
        ignored_header_bytes,
    )?;
    if opened.evidence.locator != expected_locator {
        return Err(IdentityError::ReopenLocatorMismatch {
            path: open_path.to_path_buf(),
            expected: expected_locator,
            found: opened.evidence.locator,
        });
    }
    Ok(StableFingerprintEvidence {
        fingerprint: opened.evidence.fingerprint,
    })
}

#[cfg(test)]
pub(crate) fn open_stable_eof_with_hook(
    open_path: &Path,
    follow_symlinks: bool,
    expected_locator: Locator,
    fingerprint_bytes: u16,
    ignored_header_bytes: u32,
    after_first_sample: impl FnOnce(),
) -> Result<StableEofEvidence, IdentityError> {
    open_stable_eof_inner(
        open_path,
        follow_symlinks,
        expected_locator,
        fingerprint_bytes,
        ignored_header_bytes,
        after_first_sample,
    )
}

fn open_stable_eof_inner(
    open_path: &Path,
    follow_symlinks: bool,
    expected_locator: Locator,
    fingerprint_bytes: u16,
    ignored_header_bytes: u32,
    after_first_sample: impl FnOnce(),
) -> Result<StableEofEvidence, IdentityError> {
    let file = open_read_only(open_path, follow_symlinks).map_err(|source| {
        if no_follow_rejected_symlink(&source, follow_symlinks) {
            IdentityError::SymlinkOrReparsePoint {
                path: open_path.to_path_buf(),
            }
        } else {
            IdentityError::Io {
                operation: "open reset-to-end source",
                path: open_path.to_path_buf(),
                source,
            }
        }
    })?;
    let first_metadata = file.metadata().map_err(|source| IdentityError::Io {
        operation: "read reset-to-end source metadata",
        path: open_path.to_path_buf(),
        source,
    })?;
    let first_locator =
        locator_from_handle_cancellable(&file, open_path, follow_symlinks, &mut || false)?
            .expect("non-cancellable reset-to-end locator collection cannot be cancelled");
    if !first_metadata.is_file() {
        return Err(IdentityError::NotRegularFile {
            path: open_path.to_path_buf(),
        });
    }
    if first_locator != expected_locator {
        return Err(IdentityError::ReopenLocatorMismatch {
            path: open_path.to_path_buf(),
            expected: expected_locator,
            found: first_locator,
        });
    }
    let offset = first_metadata.len();
    let first_fingerprint = read_fingerprint_cancellable(
        &file,
        u64::from(ignored_header_bytes),
        usize::from(fingerprint_bytes),
        &mut || false,
    )
    .map_err(|source| IdentityError::Io {
        operation: "read administrative reset fingerprint",
        path: open_path.to_path_buf(),
        source,
    })?
    .expect("non-cancellable reset fingerprint read cannot be cancelled");
    if !fingerprint_length_is_consistent(
        first_fingerprint.len(),
        offset,
        fingerprint_bytes,
        ignored_header_bytes,
    ) {
        return Err(IdentityError::CandidateChangedDuringIdentity {
            path: open_path.to_path_buf(),
        });
    }
    let window_len = offset.min(u64::from(COMMITTED_FRONTIER_GUARD_WINDOW_BYTES)) as usize;
    let window_offset = offset - window_len as u64;
    let first_window =
        read_fingerprint_cancellable(&file, window_offset, window_len, &mut || false)
            .map_err(|source| IdentityError::Io {
                operation: "read reset-to-end committed-frontier window",
                path: open_path.to_path_buf(),
                source,
            })?
            .expect("non-cancellable reset-to-end source read cannot be cancelled");
    if first_window.len() != window_len {
        return Err(IdentityError::CandidateChangedDuringIdentity {
            path: open_path.to_path_buf(),
        });
    }

    after_first_sample();

    let second_metadata = file.metadata().map_err(|source| IdentityError::Io {
        operation: "recheck reset-to-end source metadata",
        path: open_path.to_path_buf(),
        source,
    })?;
    let second_locator =
        locator_from_handle_cancellable(&file, open_path, follow_symlinks, &mut || false)?
            .expect("non-cancellable reset-to-end locator recheck cannot be cancelled");
    if !second_metadata.is_file()
        || second_metadata.len() != offset
        || second_locator != first_locator
    {
        return Err(IdentityError::CandidateChangedDuringIdentity {
            path: open_path.to_path_buf(),
        });
    }
    let second_fingerprint = read_fingerprint_cancellable(
        &file,
        u64::from(ignored_header_bytes),
        usize::from(fingerprint_bytes),
        &mut || false,
    )
    .map_err(|source| IdentityError::Io {
        operation: "reread administrative reset fingerprint",
        path: open_path.to_path_buf(),
        source,
    })?
    .expect("non-cancellable reset fingerprint reread cannot be cancelled");
    if second_fingerprint != first_fingerprint
        || !fingerprint_length_is_consistent(
            second_fingerprint.len(),
            offset,
            fingerprint_bytes,
            ignored_header_bytes,
        )
    {
        return Err(IdentityError::CandidateChangedDuringIdentity {
            path: open_path.to_path_buf(),
        });
    }
    let second_window =
        read_fingerprint_cancellable(&file, window_offset, window_len, &mut || false)
            .map_err(|source| IdentityError::Io {
                operation: "reread reset-to-end committed-frontier window",
                path: open_path.to_path_buf(),
                source,
            })?
            .expect("non-cancellable reset-to-end source reread cannot be cancelled");
    if second_window != first_window {
        return Err(IdentityError::CandidateChangedDuringIdentity {
            path: open_path.to_path_buf(),
        });
    }
    let final_metadata = file.metadata().map_err(|source| IdentityError::Io {
        operation: "finalize reset-to-end source metadata",
        path: open_path.to_path_buf(),
        source,
    })?;
    let final_locator =
        locator_from_handle_cancellable(&file, open_path, follow_symlinks, &mut || false)?
            .expect("non-cancellable reset-to-end final locator check cannot be cancelled");
    if !final_metadata.is_file() || final_metadata.len() != offset || final_locator != first_locator
    {
        return Err(IdentityError::CandidateChangedDuringIdentity {
            path: open_path.to_path_buf(),
        });
    }
    let (path_locator, path_size) =
        open_locator_and_size_for_stability_check_cancellable(open_path, follow_symlinks, || {
            false
        })?
        .expect("non-cancellable reset-to-end path recheck cannot be cancelled");
    if path_locator != first_locator || path_size != offset {
        return Err(IdentityError::CandidateChangedDuringIdentity {
            path: open_path.to_path_buf(),
        });
    }
    let committed_frontier_guard = CommittedFrontierGuard::compute(offset, &second_window)
        .map_err(|_| IdentityError::CandidateChangedDuringIdentity {
            path: open_path.to_path_buf(),
        })?;
    Ok(StableEofEvidence {
        locator: first_locator,
        offset,
        committed_frontier_guard,
        fingerprint: second_fingerprint,
    })
}

#[cfg(unix)]
fn no_follow_rejected_symlink(source: &io::Error, follow_symlinks: bool) -> bool {
    !follow_symlinks && source.raw_os_error() == Some(libc::ELOOP)
}

#[cfg(not(unix))]
fn no_follow_rejected_symlink(_source: &io::Error, _follow_symlinks: bool) -> bool {
    false
}

fn open_verified_locator_at_expected_cancellable(
    open_path: &Path,
    expected_resolved_path: &Path,
    advisory_path: &Path,
    follow_symlinks: bool,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<Option<OpenedLocator>, IdentityError> {
    if cancelled() {
        return Ok(None);
    }
    let file = open_read_only(open_path, follow_symlinks);
    if cancelled() {
        return Ok(None);
    }
    let file = file.map_err(|source| IdentityError::Io {
        operation: "open candidate",
        path: open_path.to_path_buf(),
        source,
    })?;
    let metadata = file.metadata();
    if cancelled() {
        return Ok(None);
    }
    let metadata = metadata.map_err(|source| IdentityError::Io {
        operation: "read candidate metadata",
        path: open_path.to_path_buf(),
        source,
    })?;
    if !metadata.is_file() {
        return Err(IdentityError::NotRegularFile {
            path: open_path.to_path_buf(),
        });
    }

    let Some(locator) =
        locator_from_handle_cancellable(&file, open_path, follow_symlinks, &mut *cancelled)?
    else {
        return Ok(None);
    };
    let Some(resolved_path) =
        resolved_path_from_handle_cancellable(&file, open_path, &mut *cancelled)?
    else {
        return Ok(None);
    };
    if !resolved_paths_equal(&resolved_path, expected_resolved_path) {
        return Err(IdentityError::CandidateChangedDuringIdentity {
            path: advisory_path.to_path_buf(),
        });
    }
    Ok(Some(OpenedLocator { file, locator }))
}

/// Result of reopening a known native locator.
pub(crate) enum ReopenCandidate {
    /// The durable fingerprint and source frontier remain compatible.
    Compatible(OpenedCandidate),
    /// The locator still matches, but size or fingerprint evidence proves an
    /// observable truncation that runtime policy must handle.
    Truncated(OpenedCandidate),
}

/// Reopens an existing logical reader, validates the exact locator, and
/// classifies durable fingerprint or source-frontier incompatibility as
/// observable truncation while retaining the verified handle.
pub(crate) fn reopen_candidate_at(
    open_path: &Path,
    advisory_path: &Path,
    follow_symlinks: bool,
    fingerprint_bytes: u16,
    ignored_header_bytes: u32,
    expected_locator: Locator,
    durable_fingerprint: &[u8],
    required_offset: u64,
) -> Result<ReopenCandidate, IdentityError> {
    reopen_candidate_at_cancellable(
        open_path,
        advisory_path,
        follow_symlinks,
        fingerprint_bytes,
        ignored_header_bytes,
        expected_locator,
        durable_fingerprint,
        required_offset,
        || false,
    )
    .map(|opened| opened.expect("non-cancellable candidate reopen cannot be cancelled"))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn reopen_candidate_at_cancellable(
    open_path: &Path,
    advisory_path: &Path,
    follow_symlinks: bool,
    fingerprint_bytes: u16,
    ignored_header_bytes: u32,
    expected_locator: Locator,
    durable_fingerprint: &[u8],
    required_offset: u64,
    mut cancelled: impl FnMut() -> bool,
) -> Result<Option<ReopenCandidate>, IdentityError> {
    let Some(opened) = open_candidate_at_cancellable(
        open_path,
        advisory_path,
        follow_symlinks,
        fingerprint_bytes,
        ignored_header_bytes,
        &mut cancelled,
    )?
    else {
        return Ok(None);
    };
    if cancelled() {
        return Ok(None);
    }
    if opened.evidence.locator != expected_locator {
        return Err(IdentityError::ReopenLocatorMismatch {
            path: open_path.to_path_buf(),
            expected: expected_locator,
            found: opened.evidence.locator,
        });
    }
    if !opened.evidence.fingerprint.starts_with(durable_fingerprint)
        || opened.evidence.size < required_offset
    {
        return Ok(Some(ReopenCandidate::Truncated(opened)));
    }
    Ok(Some(ReopenCandidate::Compatible(opened)))
}

pub(crate) fn collect_consistent_fingerprint(
    file: &File,
    path: &Path,
    fingerprint_bytes: u16,
    ignored_header_bytes: u32,
) -> Result<(Vec<u8>, u64), IdentityError> {
    collect_consistent_fingerprint_cancellable(
        file,
        path,
        fingerprint_bytes,
        ignored_header_bytes,
        &mut || false,
    )
    .map(|observation| {
        observation.expect("non-cancellable fingerprint collection cannot be cancelled")
    })
}

/// Reads the exact real committed-frontier window ending at `size` from
/// `file`: the last `min(size, 64)` raw bytes immediately preceding it.
///
/// This is the real evidence used at registration instead of a fabricated
/// placeholder: `size` is the exact offset a fresh registration's
/// `committed_offset` will use for `start_at: end` (or a recovery-mismatch
/// skip-to-end), so the window read here is byte-for-byte the same
/// evidence [`CommittedFrontierGuard::compute`] would need.
///
/// [`CommittedFrontierGuard::compute`]: crate::receivers::filelog_receiver::checkpoint::primitives::CommittedFrontierGuard::compute
fn read_committed_frontier_window_cancellable(
    file: &File,
    path: &Path,
    size: u64,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<Option<CommittedFrontierWindow>, IdentityError> {
    let window_len = size.min(u64::from(COMMITTED_FRONTIER_GUARD_WINDOW_BYTES)) as usize;
    let offset = size - window_len as u64;
    let Some(bytes) =
        read_fingerprint_cancellable(file, offset, window_len, cancelled).map_err(|source| {
            IdentityError::Io {
                operation: "read candidate committed-frontier window",
                path: path.to_path_buf(),
                source,
            }
        })?
    else {
        return Ok(None);
    };
    if bytes.len() != window_len {
        // The file shrank between the fingerprint observation and this
        // read; the caller's own consistency checks (a subsequent
        // fingerprint re-observation) will reject the candidate.
        return Err(IdentityError::CandidateChangedDuringIdentity {
            path: path.to_path_buf(),
        });
    }
    CommittedFrontierWindow::new(size, bytes)
        .map(Some)
        .map_err(|_| IdentityError::CandidateChangedDuringIdentity {
            path: path.to_path_buf(),
        })
}

pub(crate) fn collect_consistent_fingerprint_cancellable(
    file: &File,
    path: &Path,
    fingerprint_bytes: u16,
    ignored_header_bytes: u32,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<Option<(Vec<u8>, u64)>, IdentityError> {
    collect_consistent_fingerprint_cancellable_inner(
        file,
        path,
        fingerprint_bytes,
        ignored_header_bytes,
        cancelled,
        || {},
    )
}

#[cfg(test)]
pub(crate) fn collect_consistent_fingerprint_cancellable_with_hook(
    file: &File,
    path: &Path,
    fingerprint_bytes: u16,
    ignored_header_bytes: u32,
    cancelled: &mut impl FnMut() -> bool,
    after_first_observation: impl FnOnce(),
) -> Result<Option<(Vec<u8>, u64)>, IdentityError> {
    collect_consistent_fingerprint_cancellable_inner(
        file,
        path,
        fingerprint_bytes,
        ignored_header_bytes,
        cancelled,
        after_first_observation,
    )
}

fn collect_consistent_fingerprint_cancellable_inner(
    file: &File,
    path: &Path,
    fingerprint_bytes: u16,
    ignored_header_bytes: u32,
    cancelled: &mut impl FnMut() -> bool,
    after_first_observation: impl FnOnce(),
) -> Result<Option<(Vec<u8>, u64)>, IdentityError> {
    let Some((first_fingerprint, first_size)) = observe_fingerprint_cancellable(
        file,
        path,
        fingerprint_bytes,
        ignored_header_bytes,
        &mut *cancelled,
    )?
    else {
        return Ok(None);
    };
    after_first_observation();
    let Some((second_fingerprint, second_size)) = observe_fingerprint_cancellable(
        file,
        path,
        fingerprint_bytes,
        ignored_header_bytes,
        &mut *cancelled,
    )?
    else {
        return Ok(None);
    };
    if fingerprint_observations_are_compatible(
        &first_fingerprint,
        first_size,
        &second_fingerprint,
        second_size,
        fingerprint_bytes,
        ignored_header_bytes,
    ) {
        return Ok(Some((second_fingerprint, second_size)));
    }
    Err(IdentityError::CandidateChangedDuringIdentity {
        path: path.to_path_buf(),
    })
}

fn observe_fingerprint_cancellable(
    file: &File,
    path: &Path,
    fingerprint_bytes: u16,
    ignored_header_bytes: u32,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<Option<(Vec<u8>, u64)>, IdentityError> {
    if cancelled() {
        return Ok(None);
    }
    let fingerprint = read_fingerprint_cancellable(
        file,
        u64::from(ignored_header_bytes),
        usize::from(fingerprint_bytes),
        &mut *cancelled,
    )
    .map_err(|source| IdentityError::Io {
        operation: "read candidate fingerprint",
        path: path.to_path_buf(),
        source,
    })?;
    let Some(fingerprint) = fingerprint else {
        return Ok(None);
    };
    if cancelled() {
        return Ok(None);
    }
    let size = file.metadata();
    if cancelled() {
        return Ok(None);
    }
    let size = size
        .map_err(|source| IdentityError::Io {
            operation: "refresh candidate metadata",
            path: path.to_path_buf(),
            source,
        })?
        .len();
    Ok(Some((fingerprint, size)))
}

fn fingerprint_length_is_consistent(
    actual: usize,
    size: u64,
    fingerprint_bytes: u16,
    ignored_header_bytes: u32,
) -> bool {
    let expected =
        u64::from(fingerprint_bytes).min(size.saturating_sub(u64::from(ignored_header_bytes)));
    u64::try_from(actual).is_ok_and(|actual| actual == expected)
}

fn fingerprint_observations_are_compatible(
    first: &[u8],
    first_size: u64,
    second: &[u8],
    second_size: u64,
    fingerprint_bytes: u16,
    ignored_header_bytes: u32,
) -> bool {
    fingerprint_length_is_consistent(
        first.len(),
        first_size,
        fingerprint_bytes,
        ignored_header_bytes,
    ) && fingerprint_length_is_consistent(
        second.len(),
        second_size,
        fingerprint_bytes,
        ignored_header_bytes,
    ) && second_size >= first_size
        && second.starts_with(first)
}

#[cfg(unix)]
fn open_read_only(path: &Path, follow_symlinks: bool) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut options = OpenOptions::new();
    let _ = options.read(true);
    let mut flags = libc::O_CLOEXEC | libc::O_NONBLOCK;
    if !follow_symlinks {
        flags |= libc::O_NOFOLLOW;
    }
    options.custom_flags(flags).open(path)
}

#[cfg(windows)]
fn open_read_only(path: &Path, follow_symlinks: bool) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let mut options = OpenOptions::new();
    options
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
    let mut flags = FILE_FLAG_BACKUP_SEMANTICS;
    if !follow_symlinks {
        flags |= FILE_FLAG_OPEN_REPARSE_POINT;
    }
    options.custom_flags(flags);
    options.open(path)
}

#[cfg(not(any(unix, windows)))]
fn open_read_only(_path: &Path, _follow_symlinks: bool) -> io::Result<File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "filelog identity is supported only on Unix and Windows",
    ))
}

fn resolved_paths_equal(left: &Path, right: &Path) -> bool {
    left == right
}

#[cfg(target_os = "linux")]
fn resolved_path_from_handle_cancellable(
    file: &File,
    path: &Path,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<Option<std::path::PathBuf>, IdentityError> {
    use std::os::fd::AsRawFd as _;

    if cancelled() {
        return Ok(None);
    }
    let link = std::path::PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
    let resolved = std::fs::read_link(&link);
    if cancelled() {
        return Ok(None);
    }
    let resolved = resolved.map_err(|source| IdentityError::Io {
        operation: "resolve the opened Linux candidate handle through procfs",
        path: path.to_path_buf(),
        source,
    })?;
    if !resolved.is_absolute() {
        return Err(IdentityError::Io {
            operation: "validate the opened Linux candidate handle path",
            path: path.to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::InvalidData,
                "procfs returned a non-absolute candidate handle path",
            ),
        });
    }
    Ok(Some(resolved))
}

#[cfg(target_os = "macos")]
#[allow(
    unsafe_code,
    reason = "macOS F_GETPATH requires a raw descriptor and output buffer"
)]
fn resolved_path_from_handle_cancellable(
    file: &File,
    path: &Path,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<Option<std::path::PathBuf>, IdentityError> {
    use std::ffi::{CStr, OsString};
    use std::os::fd::AsRawFd as _;
    use std::os::unix::ffi::OsStringExt as _;

    if cancelled() {
        return Ok(None);
    }
    let mut buffer = [0u8; libc::PATH_MAX as usize];
    // SAFETY: `file` owns a live descriptor and `buffer` is writable for
    // PATH_MAX bytes, which is the contract of F_GETPATH.
    let result = unsafe {
        libc::fcntl(
            file.as_raw_fd(),
            libc::F_GETPATH,
            buffer.as_mut_ptr().cast::<i8>(),
        )
    };
    let source = (result == -1).then(io::Error::last_os_error);
    if cancelled() {
        return Ok(None);
    }
    if let Some(source) = source {
        return Err(IdentityError::Io {
            operation: "resolve the opened macOS candidate handle with F_GETPATH",
            path: path.to_path_buf(),
            source,
        });
    }
    let resolved = {
        // SAFETY: successful F_GETPATH writes a NUL-terminated path into the
        // fixed-size output buffer.
        let bytes = unsafe { CStr::from_ptr(buffer.as_ptr().cast::<i8>()) }.to_bytes();
        std::path::PathBuf::from(OsString::from_vec(bytes.to_vec()))
    };
    if !resolved.is_absolute() {
        return Err(IdentityError::Io {
            operation: "validate the opened macOS candidate handle path",
            path: path.to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::InvalidData,
                "F_GETPATH returned a non-absolute candidate handle path",
            ),
        });
    }
    Ok(Some(resolved))
}

#[cfg(windows)]
#[allow(
    unsafe_code,
    reason = "GetFinalPathNameByHandleW requires a raw handle and output buffer"
)]
fn resolved_path_from_handle_cancellable(
    file: &File,
    path: &Path,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<Option<std::path::PathBuf>, IdentityError> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt as _;
    use std::os::windows::io::AsRawHandle as _;
    use std::ptr;

    use windows_sys::Win32::Storage::FileSystem::{
        FILE_NAME_NORMALIZED, GetFinalPathNameByHandleW, VOLUME_NAME_DOS,
    };

    const MAX_FINAL_PATH_WIDE_UNITS: u32 = 32_768;

    if cancelled() {
        return Ok(None);
    }
    // SAFETY: `file` owns a live handle. A zero-sized query with a null
    // buffer asks Windows for the required UTF-16 capacity.
    let required = unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle(),
            ptr::null_mut(),
            0,
            FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
        )
    };
    let source = (required == 0).then(io::Error::last_os_error);
    if cancelled() {
        return Ok(None);
    }
    if let Some(source) = source {
        return Err(IdentityError::Io {
            operation: "size the opened Windows candidate handle path",
            path: path.to_path_buf(),
            source,
        });
    }
    if required > MAX_FINAL_PATH_WIDE_UNITS {
        return Err(IdentityError::Io {
            operation: "bound the opened Windows candidate handle path",
            path: path.to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::InvalidData,
                "Windows candidate handle path exceeds the supported extended-path bound",
            ),
        });
    }
    let capacity = required;
    let capacity_usize = usize::try_from(capacity).map_err(|_| IdentityError::Io {
        operation: "size the opened Windows candidate handle path buffer",
        path: path.to_path_buf(),
        source: io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows candidate handle path length does not fit usize",
        ),
    })?;
    let mut buffer = vec![0u16; capacity_usize];
    // SAFETY: `buffer` is writable for `capacity` UTF-16 units and the live
    // handle remains valid throughout the call.
    let written = unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle(),
            buffer.as_mut_ptr(),
            capacity,
            FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
        )
    };
    let source = (written == 0).then(io::Error::last_os_error);
    if cancelled() {
        return Ok(None);
    }
    if let Some(source) = source {
        return Err(IdentityError::Io {
            operation: "resolve the opened Windows candidate handle path",
            path: path.to_path_buf(),
            source,
        });
    }
    if written >= capacity {
        return Err(IdentityError::Io {
            operation: "validate the opened Windows candidate handle path length",
            path: path.to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::InvalidData,
                "Windows candidate handle path changed during resolution",
            ),
        });
    }
    buffer.truncate(usize::try_from(written).map_err(|_| IdentityError::Io {
        operation: "decode the opened Windows candidate handle path",
        path: path.to_path_buf(),
        source: io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows candidate handle path length does not fit usize",
        ),
    })?);
    let resolved = std::path::PathBuf::from(OsString::from_wide(&buffer));
    if !resolved.is_absolute() {
        return Err(IdentityError::Io {
            operation: "validate the opened Windows candidate handle path",
            path: path.to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::InvalidData,
                "Windows returned a non-absolute candidate handle path",
            ),
        });
    }
    Ok(Some(resolved))
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn resolved_path_from_handle_cancellable(
    _file: &File,
    path: &Path,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<Option<std::path::PathBuf>, IdentityError> {
    if cancelled() {
        Ok(None)
    } else {
        Err(IdentityError::UnsupportedPlatform {
            path: path.to_path_buf(),
        })
    }
}

#[cfg(not(any(unix, windows)))]
fn resolved_path_from_handle_cancellable(
    _file: &File,
    path: &Path,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<Option<std::path::PathBuf>, IdentityError> {
    if cancelled() {
        Ok(None)
    } else {
        Err(IdentityError::UnsupportedPlatform {
            path: path.to_path_buf(),
        })
    }
}

#[cfg(unix)]
fn locator_from_handle_cancellable(
    file: &File,
    path: &Path,
    _follow_symlinks: bool,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<Option<Locator>, IdentityError> {
    use std::os::unix::fs::MetadataExt;

    if cancelled() {
        return Ok(None);
    }
    let metadata = file.metadata();
    if cancelled() {
        return Ok(None);
    }
    let metadata = metadata.map_err(|source| IdentityError::Io {
        operation: "extract POSIX file identity",
        path: path.to_path_buf(),
        source,
    })?;
    Ok(Some(Locator::PosixDevIno {
        dev: metadata.dev(),
        ino: metadata.ino(),
    }))
}

#[cfg(windows)]
fn locator_from_handle_cancellable(
    file: &File,
    path: &Path,
    follow_symlinks: bool,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<Option<Locator>, IdentityError> {
    use std::mem::{size_of, zeroed};
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ID_INFO, FileIdInfo,
        GetFileInformationByHandle, GetFileInformationByHandleEx,
    };

    let handle = file.as_raw_handle();
    let mut basic: BY_HANDLE_FILE_INFORMATION = unsafe { zeroed() };
    if cancelled() {
        return Ok(None);
    }
    let basic_succeeded = unsafe { GetFileInformationByHandle(handle, &mut basic) };
    let basic_error = (basic_succeeded == 0).then(io::Error::last_os_error);
    if cancelled() {
        return Ok(None);
    }
    if let Some(source) = basic_error {
        return Err(IdentityError::Io {
            operation: "validate Windows candidate handle",
            path: path.to_path_buf(),
            source,
        });
    }
    if !follow_symlinks && basic.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(IdentityError::SymlinkOrReparsePoint {
            path: path.to_path_buf(),
        });
    }

    let mut identity: FILE_ID_INFO = unsafe { zeroed() };
    if cancelled() {
        return Ok(None);
    }
    let identity_succeeded = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileIdInfo,
            (&raw mut identity).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    };
    let identity_error = (identity_succeeded == 0).then(io::Error::last_os_error);
    if cancelled() {
        return Ok(None);
    }
    if let Some(source) = identity_error {
        return Err(IdentityError::Io {
            operation: "extract Windows file identity",
            path: path.to_path_buf(),
            source,
        });
    }

    Ok(Some(Locator::WindowsVolumeFileId {
        volume_serial: identity.VolumeSerialNumber,
        file_id: identity.FileId.Identifier,
    }))
}

#[cfg(not(any(unix, windows)))]
fn locator_from_handle_cancellable(
    _file: &File,
    path: &Path,
    _follow_symlinks: bool,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<Option<Locator>, IdentityError> {
    if cancelled() {
        return Ok(None);
    }
    Err(IdentityError::UnsupportedPlatform {
        path: path.to_path_buf(),
    })
}

#[cfg(unix)]
pub(crate) fn read_fingerprint_cancellable(
    file: &File,
    offset: u64,
    maximum: usize,
    cancelled: &mut impl FnMut() -> bool,
) -> io::Result<Option<Vec<u8>>> {
    use std::os::unix::fs::FileExt;

    read_bounded_at_cancellable(
        maximum,
        |buffer, relative| file.read_at(buffer, offset + relative),
        cancelled,
    )
}

#[cfg(windows)]
pub(crate) fn read_fingerprint_cancellable(
    file: &File,
    offset: u64,
    maximum: usize,
    cancelled: &mut impl FnMut() -> bool,
) -> io::Result<Option<Vec<u8>>> {
    use std::os::windows::fs::FileExt;

    read_bounded_at_cancellable(
        maximum,
        |buffer, relative| file.seek_read(buffer, offset + relative),
        cancelled,
    )
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn read_fingerprint_cancellable(
    _file: &File,
    _offset: u64,
    _maximum: usize,
    cancelled: &mut impl FnMut() -> bool,
) -> io::Result<Option<Vec<u8>>> {
    if cancelled() {
        return Ok(None);
    }
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "filelog identity is supported only on Unix and Windows",
    ))
}

fn read_bounded_at_cancellable(
    maximum: usize,
    mut read_at: impl FnMut(&mut [u8], u64) -> io::Result<usize>,
    cancelled: &mut impl FnMut() -> bool,
) -> io::Result<Option<Vec<u8>>> {
    let mut bytes = vec![0; maximum];
    let mut read = 0usize;
    while read < maximum {
        if cancelled() {
            return Ok(None);
        }
        let result = read_at(&mut bytes[read..], read as u64);
        if cancelled() {
            return Ok(None);
        }
        match result {
            Ok(0) => break,
            Ok(count) => {
                read = read.checked_add(count).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "fingerprint length overflow")
                })?;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    bytes.truncate(read);
    Ok(Some(bytes))
}

/// Performs one bounded positioned source read, retrying only interrupted
/// system calls. A short read or zero-byte EOF returns control to the fair
/// reader scheduler.
#[cfg(unix)]
pub(crate) fn read_source_at_cancellable(
    file: &File,
    offset: u64,
    buffer: &mut [u8],
    cancelled: &mut impl FnMut() -> bool,
) -> io::Result<Option<usize>> {
    use std::os::unix::fs::FileExt;

    loop {
        if cancelled() {
            return Ok(None);
        }
        let result = file.read_at(buffer, offset);
        if cancelled() {
            return Ok(None);
        }
        match result {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            result => return result.map(Some),
        }
    }
}

/// Performs one bounded positioned source read, retrying only interrupted
/// system calls. A short read or zero-byte EOF returns control to the fair
/// reader scheduler.
#[cfg(windows)]
pub(crate) fn read_source_at_cancellable(
    file: &File,
    offset: u64,
    buffer: &mut [u8],
    cancelled: &mut impl FnMut() -> bool,
) -> io::Result<Option<usize>> {
    use std::os::windows::fs::FileExt;

    loop {
        if cancelled() {
            return Ok(None);
        }
        let result = file.seek_read(buffer, offset);
        if cancelled() {
            return Ok(None);
        }
        match result {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            result => return result.map(Some),
        }
    }
}

/// Reports unsupported source reading on targets without a Phase 1 locator
/// contract.
#[cfg(not(any(unix, windows)))]
pub(crate) fn read_source_at_cancellable(
    _file: &File,
    _offset: u64,
    _buffer: &mut [u8],
    cancelled: &mut impl FnMut() -> bool,
) -> io::Result<Option<usize>> {
    if cancelled() {
        return Ok(None);
    }
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "filelog reading is supported only on Unix and Windows",
    ))
}

#[cfg(unix)]
pub(crate) fn encode_advisory_path(path: &Path) -> Result<AdvisoryPath, IdentityError> {
    use std::os::unix::ffi::OsStrExt;

    AdvisoryPath::from_unix_bytes(path.as_os_str().as_bytes()).map_err(|source| {
        IdentityError::InvalidAdvisoryPath {
            path: path.to_path_buf(),
            source,
        }
    })
}

#[cfg(windows)]
pub(crate) fn encode_advisory_path(path: &Path) -> Result<AdvisoryPath, IdentityError> {
    use std::os::windows::ffi::OsStrExt;

    // Collects the native UTF-16 code units once; `AdvisoryPath` then
    // derives the digest and stored suffix directly from this buffer
    // (streaming the digest per code unit and copying only the bounded
    // stored suffix), so no further full-length copy is made.
    let units: Vec<u16> = path.as_os_str().encode_wide().collect();
    AdvisoryPath::from_windows_utf16_units(&units).map_err(|source| {
        IdentityError::InvalidAdvisoryPath {
            path: path.to_path_buf(),
            source,
        }
    })
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn encode_advisory_path(path: &Path) -> Result<AdvisoryPath, IdentityError> {
    Err(IdentityError::UnsupportedPlatform {
        path: path.to_path_buf(),
    })
}

/// Whether the complete native path fits the reversible advisory-path bound.
#[cfg(unix)]
pub(crate) fn native_path_fits_advisory_bound(path: &Path) -> Result<bool, IdentityError> {
    use std::os::unix::ffi::OsStrExt;

    Ok(path.as_os_str().as_bytes().len() <= ADVISORY_PATH_STORED_MAX_BYTES)
}

/// Whether the complete native path fits the reversible advisory-path bound.
#[cfg(windows)]
pub(crate) fn native_path_fits_advisory_bound(path: &Path) -> Result<bool, IdentityError> {
    use std::os::windows::ffi::OsStrExt;

    let maximum_units = ADVISORY_PATH_STORED_MAX_BYTES / 2;
    Ok(path
        .as_os_str()
        .encode_wide()
        .take(maximum_units + 1)
        .count()
        <= maximum_units)
}

/// Whether the complete native path fits the reversible advisory-path bound.
#[cfg(not(any(unix, windows)))]
pub(crate) fn native_path_fits_advisory_bound(path: &Path) -> Result<bool, IdentityError> {
    Err(IdentityError::UnsupportedPlatform {
        path: path.to_path_buf(),
    })
}

/// Plain SHA-256 over the complete native path representation used by OTAP
/// provenance: Unix bytes or little-endian Windows UTF-16 code-unit bytes.
#[cfg(unix)]
pub(crate) fn native_path_sha256(path: &Path) -> Result<[u8; 32], IdentityError> {
    use std::os::unix::ffi::OsStrExt;

    Ok(Sha256::digest(path.as_os_str().as_bytes()).into())
}

/// Plain SHA-256 over the complete native path representation used by OTAP
/// provenance: Unix bytes or little-endian Windows UTF-16 code-unit bytes.
#[cfg(windows)]
pub(crate) fn native_path_sha256(path: &Path) -> Result<[u8; 32], IdentityError> {
    use std::os::windows::ffi::OsStrExt;

    let mut hasher = Sha256::new();
    for unit in path.as_os_str().encode_wide() {
        hasher.update(unit.to_le_bytes());
    }
    Ok(hasher.finalize().into())
}

/// Plain SHA-256 over the complete native path representation used by OTAP
/// provenance.
#[cfg(not(any(unix, windows)))]
pub(crate) fn native_path_sha256(path: &Path) -> Result<[u8; 32], IdentityError> {
    Err(IdentityError::UnsupportedPlatform {
        path: path.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::path::PathBuf;

    use tempfile::tempdir;

    use super::*;

    /// Scenario: a regular file has ignored prefix bytes and less content
    /// than the configured fingerprint window.
    /// Guarantees: evidence comes from the opened handle, skips the exact
    /// prefix, preserves the short fingerprint, and reports the handle size.
    #[test]
    fn opened_candidate_collects_short_bounded_evidence() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("app.log");
        std::fs::write(&path, b"headerpayload").unwrap();

        let candidate = open_candidate(&path, false, 32, 6).unwrap();

        assert_eq!(candidate.evidence.fingerprint, b"payload");
        assert_eq!(candidate.evidence.size, 13);
        assert_ne!(candidate.evidence.locator, Locator::Unspecified);
        assert_eq!(
            candidate.file.metadata().unwrap().len(),
            candidate.evidence.size
        );
    }

    /// Scenario: cancellation becomes visible after one fingerprint read but
    /// before its paired metadata observation.
    /// Guarantees: identity sampling returns cancellation without starting
    /// the metadata call or a second fingerprint observation.
    #[test]
    fn fingerprint_sampling_stops_between_filesystem_operations() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("cancel.log");
        std::fs::write(&path, b"payload").unwrap();
        let file = File::open(&path).unwrap();
        let mut cancellation_checks = 0usize;

        let observation =
            collect_consistent_fingerprint_cancellable(&file, &path, 4, 0, &mut || {
                let cancelled = cancellation_checks != 0;
                cancellation_checks += 1;
                cancelled
            })
            .unwrap();

        assert!(observation.is_none());
        assert_eq!(cancellation_checks, 2);
    }

    /// Scenario: cancellation becomes visible after the first platform
    /// handle query used to derive a source locator.
    /// Guarantees: locator sampling returns cancellation before any later
    /// handle query, including the Windows file-ID query.
    #[test]
    fn locator_sampling_stops_between_handle_queries() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("locator.log");
        std::fs::write(&path, b"payload").unwrap();
        let file = File::open(&path).unwrap();
        let mut cancellation_checks = 0usize;

        let locator = locator_from_handle_cancellable(&file, &path, false, &mut || {
            cancellation_checks += 1;
            cancellation_checks == 2
        })
        .unwrap();

        assert!(locator.is_none());
        assert_eq!(cancellation_checks, 2);
    }

    /// Scenario: a file grows from a short fingerprint to the configured
    /// evidence-window length.
    /// Guarantees: reopening preserves the locator and grows matching
    /// evidence without moving either file handle's stream position.
    #[test]
    fn fingerprint_grows_without_changing_locator_or_cursor() {
        use std::io::Seek;

        let directory = tempdir().unwrap();
        let path = directory.path().join("grow.log");
        std::fs::write(&path, b"ab").unwrap();
        let mut first = open_candidate(&path, false, 4, 0).unwrap();
        assert_eq!(first.file.stream_position().unwrap(), 0);

        let mut append = OpenOptions::new().append(true).open(&path).unwrap();
        append.write_all(b"cdmore").unwrap();
        let mut second = open_candidate(&path, false, 4, 0).unwrap();

        assert_eq!(first.evidence.locator, second.evidence.locator);
        assert_eq!(first.evidence.fingerprint, b"ab");
        assert_eq!(second.evidence.fingerprint, b"abcd");
        assert_eq!(second.file.stream_position().unwrap(), 0);
    }

    /// Scenario: two hard-link names are opened only to compare native
    /// locator stability during exclusion-policy revalidation.
    /// Guarantees: locator-only opens accept either path name and return the
    /// same object identity without depending on a platform's chosen
    /// handle-derived path alias.
    #[test]
    fn locator_stability_open_accepts_hard_link_aliases() {
        let directory = tempdir().unwrap();
        let first = directory.path().join("first.log");
        let second = directory.path().join("second.log");
        std::fs::write(&first, b"same").unwrap();
        std::fs::hard_link(&first, &second).unwrap();

        let first_locator = open_locator_for_stability_check_cancellable(&first, false, || false)
            .unwrap()
            .unwrap();
        let second_locator = open_locator_for_stability_check_cancellable(&second, false, || false)
            .unwrap()
            .unwrap();

        assert_eq!(first_locator, second_locator);
    }

    #[cfg(unix)]
    /// Scenario: a Unix path contains bytes that are not valid UTF-8.
    /// Guarantees: advisory metadata preserves the original `OsStr` bytes
    /// exactly and never uses lossy replacement.
    #[test]
    fn unix_advisory_path_preserves_non_utf8_bytes() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let directory = tempdir().unwrap();
        let name = OsStr::from_bytes(b"log-\xff");
        let path = directory.path().join(name);
        let encoded = encode_advisory_path(&path).unwrap();

        assert_eq!(encoded.stored_path_bytes(), path.as_os_str().as_bytes());
        assert!(!encoded.is_truncated());
    }

    #[cfg(unix)]
    /// Scenario: discovery reaches a symbolic link while following links is
    /// disabled and then enabled.
    /// Guarantees: no-follow opening rejects the link itself, while explicit
    /// follow mode identifies the regular target from the resulting handle.
    #[test]
    fn unix_symlink_policy_is_enforced_at_open() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().unwrap();
        let target = directory.path().join("target.log");
        let link = directory.path().join("link.log");
        std::fs::write(&target, b"target").unwrap();
        symlink(&target, &link).unwrap();

        assert!(open_candidate(&link, false, 16, 0).is_err());
        let followed = open_candidate(&link, true, 16, 0).unwrap();
        let direct = open_candidate(&target, false, 16, 0).unwrap();
        assert_eq!(followed.evidence.locator, direct.evidence.locator);
    }

    #[cfg(unix)]
    /// Scenario: a Unix source is unlinked while receiver and writer
    /// descriptors remain open, then the writer appends late bytes.
    /// Guarantees: the retained receiver descriptor reads the original inode
    /// through the late append even though no path can reopen it.
    #[test]
    fn unix_retained_handle_reads_after_unlink_and_late_write() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("unlink.log");
        std::fs::write(&path, b"old\n").unwrap();
        let opened = open_candidate(&path, false, 16, 0).unwrap();
        let mut writer = OpenOptions::new().append(true).open(&path).unwrap();

        std::fs::remove_file(&path).unwrap();
        writer.write_all(b"late\n").unwrap();
        writer.flush().unwrap();

        let mut bytes = [0; 9];
        assert_eq!(
            read_source_at_cancellable(&opened.file, 0, &mut bytes, &mut || false)
                .unwrap()
                .unwrap(),
            9
        );
        assert_eq!(&bytes, b"old\nlate\n");
        assert_eq!(opened.file.metadata().unwrap().len(), 9);
    }

    /// Scenario: a candidate advisory path exceeds the durable format's
    /// stored-byte bound.
    /// Guarantees: path encoding still succeeds -- an oversized path is
    /// durably truncated evidence, never rejected -- and the resulting
    /// value reports `is_truncated()` with the exact final
    /// `ADVISORY_PATH_STORED_MAX_BYTES` bytes stored.
    #[test]
    fn oversized_advisory_path_is_truncated_not_rejected() {
        use crate::receivers::filelog_receiver::checkpoint::primitives::ADVISORY_PATH_STORED_MAX_BYTES;

        let long_name = "x".repeat(ADVISORY_PATH_STORED_MAX_BYTES + 1);
        let path = PathBuf::from(&long_name);
        let encoded = encode_advisory_path(&path).unwrap();
        assert!(encoded.is_truncated());
        assert_eq!(
            encoded.stored_path_bytes().len(),
            ADVISORY_PATH_STORED_MAX_BYTES
        );
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let full = path.as_os_str().as_bytes();
            assert_eq!(encoded.full_path_len(), full.len() as u64);
            assert_eq!(
                encoded.stored_path_bytes(),
                &full[full.len() - ADVISORY_PATH_STORED_MAX_BYTES..]
            );
        }
    }

    /// Scenario: a fingerprint read reaches an early EOF but the following
    /// handle metadata says more evidence bytes were available, as can occur
    /// during concurrent growth or truncation.
    /// Guarantees: evidence-length validation accepts only the exact
    /// configured-or-EOF length and marks the inconsistent observation for
    /// bounded retry.
    #[test]
    fn fingerprint_length_detects_file_mutation_during_collection() {
        assert!(fingerprint_length_is_consistent(4, 4, 16, 0));
        assert!(fingerprint_length_is_consistent(16, 100, 16, 0));
        assert!(fingerprint_length_is_consistent(0, 4, 16, 8));
        assert!(!fingerprint_length_is_consistent(4, 100, 16, 0));
        assert!(!fingerprint_length_is_consistent(16, 4, 16, 0));
    }

    /// Scenario: two evidence observations see stable bytes, append-only
    /// growth, a same-size rewrite, and a truncation respectively.
    /// Guarantees: collection accepts stable/prefix-growing evidence but
    /// rejects changed bytes and non-monotonic size before recovery matching.
    #[test]
    fn fingerprint_observations_require_stable_prefix_bytes() {
        assert!(fingerprint_observations_are_compatible(
            b"same", 4, b"same", 4, 16, 0
        ));
        assert!(fingerprint_observations_are_compatible(
            b"ab", 2, b"abcd", 4, 16, 0
        ));
        assert!(!fingerprint_observations_are_compatible(
            b"aaaa", 4, b"bbbb", 4, 16, 0
        ));
        assert!(!fingerprint_observations_are_compatible(
            b"abcd", 4, b"abc", 3, 16, 0
        ));
    }

    #[cfg(windows)]
    /// Scenario: a Windows advisory path contains both BMP and surrogate-pair
    /// UTF-16 code units.
    /// Guarantees: path evidence is the reversible little-endian byte
    /// encoding of native UTF-16 units (`AdvisoryPathKind::WindowsUtf16Le`),
    /// not a lossy UTF-8 conversion.
    #[test]
    fn windows_advisory_path_uses_little_endian_utf16_units() {
        use std::os::windows::ffi::OsStrExt;

        let path = PathBuf::from("C:\\logs\\snowman-\u{2603}-rocket-\u{1f680}.log");
        let expected: Vec<u8> = path
            .as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect();

        let encoded = encode_advisory_path(&path).unwrap();
        assert_eq!(encoded.stored_path_bytes(), expected.as_slice());
        assert!(!encoded.is_truncated());
    }

    #[cfg(windows)]
    /// Scenario: a candidate remains open while its path is renamed, as a
    /// log rotator would do under move/create rotation.
    /// Guarantees: read/write/delete sharing permits the rename and the
    /// handle-derived volume/file ID remains stable at the new path.
    #[test]
    fn windows_candidate_sharing_and_identity_survive_rename() {
        let directory = tempdir().unwrap();
        let original = directory.path().join("active.log");
        let rotated = directory.path().join("active.log.1");
        std::fs::write(&original, b"line").unwrap();
        let candidate = open_candidate(&original, false, 16, 0).unwrap();

        std::fs::rename(&original, &rotated).unwrap();
        let reopened = open_candidate(&rotated, false, 16, 0).unwrap();

        assert_eq!(candidate.evidence.locator, reopened.evidence.locator);
        assert!(matches!(
            candidate.evidence.locator,
            Locator::WindowsVolumeFileId { .. }
        ));
        assert_eq!(candidate.file.metadata().unwrap().len(), 4);
    }

    #[cfg(windows)]
    /// Scenario: a Windows file symlink is opened with link following
    /// disabled and then explicitly enabled.
    /// Guarantees: `FILE_FLAG_OPEN_REPARSE_POINT` plus handle validation
    /// prevents a reparse point from bypassing no-follow policy, while
    /// follow mode identifies the regular target.
    #[test]
    fn windows_reparse_point_policy_is_enforced_at_open() {
        use std::os::windows::fs::symlink_file;

        let directory = tempdir().unwrap();
        let target = directory.path().join("target.log");
        let link = directory.path().join("link.log");
        std::fs::write(&target, b"target").unwrap();
        symlink_file(&target, &link).unwrap();

        assert!(open_candidate(&link, false, 16, 0).is_err());
        let followed = open_candidate(&link, true, 16, 0).unwrap();
        let direct = open_candidate(&target, false, 16, 0).unwrap();
        assert_eq!(followed.evidence.locator, direct.evidence.locator);
    }

    #[cfg(windows)]
    /// Scenario: another Windows handle denies every sharing mode before the
    /// receiver attempts to open the file.
    /// Guarantees: candidate opening surfaces the sharing violation instead
    /// of claiming identity evidence from a different path lookup or a
    /// success-shaped fallback.
    #[test]
    fn windows_incompatible_writer_sharing_is_reported() {
        use std::os::windows::fs::OpenOptionsExt;

        let directory = tempdir().unwrap();
        let path = directory.path().join("exclusive.log");
        std::fs::write(&path, b"line").unwrap();
        let exclusive = OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .unwrap();

        assert!(open_candidate(&path, false, 16, 0).is_err());
        drop(exclusive);
        assert!(open_candidate(&path, false, 16, 0).is_ok());
    }
}
