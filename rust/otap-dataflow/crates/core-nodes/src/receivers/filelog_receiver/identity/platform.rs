// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Handle-based file opening, locator extraction, and bounded identity
//! evidence collection.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

use super::{CandidateEvidence, IdentityError};
use crate::receivers::filelog_receiver::checkpoint::primitives::{
    ADVISORY_PATH_MAX_BYTES, Locator,
};

/// An opened regular-file candidate and the evidence collected from that
/// same handle.
#[derive(Debug)]
pub(crate) struct OpenedCandidate {
    pub(crate) file: File,
    pub(crate) evidence: CandidateEvidence,
}

/// Opens one candidate without write access and collects all identity
/// evidence from the resulting handle.
pub(crate) fn open_candidate(
    path: &Path,
    follow_symlinks: bool,
    fingerprint_bytes: u16,
    ignored_header_bytes: u32,
) -> Result<OpenedCandidate, IdentityError> {
    open_candidate_at(
        path,
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
        locator_from_handle_cancellable(&file, open_path, follow_symlinks, &mut cancelled)?
    else {
        return Ok(None);
    };
    let Some((fingerprint, size)) = collect_consistent_fingerprint_cancellable(
        &file,
        open_path,
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
    let advisory_path = encode_advisory_path(advisory_path)?;

    Ok(Some(OpenedCandidate {
        file,
        evidence: CandidateEvidence {
            locator,
            size,
            fingerprint,
            advisory_path,
        },
    }))
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
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let mut options = OpenOptions::new();
    options
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
    if !follow_symlinks {
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options.open(path)
}

#[cfg(not(any(unix, windows)))]
fn open_read_only(_path: &Path, _follow_symlinks: bool) -> io::Result<File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "filelog identity is supported only on Unix and Windows",
    ))
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
fn read_fingerprint_cancellable(
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
fn read_fingerprint_cancellable(
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
fn read_fingerprint_cancellable(
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
pub(crate) fn encode_advisory_path(path: &Path) -> Result<Vec<u8>, IdentityError> {
    use std::os::unix::ffi::OsStrExt;

    bounded_path_bytes(path, path.as_os_str().as_bytes().to_vec())
}

#[cfg(windows)]
pub(crate) fn encode_advisory_path(path: &Path) -> Result<Vec<u8>, IdentityError> {
    use std::os::windows::ffi::OsStrExt;

    let mut bytes = Vec::new();
    for code_unit in path.as_os_str().encode_wide() {
        bytes.extend_from_slice(&code_unit.to_be_bytes());
    }
    bounded_path_bytes(path, bytes)
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn encode_advisory_path(path: &Path) -> Result<Vec<u8>, IdentityError> {
    Err(IdentityError::UnsupportedPlatform {
        path: path.to_path_buf(),
    })
}

fn bounded_path_bytes(path: &Path, bytes: Vec<u8>) -> Result<Vec<u8>, IdentityError> {
    if bytes.len() > ADVISORY_PATH_MAX_BYTES {
        return Err(IdentityError::AdvisoryPathTooLong {
            path: path.to_path_buf(),
            bytes: bytes.len(),
            maximum: ADVISORY_PATH_MAX_BYTES,
        });
    }
    Ok(bytes)
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

        assert_eq!(encoded, path.as_os_str().as_bytes());
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
    /// byte bound.
    /// Guarantees: path encoding reports the exact bound violation instead
    /// of truncating two distinct paths to the same diagnostic evidence.
    #[test]
    fn oversized_advisory_path_is_rejected() {
        let path = PathBuf::from("x".repeat(ADVISORY_PATH_MAX_BYTES + 1));
        assert!(matches!(
            encode_advisory_path(&path),
            Err(IdentityError::AdvisoryPathTooLong { .. })
        ));
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
    /// Guarantees: path evidence is the reversible big-endian byte encoding
    /// of native UTF-16 units, not a lossy UTF-8 conversion.
    #[test]
    fn windows_advisory_path_uses_big_endian_utf16_units() {
        use std::os::windows::ffi::OsStrExt;

        let path = PathBuf::from("C:\\logs\\snowman-\u{2603}-rocket-\u{1f680}.log");
        let expected: Vec<u8> = path
            .as_os_str()
            .encode_wide()
            .flat_map(u16::to_be_bytes)
            .collect();

        assert_eq!(encode_advisory_path(&path).unwrap(), expected);
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
