// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Durable filesystem primitives shared by generation creation, compaction,
//! and recovery.
//!
//! Every function here is synchronous and blocking by design: the durable
//! checkpoint store runs on the receiver's dedicated read/checkpoint OS
//! thread and never performs filesystem I/O from an async context (see the
//! [`super`] module documentation).
//!
//! Three properties matter for correctness and are implemented here rather
//! than at each call site:
//!
//! - **Atomic publication.** A file is written to a temporary name *in the
//!   same directory*, synced, and then renamed over its final name. A
//!   partially written file therefore never appears under a name recovery
//!   reads, and the rename is atomic because both names live in one
//!   directory.
//! - **Bounded reads.** A file's length is validated against a configured
//!   maximum before any buffer is allocated for it, and the read itself is
//!   capped, so a corrupted or hostile length can never drive an unbounded
//!   allocation.
//! - **Safe permissions.** On Unix the namespace directory is created
//!   `0700` and every file `0600`: durable checkpoint state records the
//!   paths and progress of collected log files and is not world-readable.
//!   On Windows the files inherit the engine state directory's ACL; native
//!   CI must verify that deployment-level ACL is private.

use std::fs::{File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::Path;

use super::error::StoreError;
use super::fault::{FaultPlan, FaultPoint};
use super::layout::temp_file_name;

/// Directory mode for the checkpoint namespace on Unix.
#[cfg(unix)]
const NAMESPACE_DIR_MODE: u32 = 0o700;
/// File mode for every checkpoint artifact on Unix.
#[cfg(unix)]
const CHECKPOINT_FILE_MODE: u32 = 0o600;

/// The four persistence boundaries of one atomically published artifact.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AtomicWriteFaults {
    /// Before the temporary file is created.
    pub(crate) before_write: FaultPoint,
    /// After the bytes are written, before they are synced.
    pub(crate) after_write: FaultPoint,
    /// After the temporary file is synced, before the rename.
    pub(crate) after_sync: FaultPoint,
    /// After the rename installs the artifact under its final name.
    pub(crate) after_publish: FaultPoint,
}

impl AtomicWriteFaults {
    /// Boundaries of the snapshot file.
    pub(crate) const SNAPSHOT: Self = Self {
        before_write: FaultPoint::BeforeSnapshotWrite,
        after_write: FaultPoint::AfterSnapshotWrite,
        after_sync: FaultPoint::AfterSnapshotSync,
        after_publish: FaultPoint::AfterSnapshotPublish,
    };
    /// Boundaries of the WAL file.
    pub(crate) const WAL: Self = Self {
        before_write: FaultPoint::BeforeWalWrite,
        after_write: FaultPoint::AfterWalWrite,
        after_sync: FaultPoint::AfterGenerationWalSync,
        after_publish: FaultPoint::AfterWalPublish,
    };
    /// Boundaries of the `CURRENT` marker.
    pub(crate) const MARKER: Self = Self {
        before_write: FaultPoint::BeforeMarkerWrite,
        after_write: FaultPoint::AfterMarkerWrite,
        after_sync: FaultPoint::AfterMarkerSync,
        after_publish: FaultPoint::AfterMarkerPublish,
    };
}

/// An atomic-write failure, together with whether the destination may have
/// changed when the failure happened.
///
/// The caller needs this distinction for the `CURRENT` marker: a failure
/// before the rename leaves the previously selected generation authoritative
/// and is safe to retry, while a failure after the rename -- or a documented
/// side-effecting Windows replacement failure -- means the authoritative
/// marker can no longer be inferred from the live store handle.
#[derive(Debug)]
pub(crate) struct AtomicWriteError {
    /// The underlying failure.
    pub(crate) error: StoreError,
    /// Whether the destination was published or may otherwise have changed.
    pub(crate) destination_may_have_changed: bool,
}

impl AtomicWriteError {
    fn staged(error: StoreError) -> Self {
        Self {
            error,
            destination_may_have_changed: false,
        }
    }

    fn published(error: StoreError) -> Self {
        Self {
            error,
            destination_may_have_changed: true,
        }
    }
}

#[derive(Debug)]
struct ReplaceFileError {
    source: std::io::Error,
    destination_may_have_changed: bool,
}

