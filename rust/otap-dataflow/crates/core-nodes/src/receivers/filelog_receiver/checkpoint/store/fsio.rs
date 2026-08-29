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
use std::path::{Path, PathBuf};

use super::error::StoreError;
use super::fault::{FaultPlan, FaultPoint};
use super::layout::{backup_file_name, temp_file_name};

/// Directory mode for the checkpoint namespace on Unix.
#[cfg(unix)]
const NAMESPACE_DIR_MODE: u32 = 0o700;
/// File mode for every checkpoint artifact on Unix.
#[cfg(unix)]
const CHECKPOINT_FILE_MODE: u32 = 0o600;

/// Whether a validated checkpoint read may repair private Unix file modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArtifactReadMode {
    /// Runtime store behavior: validate the object and repair its mode.
    RepairPermissions,
    /// Administration behavior: validate without changing any metadata.
    PreserveMetadata,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileIdentity {
    #[cfg(unix)]
    Unix { device: u64, inode: u64 },
    #[cfg(windows)]
    Windows {
        volume_serial: u64,
        file_id: [u8; 16],
    },
    #[cfg(not(any(unix, windows)))]
    Unsupported,
}

/// A canonical path retained together with an open handle and stable
/// filesystem identity.
///
/// Administration uses this to fail closed if a namespace, backup parent,
/// or backup directory is renamed or replaced while pathname-based
/// operations are in progress.
#[derive(Debug)]
pub(crate) struct DirectoryPathBinding {
    path: PathBuf,
    handle: File,
    identity: FileIdentity,
}

impl DirectoryPathBinding {
    /// Resolves an existing real directory to an absolute canonical path,
    /// opens that directory without following its final component, and
    /// retains its filesystem identity.
    pub(crate) fn open_canonical(path: &Path, operation: &'static str) -> Result<Self, StoreError> {
        validate_existing_directory_path(path, operation)?;
        let binding = Self::open_canonical_resolving(path, operation)?;
        validate_existing_directory_path(path, operation)?;
        binding.verify(operation)?;
        Ok(binding)
    }

    /// Resolves an existing directory path, including an intentional
    /// symlink in the supplied final component, then retains the real
    /// canonical directory identity.
    ///
    /// This is used only for a caller-selected backup parent. Source
    /// namespaces and newly created backup directories use
    /// [`Self::open_canonical`] and reject final-component links.
    pub(crate) fn open_canonical_resolving(
        path: &Path,
        operation: &'static str,
    ) -> Result<Self, StoreError> {
        let canonical_path = std::fs::canonicalize(path).map_err(|source| StoreError::Io {
            operation,
            path: path.to_path_buf(),
            source,
        })?;
        let verified_path = std::fs::canonicalize(path).map_err(|source| StoreError::Io {
            operation,
            path: path.to_path_buf(),
            source,
        })?;
        if verified_path != canonical_path {
            return Err(StoreError::UnsafeFilesystemObject {
                path: path.to_path_buf(),
                reason: "the directory path changed while it was being resolved",
            });
        }

        let handle = open_directory_handle(&canonical_path, operation)?;
        let identity = file_identity(&handle, &canonical_path, operation)?;
        let binding = Self {
            path: canonical_path,
            handle,
            identity,
        };
        binding.verify(operation)?;
        Ok(binding)
    }

    /// The absolute canonical path whose binding is retained.
    #[must_use]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Verifies that the retained canonical path still names the originally
    /// opened directory.
    pub(crate) fn verify(&self, operation: &'static str) -> Result<(), StoreError> {
        let current = open_directory_handle(&self.path, operation)?;
        let current_identity = file_identity(&current, &self.path, operation)?;
        if current_identity != self.identity {
            return Err(StoreError::UnsafeFilesystemObject {
                path: self.path.clone(),
                reason: "the directory path no longer names the originally opened directory",
            });
        }
        Ok(())
    }

    /// Syncs the retained directory handle itself.
    ///
    /// On Windows this preserves the existing documented limitation: stable
    /// directory handles can be retained and verified, but `std::fs` exposes
    /// no supported directory-sync operation, so this is a no-op there.
    pub(crate) fn sync(&self, operation: &'static str) -> Result<(), StoreError> {
        #[cfg(unix)]
        {
            self.handle.sync_all().map_err(|source| StoreError::Io {
                operation,
                path: self.path.clone(),
                source,
            })
        }
        #[cfg(not(unix))]
        {
            let _ = operation;
            Ok(())
        }
    }
}

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
pub(crate) fn create_namespace_dir_cancellable(
    dir: &Path,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<Option<()>, StoreError> {
    let mut missing_directories = Vec::new();
    let mut candidate = dir;
    let existing_boundary = loop {
        if cancelled() {
            return Ok(None);
        }
        let metadata = std::fs::symlink_metadata(candidate);
        if cancelled() {
            return Ok(None);
        }
        match metadata {
            Ok(_) => break candidate.to_path_buf(),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                missing_directories.push(candidate.to_path_buf());
            }
            #[cfg(unix)]
            Err(source) if source.raw_os_error() == Some(libc::ENAMETOOLONG) => {
                // Defer this failure until the nearest existing ancestor can
                // report its actual per-component limit via fpathconf.
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
    };

    let namespace_presecured = missing_directories.is_empty();
    if !namespace_presecured {
        validate_missing_namespace_component_lengths(&existing_boundary, &missing_directories)?;
    }
    if namespace_presecured {
        let Some(()) = secure_namespace_dir_cancellable(dir, &mut *cancelled)? else {
            return Ok(None);
        };
    }
    let sync_existing_boundary = if namespace_presecured {
        if cancelled() {
            return Ok(None);
        }
        let entries = std::fs::read_dir(dir);
        if cancelled() {
            return Ok(None);
        }
        match entries {
            Ok(mut entries) => {
                let first = entries.next();
                if cancelled() {
                    return Ok(None);
                }
                match first {
                    None => true,
                    Some(Ok(_)) => false,
                    Some(Err(source)) => {
                        return Err(StoreError::Io {
                            operation: "inspect an existing checkpoint namespace entry",
                            path: dir.to_path_buf(),
                            source,
                        });
                    }
                }
            }
            Err(source) => {
                return Err(StoreError::Io {
                    operation: "inspect an existing checkpoint namespace",
                    path: dir.to_path_buf(),
                    source,
                });
            }
        }
    } else {
        true
    };

    // A prior interrupted attempt may have created this empty boundary but
    // failed while syncing its parent. Retrying that parent sync before
    // creating a child makes directory creation resumably durable.
    if sync_existing_boundary && let Some(parent) = existing_boundary.parent() {
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        let Some(()) = sync_directory_cancellable(parent, &mut *cancelled)? else {
            return Ok(None);
        };
    }

    #[allow(unused_mut)]
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        let _ = builder.mode(NAMESPACE_DIR_MODE);
    }
    for missing in missing_directories.iter().rev() {
        if cancelled() {
            return Ok(None);
        }
        let created = builder.create(missing);
        if let Err(source) = created
            && source.kind() != std::io::ErrorKind::AlreadyExists
        {
            if cancelled() {
                return Ok(None);
            }
            return Err(StoreError::Io {
                operation: "create the checkpoint namespace directory",
                path: missing.clone(),
                source,
            });
        }
        // Once mkdir starts, finish its parent durability boundary even if
        // cancellation arrives during the call. A retry cannot otherwise
        // distinguish this directory from one that was durable already.
        if let Some(parent) = missing.parent() {
            sync_directory(if parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            })?;
        }
        if cancelled() {
            return Ok(None);
        }
    }
    if !namespace_presecured {
        let Some(()) = secure_namespace_dir_cancellable(dir, &mut *cancelled)? else {
            return Ok(None);
        };
    }
    Ok(Some(()))
}