/// Creates the checkpoint namespace directory (and any missing parent),
/// with restrictive permissions on Unix. Succeeds if it already exists.
pub(crate) fn create_namespace_dir(dir: &Path) -> Result<(), StoreError> {
    let mut missing_directories = Vec::new();
    let mut candidate = dir;
    loop {
        match std::fs::symlink_metadata(candidate) {
            Ok(_) => break,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                missing_directories.push(candidate.to_path_buf());
            }
            Err(source) => {
                return Err(StoreError::Io {
                    operation: "inspect a checkpoint namespace ancestor",
                    path: candidate.to_path_buf(),
                    source,
                });
            }
        }
        candidate = match candidate.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
    }

    let mut builder = std::fs::DirBuilder::new();
    let _ = builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        let _ = builder.mode(NAMESPACE_DIR_MODE);
    }
    builder.create(dir).map_err(|source| StoreError::Io {
        operation: "create the checkpoint namespace directory",
        path: dir.to_path_buf(),
        source,
    })?;
    let metadata = std::fs::symlink_metadata(dir).map_err(|source| StoreError::Io {
        operation: "inspect the checkpoint namespace directory",
        path: dir.to_path_buf(),
        source,
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(StoreError::UnsafeFilesystemObject {
            path: dir.to_path_buf(),
            reason: "the checkpoint namespace must be a real directory, not a symlink",
        });
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(StoreError::UnsafeFilesystemObject {
                path: dir.to_path_buf(),
                reason: "the checkpoint namespace must not be a Windows reparse point",
            });
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(NAMESPACE_DIR_MODE))
            .map_err(|source| StoreError::Io {
                operation: "set private checkpoint namespace permissions",
                path: dir.to_path_buf(),
                source,
            })?;
    }
    // Persist every newly created directory entry from the existing
    // ancestor downward. Syncing only the namespace itself would not make
    // the namespace name durable in its parent after first creation.
    for created in missing_directories.iter().rev() {
        if let Some(parent) = created.parent() {
            sync_directory(if parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            })?;
        }
    }
    Ok(())
}

/// Applies platform flags that open the named object itself rather than
/// following a final-component symlink or reparse point.
fn no_follow(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        let _ = options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        let _ = options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
}

/// Validates an opened checkpoint file and repairs its Unix permissions.
pub(super) fn secure_checkpoint_file(
    file: &File,
    path: &Path,
    operation: &'static str,
) -> Result<(), StoreError> {
    let metadata = file.metadata().map_err(|source| StoreError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(StoreError::UnsafeFilesystemObject {
            path: path.to_path_buf(),
            reason: "checkpoint artifacts must be regular files, not symlinks or reparse points",
        });
    }
    #[cfg(windows)]
    {
        use std::mem::MaybeUninit;
        use std::os::windows::io::AsRawHandle as _;

        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT, GetFileInformationByHandle,
        };

        let mut information = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
        // SAFETY: `file` owns a live Windows file handle for the duration of
        // the call, and `information` points to writable, correctly aligned
        // storage of exactly the structure the API initializes.
        let succeeded =
            unsafe { GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr()) };
        if succeeded == 0 {
            return Err(StoreError::Io {
                operation: "inspect a Windows checkpoint file handle",
                path: path.to_path_buf(),
                source: std::io::Error::last_os_error(),
            });
        }
        // SAFETY: a nonzero return from GetFileInformationByHandle
        // guarantees that the output structure was initialized.
        let information = unsafe { information.assume_init() };
        if information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(StoreError::UnsafeFilesystemObject {
                path: path.to_path_buf(),
                reason: "checkpoint artifacts must not be Windows reparse points",
            });
        }
        if information.nNumberOfLinks != 1 {
            return Err(StoreError::UnsafeFilesystemObject {
                path: path.to_path_buf(),
                reason: "checkpoint artifacts must not have multiple hard links",
            });
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        if metadata.nlink() != 1 {
            return Err(StoreError::UnsafeFilesystemObject {
                path: path.to_path_buf(),
                reason: "checkpoint artifacts must not have multiple hard links",
            });
        }
        file.set_permissions(std::fs::Permissions::from_mode(CHECKPOINT_FILE_MODE))
            .map_err(|source| StoreError::Io {
                operation: "set private checkpoint file permissions",
                path: path.to_path_buf(),
                source,
            })?;
    }
    Ok(())
}

/// Opens a new temporary checkpoint file without following or truncating an
/// existing filesystem object.
fn create_file(path: &Path, operation: &'static str) -> Result<File, StoreError> {
    let mut options = OpenOptions::new();
    let _ = options.write(true).create_new(true);
    no_follow(&mut options);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        let _ = options.mode(CHECKPOINT_FILE_MODE);
    }
    let file = options.open(path).map_err(|source| StoreError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    })?;
    secure_checkpoint_file(&file, path, operation)?;
    Ok(file)
}