#[cfg(unix)]
#[allow(unsafe_code, reason = "POSIX fpathconf requires a raw file descriptor")]
fn validate_missing_namespace_component_lengths(
    existing_boundary: &Path,
    missing_directories: &[PathBuf],
) -> Result<(), StoreError> {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let mut options = OpenOptions::new();
    let _ = options
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC);
    let boundary = options
        .open(existing_boundary)
        .map_err(|source| StoreError::Io {
            operation: "open a checkpoint namespace ancestor for component-limit validation",
            path: existing_boundary.to_path_buf(),
            source,
        })?;
    // SAFETY: `boundary` owns a live file descriptor for the duration of
    // this call, and `_PC_NAME_MAX` does not write through any pointer.
    let reported = unsafe { libc::fpathconf(boundary.as_raw_fd(), libc::_PC_NAME_MAX) };
    if reported <= 0 {
        // POSIX permits -1 for an indeterminate/unbounded value. Namespace
        // creation still surfaces any concrete filesystem error.
        return Ok(());
    }
    let max = usize::try_from(reported).map_err(|_| StoreError::Io {
        operation: "interpret a checkpoint filesystem component limit",
        path: existing_boundary.to_path_buf(),
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "filesystem component limit does not fit usize",
        ),
    })?;
    for path in missing_directories {
        let Some(component) = path.file_name() else {
            continue;
        };
        let len = component.as_bytes().len();
        if len > max {
            return Err(StoreError::NamespaceComponentTooLong {
                path: path.clone(),
                len,
                max,
            });
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_missing_namespace_component_lengths(
    _existing_boundary: &Path,
    _missing_directories: &[PathBuf],
) -> Result<(), StoreError> {
    Ok(())
}

fn secure_namespace_dir_cancellable(
    dir: &Path,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<Option<()>, StoreError> {
    let Some(()) = validate_namespace_dir_cancellable(dir, &mut *cancelled)? else {
        return Ok(None);
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if cancelled() {
            return Ok(None);
        }
        let permissions =
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(NAMESPACE_DIR_MODE));
        if cancelled() {
            return Ok(None);
        }
        permissions.map_err(|source| StoreError::Io {
            operation: "set private checkpoint namespace permissions",
            path: dir.to_path_buf(),
            source,
        })?;
    }
    Ok(Some(()))
}

/// Validates that an existing namespace is a real directory without
/// creating it or changing its metadata.
pub(crate) fn validate_existing_namespace_dir(dir: &Path) -> Result<(), StoreError> {
    validate_directory_path_cancellable(
        dir,
        "inspect the checkpoint namespace directory",
        &mut || false,
    )
    .map(|validated| validated.expect("non-cancellable directory validation cannot cancel"))
}

fn validate_namespace_dir_cancellable(
    dir: &Path,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<Option<()>, StoreError> {
    validate_directory_path_cancellable(
        dir,
        "inspect the checkpoint namespace directory",
        cancelled,
    )
}

fn validate_existing_directory_path(dir: &Path, operation: &'static str) -> Result<(), StoreError> {
    validate_directory_path_cancellable(dir, operation, &mut || false)
        .map(|validated| validated.expect("non-cancellable directory validation cannot cancel"))
}

fn validate_directory_path_cancellable(
    dir: &Path,
    operation: &'static str,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<Option<()>, StoreError> {
    if cancelled() {
        return Ok(None);
    }
    let metadata = std::fs::symlink_metadata(dir);
    if cancelled() {
        return Ok(None);
    }
    let metadata = metadata.map_err(|source| StoreError::Io {
        operation,
        path: dir.to_path_buf(),
        source,
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(StoreError::UnsafeFilesystemObject {
            path: dir.to_path_buf(),
            reason: "checkpoint directories must be real directories, not symlinks",
        });
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(StoreError::UnsafeFilesystemObject {
                path: dir.to_path_buf(),
                reason: "checkpoint directories must not be Windows reparse points",
            });
        }
    }
    Ok(Some(()))
}

fn open_directory_handle(path: &Path, operation: &'static str) -> Result<File, StoreError> {
    let mut options = OpenOptions::new();
    let _ = options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;

        let _ = options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        };

        let _ = options.custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let handle = options.open(path).map_err(|source| StoreError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    })?;
    let metadata = handle.metadata().map_err(|source| StoreError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(StoreError::UnsafeFilesystemObject {
            path: path.to_path_buf(),
            reason: "checkpoint directory handles must refer to real directories",
        });
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

        let information = windows_file_information(&handle, path, operation)?;
        if information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(StoreError::UnsafeFilesystemObject {
                path: path.to_path_buf(),
                reason: "checkpoint directory handles must not refer to Windows reparse points",
            });
        }
    }
    Ok(handle)
}