/// Writes `bytes` to `final_name` in `dir` through a same-directory
/// temporary file, syncing the bytes before the installing rename.
///
/// The directory itself is not synced here: a caller publishing several
/// artifacts syncs the directory once, at the point its own durable
/// sequence requires.
pub(crate) fn write_file_atomically(
    dir: &Path,
    final_name: &str,
    bytes: &[u8],
    plan: &mut FaultPlan,
    faults: AtomicWriteFaults,
) -> Result<(), AtomicWriteError> {
    let temp_path = dir.join(temp_file_name(final_name));
    let final_path = dir.join(final_name);

    plan.check(faults.before_write)
        .map_err(AtomicWriteError::staged)?;

    // A previous staged write may have failed before publication. Removing
    // the known temporary name is safe: unlinking a symlink removes the link
    // itself rather than its target, and create_new below prevents a race
    // from turning into truncation of an injected object.
    let _removed = remove_file_if_present(&temp_path).map_err(AtomicWriteError::staged)?;
    let mut file = create_file(&temp_path, "create a checkpoint temporary file")
        .map_err(AtomicWriteError::staged)?;
    file.write_all(bytes).map_err(|source| {
        AtomicWriteError::staged(StoreError::Io {
            operation: "write a checkpoint temporary file",
            path: temp_path.clone(),
            source,
        })
    })?;

    plan.check(faults.after_write)
        .map_err(AtomicWriteError::staged)?;

    file.sync_all().map_err(|source| {
        AtomicWriteError::staged(StoreError::Io {
            operation: "sync a checkpoint temporary file",
            path: temp_path.clone(),
            source,
        })
    })?;
    drop(file);

    plan.check(faults.after_sync)
        .map_err(AtomicWriteError::staged)?;

    replace_file(&temp_path, &final_path).map_err(|failure| {
        let error = StoreError::Io {
            operation: "atomically install a checkpoint temporary file",
            path: final_path.clone(),
            source: failure.source,
        };
        if failure.destination_may_have_changed {
            AtomicWriteError::published(error)
        } else {
            AtomicWriteError::staged(error)
        }
    })?;

    plan.check(faults.after_publish)
        .map_err(AtomicWriteError::published)
}

/// Installs `temp_path` at `final_path` atomically on the current platform.
#[cfg(not(windows))]
fn replace_file(temp_path: &Path, final_path: &Path) -> Result<(), ReplaceFileError> {
    std::fs::rename(temp_path, final_path).map_err(|source| ReplaceFileError {
        source,
        destination_may_have_changed: false,
    })
}

/// Installs `temp_path` at `final_path` with Windows replacement semantics.
///
/// `std::fs::rename` cannot replace an existing destination on Windows.
/// `ReplaceFileW` is the supported atomic replacement API when `CURRENT`
/// (or a previously staged generation file) already exists. A new
/// destination uses `MoveFileExW` with write-through so first publication
/// is also a same-volume atomic rename and does not return before the move
/// is flushed. Both handles created by this module are closed before this
/// call, satisfying `ReplaceFileW`'s sharing requirements.
#[cfg(windows)]
pub(super) fn windows_replace_failure_may_have_changed(raw_os_error: Option<i32>) -> bool {
    use windows_sys::Win32::Foundation::{
        ERROR_UNABLE_TO_MOVE_REPLACEMENT, ERROR_UNABLE_TO_MOVE_REPLACEMENT_2,
    };

    raw_os_error
        .and_then(|code| u32::try_from(code).ok())
        .is_some_and(|code| {
            code == ERROR_UNABLE_TO_MOVE_REPLACEMENT || code == ERROR_UNABLE_TO_MOVE_REPLACEMENT_2
        })
}