#[cfg(unix)]
fn file_identity(
    file: &File,
    path: &Path,
    operation: &'static str,
) -> Result<FileIdentity, StoreError> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = file.metadata().map_err(|source| StoreError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    })?;
    Ok(FileIdentity::Unix {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(windows)]
fn file_identity(
    file: &File,
    path: &Path,
    operation: &'static str,
) -> Result<FileIdentity, StoreError> {
    let information = windows_file_id_information(file, path, operation)?;
    Ok(FileIdentity::Windows {
        volume_serial: information.VolumeSerialNumber,
        file_id: information.FileId.Identifier,
    })
}

#[cfg(not(any(unix, windows)))]
fn file_identity(
    _file: &File,
    path: &Path,
    operation: &'static str,
) -> Result<FileIdentity, StoreError> {
    Err(StoreError::Io {
        operation,
        path: path.to_path_buf(),
        source: std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "stable filesystem identity is unsupported on this platform",
        ),
    })
}

#[cfg(windows)]
#[allow(
    unsafe_code,
    reason = "GetFileInformationByHandle requires a raw Windows handle"
)]
fn windows_file_information(
    file: &File,
    path: &Path,
    operation: &'static str,
) -> Result<windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION, StoreError> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle as _;

    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let mut information = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // SAFETY: `file` owns a live Windows handle and `information` is valid
    // writable storage for the duration of this synchronous call.
    let succeeded =
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr()) };
    if succeeded == 0 {
        return Err(StoreError::Io {
            operation,
            path: path.to_path_buf(),
            source: std::io::Error::last_os_error(),
        });
    }
    // SAFETY: a nonzero return guarantees the output was initialized.
    Ok(unsafe { information.assume_init() })
}

#[cfg(windows)]
#[allow(
    unsafe_code,
    reason = "GetFileInformationByHandleEx requires a raw Windows handle"
)]
fn windows_file_id_information(
    file: &File,
    path: &Path,
    operation: &'static str,
) -> Result<windows_sys::Win32::Storage::FileSystem::FILE_ID_INFO, StoreError> {
    use std::mem::{MaybeUninit, size_of};
    use std::os::windows::io::AsRawHandle as _;

    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx,
    };

    let mut information = MaybeUninit::<FILE_ID_INFO>::zeroed();
    // SAFETY: `file` owns a live Windows handle and `information` is valid
    // writable storage for exactly the structure initialized by this call.
    let succeeded = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            information.as_mut_ptr().cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if succeeded == 0 {
        return Err(StoreError::Io {
            operation,
            path: path.to_path_buf(),
            source: std::io::Error::last_os_error(),
        });
    }
    // SAFETY: a nonzero return guarantees the output was initialized.
    Ok(unsafe { information.assume_init() })
}

/// Verifies that `path` still names the same regular checkpoint file as the
/// retained open handle.
pub(crate) fn verify_checkpoint_file_path_binding(
    file: &File,
    path: &Path,
    operation: &'static str,
) -> Result<(), StoreError> {
    let expected_identity = file_identity(file, path, operation)?;
    let mut options = OpenOptions::new();
    let _ = options.read(true);
    no_follow(&mut options);
    let current = options.open(path).map_err(|source| StoreError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    })?;
    validate_checkpoint_file_cancellable(&current, path, operation, &mut || false)?
        .expect("non-cancellable file validation cannot cancel");
    let current_identity = file_identity(&current, path, operation)?;
    if current_identity != expected_identity {
        return Err(StoreError::UnsafeFilesystemObject {
            path: path.to_path_buf(),
            reason: "the file path no longer names the originally opened checkpoint file",
        });
    }
    Ok(())
}

/// Applies platform flags that open the named object itself without blocking
/// on a special file before handle-based validation can reject it.
fn no_follow(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        let _ = options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
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
    secure_checkpoint_file_cancellable(file, path, operation, &mut || false)
        .map(|secured| secured.expect("non-cancellable file validation cannot be cancelled"))
}

pub(super) fn secure_checkpoint_file_cancellable(
    file: &File,
    path: &Path,
    operation: &'static str,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<Option<()>, StoreError> {
    let Some(()) = validate_checkpoint_file_cancellable(file, path, operation, &mut *cancelled)?
    else {
        return Ok(None);
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if cancelled() {
            return Ok(None);
        }
        let permissions =
            file.set_permissions(std::fs::Permissions::from_mode(CHECKPOINT_FILE_MODE));
        if cancelled() {
            return Ok(None);
        }
        permissions.map_err(|source| StoreError::Io {
            operation: "set private checkpoint file permissions",
            path: path.to_path_buf(),
            source,
        })?;
    }
    Ok(Some(()))
}