#[cfg(windows)]
fn replace_file(temp_path: &Path, final_path: &Path) -> Result<(), ReplaceFileError> {
    use std::os::windows::ffi::OsStrExt as _;
    use std::ptr;

    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_WRITE_THROUGH, MoveFileExW, ReplaceFileW,
    };

    fn wide(path: &Path) -> std::io::Result<Vec<u16>> {
        let file_name = path.file_name().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "checkpoint publication path has no file name",
            )
        })?;
        let parent = path.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "checkpoint publication path has no parent directory",
            )
        })?;
        // `canonicalize` returns Windows extended-length (`\\?\`) syntax.
        // Canonicalizing the existing parent works for both a new and an
        // existing destination and handles drive and UNC paths correctly.
        let extended_path = std::fs::canonicalize(parent)?.join(file_name);
        let mut value: Vec<u16> = extended_path.as_os_str().encode_wide().collect();
        if value.contains(&0) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "checkpoint path contains an embedded NUL",
            ));
        }
        value.push(0);
        Ok(value)
    }

    let temp = wide(temp_path).map_err(|source| ReplaceFileError {
        source,
        destination_may_have_changed: false,
    })?;
    let final_path_wide = wide(final_path).map_err(|source| ReplaceFileError {
        source,
        destination_may_have_changed: false,
    })?;
    let destination_exists = match std::fs::symlink_metadata(final_path) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(source) => {
            return Err(ReplaceFileError {
                source,
                destination_may_have_changed: false,
            });
        }
    };

    // SAFETY: both path buffers are NUL-terminated and remain alive for the
    // call. The optional backup and exclusion pointers are null. The
    // temporary file handle was closed after sync_all, and this store never
    // keeps a handle to CURRENT open while publishing it.
    let succeeded = unsafe {
        if destination_exists {
            ReplaceFileW(
                final_path_wide.as_ptr(),
                temp.as_ptr(),
                ptr::null(),
                0,
                ptr::null(),
                ptr::null(),
            )
        } else {
            MoveFileExW(
                temp.as_ptr(),
                final_path_wide.as_ptr(),
                MOVEFILE_WRITE_THROUGH,
            )
        }
    };
    if succeeded == 0 {
        let source = std::io::Error::last_os_error();
        let destination_may_have_changed =
            windows_replace_failure_may_have_changed(source.raw_os_error());
        return Err(ReplaceFileError {
            source,
            destination_may_have_changed,
        });
    }
    Ok(())
}

/// Syncs `dir` so that renames performed inside it are themselves durable.
///
/// On Unix a rename is atomic but its directory entry is not guaranteed
/// durable until the containing directory is synced. On other platforms
/// (Windows) a directory handle cannot be opened and synced through
/// `std::fs`, and NTFS journals the metadata change with the rename itself,
/// so this is a documented no-op there -- matching the existing journald and
/// Quiver durability code in this repository.
#[cfg(unix)]
pub(crate) fn sync_directory(dir: &Path) -> Result<(), StoreError> {
    let handle = File::open(dir).map_err(|source| StoreError::Io {
        operation: "open the checkpoint namespace directory for sync",
        path: dir.to_path_buf(),
        source,
    })?;
    handle.sync_all().map_err(|source| StoreError::Io {
        operation: "sync the checkpoint namespace directory",
        path: dir.to_path_buf(),
        source,
    })
}

/// See the Unix implementation; this is a documented no-op on platforms
/// where a directory handle cannot be synced through `std::fs`.
///
/// Windows publication still syncs each temporary file before replacement;
/// first publication additionally uses `MOVEFILE_WRITE_THROUGH`, and
/// replacement uses `ReplaceFileW`. Windows has no supported `std::fs`
/// equivalent of Unix directory `fsync`, so the remaining metadata
/// durability depends on the filesystem's rename/replace journaling. This is
/// a platform limitation, not a claim that the no-op is equivalent to Unix
/// directory syncing.
#[cfg(not(unix))]
pub(crate) fn sync_directory(_dir: &Path) -> Result<(), StoreError> {
    Ok(())
}

/// Reads `path` in full, refusing any file longer than `max` bytes before
/// allocating a buffer for it. Returns `Ok(None)` if the file does not
/// exist; every other failure is reported.
pub(crate) fn read_file_bounded(
    path: &Path,
    artifact: &'static str,
    max: u64,
) -> Result<Option<Vec<u8>>, StoreError> {
    let mut options = OpenOptions::new();
    let _ = options.read(true);
    no_follow(&mut options);
    let file = match options.open(path) {
        Ok(file) => file,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(StoreError::Io {
                operation: "open a checkpoint file",
                path: path.to_path_buf(),
                source,
            });
        }
    };
    secure_checkpoint_file(&file, path, "validate a checkpoint file")?;
    let metadata = file.metadata().map_err(|source| StoreError::Io {
        operation: "read checkpoint file metadata",
        path: path.to_path_buf(),
        source,
    })?;
    let len = metadata.len();
    if len > max {
        return Err(StoreError::FileTooLarge {
            artifact,
            path: path.to_path_buf(),
            len,
            max,
        });
    }
    // Checked, not cast: on a 32-bit target a length that is valid as a
    // `u64` may still exceed `usize`, and an allocation must never be sized
    // by a wrapped value.
    let capacity = usize::try_from(len).map_err(|_| StoreError::FileTooLarge {
        artifact,
        path: path.to_path_buf(),
        len,
        max,
    })?;
    let buffer = read_bounded_contents(file, capacity, path, artifact, max)?;
    Ok(Some(buffer))
}

/// Reads at most `max + 1` bytes so growth after metadata inspection is
/// detected rather than decoded as a silently truncated artifact.
pub(super) fn read_bounded_contents(
    mut file: File,
    capacity: usize,
    path: &Path,
    artifact: &'static str,
    max: u64,
) -> Result<Vec<u8>, StoreError> {
    let max_capacity = usize::try_from(max).map_err(|_| StoreError::FileTooLarge {
        artifact,
        path: path.to_path_buf(),
        len: max,
        max,
    })?;
    let initial_capacity = capacity.min(max_capacity);
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(initial_capacity)
        .map_err(|source| StoreError::Allocation {
            artifact,
            path: path.to_path_buf(),
            requested: initial_capacity,
            source,
        })?;

    let mut chunk = [0u8; 8 * 1024];
    loop {
        let remaining = max_capacity.saturating_sub(buffer.len());
        let read_capacity = remaining.saturating_add(1).min(chunk.len());
        let read = file
            .read(&mut chunk[..read_capacity])
            .map_err(|source| StoreError::Io {
                operation: "read a checkpoint file",
                path: path.to_path_buf(),
                source,
            })?;
        if read == 0 {
            break;
        }
        if read > remaining {
            let observed = buffer
                .len()
                .checked_add(read)
                .and_then(|len| u64::try_from(len).ok())
                .unwrap_or_else(|| max.saturating_add(1));
            return Err(StoreError::FileTooLarge {
                artifact,
                path: path.to_path_buf(),
                len: observed,
                max,
            });
        }
        buffer
            .try_reserve_exact(read)
            .map_err(|source| StoreError::Allocation {
                artifact,
                path: path.to_path_buf(),
                requested: buffer.len().saturating_add(read),
                source,
            })?;
        buffer.extend_from_slice(&chunk[..read]);
    }
    Ok(buffer)
}

/// Truncates `path` to `len` bytes and syncs it, discarding a structurally
/// incomplete trailing WAL region so subsequent appends continue from the
/// last complete transaction.
pub(crate) fn truncate_file(path: &Path, len: u64) -> Result<(), StoreError> {
    let mut options = OpenOptions::new();
    let _ = options.write(true);
    no_follow(&mut options);
    let file = options.open(path).map_err(|source| StoreError::Io {
        operation: "open a checkpoint WAL to discard its torn tail",
        path: path.to_path_buf(),
        source,
    })?;
    secure_checkpoint_file(
        &file,
        path,
        "validate a checkpoint WAL before truncating it",
    )?;
    file.set_len(len).map_err(|source| StoreError::Io {
        operation: "truncate a checkpoint WAL to its last complete transaction",
        path: path.to_path_buf(),
        source,
    })?;
    file.sync_all().map_err(|source| StoreError::Io {
        operation: "sync a truncated checkpoint WAL",
        path: path.to_path_buf(),
        source,
    })
}

/// Opens an existing WAL for appending. Every write lands at the current end
/// of file, so a concurrent reader always sees whole prefixes.
pub(crate) fn open_for_append(path: &Path) -> Result<File, StoreError> {
    let mut options = OpenOptions::new();
    let _ = options.append(true);
    no_follow(&mut options);
    let file = options.open(path).map_err(|source| StoreError::Io {
        operation: "open the checkpoint WAL for appending",
        path: path.to_path_buf(),
        source,
    })?;
    secure_checkpoint_file(&file, path, "validate a checkpoint WAL before appending")?;
    Ok(file)
}

/// Removes `path`, reporting whether it existed. A file that is already gone
/// is not an error for idempotent cleanup, but every other failure is
/// reported.
pub(crate) fn remove_file_if_present(path: &Path) -> Result<bool, StoreError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(StoreError::Io {
            operation: "remove a checkpoint file",
            path: path.to_path_buf(),
            source,
        }),
    }
}