/// Validates an opened checkpoint file without changing its contents or
/// metadata.
pub(super) fn validate_checkpoint_file_cancellable(
    file: &File,
    path: &Path,
    operation: &'static str,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<Option<()>, StoreError> {
    if cancelled() {
        return Ok(None);
    }
    let metadata = file.metadata();
    if cancelled() {
        return Ok(None);
    }
    let metadata = metadata.map_err(|source| StoreError::Io {
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
        if cancelled() {
            return Ok(None);
        }
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
        use std::os::unix::fs::MetadataExt as _;
        if metadata.nlink() != 1 {
            return Err(StoreError::UnsafeFilesystemObject {
                path: path.to_path_buf(),
                reason: "checkpoint artifacts must not have multiple hard links",
            });
        }
    }
    Ok(Some(()))
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
    let backup_path = dir.join(backup_file_name(final_name));
    let final_path = dir.join(final_name);

    plan.check(faults.before_write)
        .map_err(AtomicWriteError::staged)?;

    // A previous staged write may have failed before publication. Removing
    // the known temporary name is safe: unlinking a symlink removes the link
    // itself rather than its target, and create_new below prevents a race
    // from turning into truncation of an injected object.
    let _removed = remove_file_if_present(&temp_path).map_err(AtomicWriteError::staged)?;
    // Windows replacement uses one deterministic backup name so even error
    // 1177 cannot create an unbounded, unknowable orphan. A usable store has
    // already resolved marker authority, and generation backups are never
    // authoritative, so a leftover is safe to remove before a new attempt.
    let _removed_backup = remove_file_if_present(&backup_path).map_err(AtomicWriteError::staged)?;
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

    install_temp_file(&temp_path, &final_path, &backup_path)?;

    plan.check(faults.after_publish)
        .map_err(AtomicWriteError::published)
}

/// Syncs and installs an already-written same-directory temporary file.
///
/// Recovery uses this only after bounded validation establishes that
/// `CURRENT.tmp`, together with any matching `CURRENT.bak`, is valid marker
/// evidence for a complete generation.
/// Syncing again also completes recovery from an interruption after the
/// temporary marker was written but before its original sync boundary.
pub(crate) fn publish_existing_temp_file(
    dir: &Path,
    final_name: &str,
) -> Result<(), AtomicWriteError> {
    let temp_path = dir.join(temp_file_name(final_name));
    let backup_path = dir.join(backup_file_name(final_name));
    let final_path = dir.join(final_name);

    let mut options = OpenOptions::new();
    let _ = options.read(true).write(true);
    no_follow(&mut options);
    let file = options.open(&temp_path).map_err(|source| {
        AtomicWriteError::staged(StoreError::Io {
            operation: "open a validated checkpoint temporary file for publication",
            path: temp_path.clone(),
            source,
        })
    })?;
    secure_checkpoint_file(
        &file,
        &temp_path,
        "validate a checkpoint temporary file before publication",
    )
    .map_err(AtomicWriteError::staged)?;
    file.sync_all().map_err(|source| {
        AtomicWriteError::staged(StoreError::Io {
            operation: "sync a checkpoint temporary file before publication",
            path: temp_path.clone(),
            source,
        })
    })?;
    drop(file);

    install_temp_file(&temp_path, &final_path, &backup_path)
}

fn install_temp_file(
    temp_path: &Path,
    final_path: &Path,
    backup_path: &Path,
) -> Result<(), AtomicWriteError> {
    replace_file(temp_path, final_path, backup_path).map_err(|failure| {
        let error = StoreError::Io {
            operation: "atomically install a checkpoint temporary file",
            path: final_path.to_path_buf(),
            source: failure.source,
        };
        if failure.destination_may_have_changed {
            AtomicWriteError::published(error)
        } else {
            AtomicWriteError::staged(error)
        }
    })?;
    remove_file_if_present(backup_path)
        .map_err(AtomicWriteError::published)
        .map(|_| ())
}

/// Installs `temp_path` at `final_path` atomically on the current platform.
#[cfg(not(windows))]
fn replace_file(
    temp_path: &Path,
    final_path: &Path,
    _backup_path: &Path,
) -> Result<(), ReplaceFileError> {
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
    use windows_sys::Win32::Foundation::ERROR_UNABLE_TO_MOVE_REPLACEMENT_2;

    raw_os_error
        .and_then(|code| u32::try_from(code).ok())
        .is_some_and(|code| code == ERROR_UNABLE_TO_MOVE_REPLACEMENT_2)
}

#[cfg(windows)]
fn replace_file(
    temp_path: &Path,
    final_path: &Path,
    backup_path: &Path,
) -> Result<(), ReplaceFileError> {
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
    let backup = wide(backup_path).map_err(|source| ReplaceFileError {
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
    // call. The exclusion pointers are null. The deterministic backup name
    // bounds the otherwise unspecified renamed-original postcondition of
    // error 1177. The temporary file handle was closed after sync_all, and
    // this store never keeps a handle to CURRENT open while publishing it.
    let succeeded = unsafe {
        if destination_exists {
            ReplaceFileW(
                final_path_wide.as_ptr(),
                temp.as_ptr(),
                backup.as_ptr(),
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
    sync_directory_cancellable(dir, &mut || false)
        .map(|synced| synced.expect("non-cancellable directory sync cannot be cancelled"))
}

#[cfg(unix)]
pub(crate) fn sync_directory_cancellable(
    dir: &Path,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<Option<()>, StoreError> {
    if cancelled() {
        return Ok(None);
    }
    let handle = File::open(dir);
    if cancelled() {
        return Ok(None);
    }
    let handle = handle.map_err(|source| StoreError::Io {
        operation: "open the checkpoint namespace directory for sync",
        path: dir.to_path_buf(),
        source,
    })?;
    let synced = handle.sync_all();
    if cancelled() {
        return Ok(None);
    }
    synced.map_err(|source| StoreError::Io {
        operation: "sync the checkpoint namespace directory",
        path: dir.to_path_buf(),
        source,
    })?;
    Ok(Some(()))
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

#[cfg(not(unix))]
pub(crate) fn sync_directory_cancellable(
    _dir: &Path,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<Option<()>, StoreError> {
    if cancelled() { Ok(None) } else { Ok(Some(())) }
}

/// Reads `path` in full while allowing lifecycle cancellation between every
/// filesystem operation and buffered read. The outer `Option` reports
/// cancellation; the inner `Option` reports an artifact that does not exist.
pub(crate) fn read_file_bounded_cancellable(
    path: &Path,
    artifact: &'static str,
    max: u64,
    cancelled: impl FnMut() -> bool,
) -> Result<Option<Option<Vec<u8>>>, StoreError> {
    read_file_bounded_cancellable_with_mode(
        path,
        artifact,
        max,
        ArtifactReadMode::RepairPermissions,
        cancelled,
    )
}

/// Reads an optional existing checkpoint artifact without creating it,
/// repairing permissions, or changing its contents.
pub(crate) fn read_file_bounded_read_only(
    path: &Path,
    artifact: &'static str,
    max: u64,
) -> Result<Option<Vec<u8>>, StoreError> {
    read_file_bounded_cancellable_with_mode(
        path,
        artifact,
        max,
        ArtifactReadMode::PreserveMetadata,
        || false,
    )
    .map(|result| result.expect("non-cancellable checkpoint read cannot be cancelled"))
}

pub(crate) fn read_file_bounded_cancellable_with_mode(
    path: &Path,
    artifact: &'static str,
    max: u64,
    mode: ArtifactReadMode,
    mut cancelled: impl FnMut() -> bool,
) -> Result<Option<Option<Vec<u8>>>, StoreError> {
    if cancelled() {
        return Ok(None);
    }
    let mut options = OpenOptions::new();
    let _ = options.read(true);
    no_follow(&mut options);
    let opened = options.open(path);
    if cancelled() {
        return Ok(None);
    }
    let file = match opened {
        Ok(file) => file,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Some(None)),
        Err(source) => {
            return Err(StoreError::Io {
                operation: "open a checkpoint file",
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let validated = match mode {
        ArtifactReadMode::RepairPermissions => secure_checkpoint_file_cancellable(
            &file,
            path,
            "validate a checkpoint file",
            &mut cancelled,
        )?,
        ArtifactReadMode::PreserveMetadata => validate_checkpoint_file_cancellable(
            &file,
            path,
            "validate a checkpoint file",
            &mut cancelled,
        )?,
    };
    let Some(()) = validated else {
        return Ok(None);
    };
    let metadata = file.metadata();
    if cancelled() {
        return Ok(None);
    }
    let metadata = metadata.map_err(|source| StoreError::Io {
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
    let Some(buffer) =
        read_bounded_contents_cancellable(file, capacity, path, artifact, max, &mut cancelled)?
    else {
        return Ok(None);
    };
    Ok(Some(Some(buffer)))
}

/// Reads at most `max + 1` bytes so growth after metadata inspection is
/// detected rather than decoded as a silently truncated artifact.
pub(super) fn read_bounded_contents(
    file: File,
    capacity: usize,
    path: &Path,
    artifact: &'static str,
    max: u64,
) -> Result<Vec<u8>, StoreError> {
    read_bounded_contents_cancellable(file, capacity, path, artifact, max, &mut || false)
        .map(|buffer| buffer.expect("non-cancellable checkpoint read cannot be cancelled"))
}

pub(super) fn read_bounded_contents_cancellable(
    mut file: File,
    capacity: usize,
    path: &Path,
    artifact: &'static str,
    max: u64,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<Option<Vec<u8>>, StoreError> {
    if cancelled() {
        return Ok(None);
    }
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
        if cancelled() {
            return Ok(None);
        }
        let remaining = max_capacity.saturating_sub(buffer.len());
        let read_capacity = remaining.saturating_add(1).min(chunk.len());
        let read = file.read(&mut chunk[..read_capacity]);
        if cancelled() {
            return Ok(None);
        }
        let read = read.map_err(|source| StoreError::Io {
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
        if cancelled() {
            return Ok(None);
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
    Ok(Some(buffer))
}

/// Truncates `path` to `len` bytes and syncs it, discarding a structurally
/// incomplete trailing WAL region so subsequent appends continue from the
/// last complete transaction.
pub(crate) fn truncate_file_cancellable(
    path: &Path,
    len: u64,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<Option<()>, StoreError> {
    if cancelled() {
        return Ok(None);
    }
    let mut options = OpenOptions::new();
    let _ = options.write(true);
    no_follow(&mut options);
    let file = options.open(path);
    if cancelled() {
        return Ok(None);
    }
    let file = file.map_err(|source| StoreError::Io {
        operation: "open a checkpoint WAL to discard its torn tail",
        path: path.to_path_buf(),
        source,
    })?;
    let Some(()) = secure_checkpoint_file_cancellable(
        &file,
        path,
        "validate a checkpoint WAL before truncating it",
        &mut *cancelled,
    )?
    else {
        return Ok(None);
    };
    let truncated = file.set_len(len);
    if cancelled() {
        return Ok(None);
    }
    truncated.map_err(|source| StoreError::Io {
        operation: "truncate a checkpoint WAL to its last complete transaction",
        path: path.to_path_buf(),
        source,
    })?;
    let synced = file.sync_all();
    if cancelled() {
        return Ok(None);
    }
    synced.map_err(|source| StoreError::Io {
        operation: "sync a truncated checkpoint WAL",
        path: path.to_path_buf(),
        source,
    })?;
    Ok(Some(()))
}

/// Opens an existing WAL for appending. Every write lands at the current end
/// of file, so a concurrent reader always sees whole prefixes.
pub(crate) fn open_for_append_cancellable(
    path: &Path,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<Option<File>, StoreError> {
    if cancelled() {
        return Ok(None);
    }
    let mut options = OpenOptions::new();
    let _ = options.append(true);
    no_follow(&mut options);
    let file = options.open(path);
    if cancelled() {
        return Ok(None);
    }
    let file = file.map_err(|source| StoreError::Io {
        operation: "open the checkpoint WAL for appending",
        path: path.to_path_buf(),
        source,
    })?;
    let Some(()) = secure_checkpoint_file_cancellable(
        &file,
        path,
        "validate a checkpoint WAL before appending",
        &mut *cancelled,
    )?
    else {
        return Ok(None);
    };
    Ok(Some(file))
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
