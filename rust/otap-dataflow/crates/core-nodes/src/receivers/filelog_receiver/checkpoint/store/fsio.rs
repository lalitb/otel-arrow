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
//! - **Atomic publication.** A file is written to a role-specific temporary
//!   name in the same directory, synced, and atomically installed. Generation
//!   names use exclusive no-replace installation; later `CURRENT` publication
//!   replaces only the prior marker.
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

use super::super::namespace::{CHECKPOINT_NAMESPACE_VERSION, FILELOG_NAMESPACE_DIRECTORY};
use super::error::StoreError;
use super::fault::{FaultPlan, FaultPoint};
use super::layout::{PublicationRole, temp_file_name};

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
    #[allow(
        dead_code,
        reason = "the retained open handle pins the verified directory for the binding lifetime"
    )]
    handle: File,
    identity: FileIdentity,
}

/// Namespace path and all versioned ancestors retained through publication.
#[derive(Debug)]
pub(crate) struct PreparedNamespace {
    state_dir: Option<DirectoryPathBinding>,
    filelog_dir: Option<DirectoryPathBinding>,
    version_dir: Option<DirectoryPathBinding>,
    namespace_dir: DirectoryPathBinding,
}

impl PreparedNamespace {
    fn direct(namespace_dir: DirectoryPathBinding) -> Self {
        Self {
            state_dir: None,
            filelog_dir: None,
            version_dir: None,
            namespace_dir,
        }
    }

    /// Revalidates every retained ancestor and the namespace itself.
    pub(crate) fn verify(&self, operation: &'static str) -> Result<(), StoreError> {
        for binding in [
            self.state_dir.as_ref(),
            self.filelog_dir.as_ref(),
            self.version_dir.as_ref(),
            Some(&self.namespace_dir),
        ]
        .into_iter()
        .flatten()
        {
            binding.verify(operation)?;
        }
        Ok(())
    }

    /// Canonical namespace path retained by this preparation.
    pub(crate) fn namespace_path(&self) -> &Path {
        self.namespace_dir.path()
    }

    /// Retained namespace-directory binding used after publication.
    pub(crate) fn into_namespace(self) -> DirectoryPathBinding {
        self.namespace_dir
    }
}

/// One exact checkpoint artifact path retained with its open file identity.
#[derive(Debug)]
pub(crate) struct CheckpointFilePathBinding {
    path: PathBuf,
    #[allow(
        dead_code,
        reason = "the retained open handle pins the verified checkpoint artifact for the binding lifetime"
    )]
    handle: File,
    identity: FileIdentity,
}

impl CheckpointFilePathBinding {
    /// Opens and validates one existing regular checkpoint artifact.
    pub(crate) fn open(path: &Path, operation: &'static str) -> Result<Self, StoreError> {
        let mut options = OpenOptions::new();
        let _ = options.read(true);
        no_follow(&mut options);
        let handle = options.open(path).map_err(|source| StoreError::Io {
            operation,
            path: path.to_path_buf(),
            source,
        })?;
        validate_checkpoint_file_cancellable(&handle, path, operation, &mut || false)?
            .expect("non-cancellable checkpoint file validation cannot cancel");
        let identity = file_identity(&handle, path, operation)?;
        let binding = Self {
            path: path.to_path_buf(),
            handle,
            identity,
        };
        binding.verify(operation)?;
        Ok(binding)
    }

    /// Verifies that the exact pathname still resolves to the retained file.
    pub(crate) fn verify(&self, operation: &'static str) -> Result<(), StoreError> {
        let mut options = OpenOptions::new();
        let _ = options.read(true);
        no_follow(&mut options);
        let current = options.open(&self.path).map_err(|source| StoreError::Io {
            operation,
            path: self.path.clone(),
            source,
        })?;
        validate_checkpoint_file_cancellable(&current, &self.path, operation, &mut || false)?
            .expect("non-cancellable checkpoint file validation cannot cancel");
        let current_identity = file_identity(&current, &self.path, operation)?;
        if current_identity != self.identity {
            return Err(StoreError::UnsafeFilesystemObject {
                path: self.path.clone(),
                reason: "the artifact path no longer names the retained checkpoint file",
            });
        }
        Ok(())
    }

    /// Removes the exact retained artifact after one final path verification.
    pub(crate) fn remove(self, operation: &'static str) -> Result<(), StoreError> {
        self.verify(operation)?;
        std::fs::remove_file(&self.path).map_err(|source| StoreError::Io {
            operation,
            path: self.path,
            source,
        })
    }
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

/// Whether installing a synced temporary may replace its destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AtomicInstallMode {
    /// Generation artifacts and the initial marker must not replace anything.
    NoReplace,
    /// Later `CURRENT` publication atomically replaces the prior marker.
    Replace,
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

/// Prepares the checkpoint namespace directory with restrictive permissions.
///
/// A versioned `filelog/@v1/<id>` path requires an already-existing engine
/// state root and unconditionally syncs each immediate parent after opening or
/// creating its child. Direct low-level test paths retain bounded recursive
/// creation behavior.
pub(crate) fn create_namespace_dir_cancellable(
    dir: &Path,
    faults: &mut FaultPlan,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<Option<PreparedNamespace>, StoreError> {
    if let Some((state_dir, filelog_dir, version_dir)) = versioned_namespace_parents(dir) {
        let state_dir = if state_dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            state_dir
        };
        return create_versioned_namespace_cancellable(
            state_dir,
            filelog_dir,
            version_dir,
            dir,
            faults,
            cancelled,
        );
    }

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
    let namespace =
        DirectoryPathBinding::open_canonical(dir, "bind the prepared checkpoint namespace")?;
    Ok(Some(PreparedNamespace::direct(namespace)))
}

fn versioned_namespace_parents(dir: &Path) -> Option<(&Path, &Path, &Path)> {
    let version_dir = dir.parent()?;
    if version_dir.file_name()? != CHECKPOINT_NAMESPACE_VERSION {
        return None;
    }
    let filelog_dir = version_dir.parent()?;
    if filelog_dir.file_name()? != FILELOG_NAMESPACE_DIRECTORY {
        return None;
    }
    let state_dir = filelog_dir.parent()?;
    Some((state_dir, filelog_dir, version_dir))
}

fn create_versioned_namespace_cancellable(
    state_dir: &Path,
    filelog_dir: &Path,
    version_dir: &Path,
    namespace_dir: &Path,
    faults: &mut FaultPlan,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<Option<PreparedNamespace>, StoreError> {
    if cancelled() {
        return Ok(None);
    }
    let state =
        DirectoryPathBinding::open_canonical(state_dir, "bind the durable engine state directory")?;
    let filelog = ensure_private_child_and_sync_parent(
        &state,
        filelog_dir,
        FaultPoint::BeforeFilelogParentSync,
        FaultPoint::AfterFilelogParentSync,
        faults,
    )?;
    if cancelled() {
        return Ok(None);
    }
    let version = ensure_private_child_and_sync_parent(
        &filelog,
        version_dir,
        FaultPoint::BeforeVersionParentSync,
        FaultPoint::AfterVersionParentSync,
        faults,
    )?;
    if cancelled() {
        return Ok(None);
    }
    let namespace = ensure_private_child_and_sync_parent(
        &version,
        namespace_dir,
        FaultPoint::BeforeNamespaceParentSync,
        FaultPoint::AfterNamespaceParentSync,
        faults,
    )?;
    if cancelled() {
        return Ok(None);
    }
    let prepared = PreparedNamespace {
        state_dir: Some(state),
        filelog_dir: Some(filelog),
        version_dir: Some(version),
        namespace_dir: namespace,
    };
    prepared.verify("revalidate the complete checkpoint namespace chain")?;
    Ok(Some(prepared))
}

fn ensure_private_child_and_sync_parent(
    parent: &DirectoryPathBinding,
    child: &Path,
    before_sync: FaultPoint,
    after_sync: FaultPoint,
    faults: &mut FaultPlan,
) -> Result<DirectoryPathBinding, StoreError> {
    parent.verify("revalidate a checkpoint namespace parent")?;
    let child_name = child.file_name().ok_or_else(|| StoreError::Io {
        operation: "derive a checkpoint namespace child name",
        path: child.to_path_buf(),
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "checkpoint namespace child has no final component",
        ),
    })?;
    let child = parent.path().join(child_name);
    #[allow(unused_mut)]
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        let _ = builder.mode(NAMESPACE_DIR_MODE);
    }
    if let Err(source) = builder.create(&child)
        && source.kind() != std::io::ErrorKind::AlreadyExists
    {
        return Err(StoreError::Io {
            operation: "create a checkpoint namespace component",
            path: child.clone(),
            source,
        });
    }
    secure_namespace_dir_cancellable(&child, &mut || false)?
        .expect("non-cancellable namespace component validation cannot cancel");
    let child_binding =
        DirectoryPathBinding::open_canonical(&child, "bind a checkpoint namespace component")?;
    faults.check(before_sync)?;
    parent.sync("sync a checkpoint namespace parent")?;
    faults.check(after_sync)?;
    parent.verify("revalidate a checkpoint namespace parent")?;
    child_binding.verify("revalidate a checkpoint namespace component")?;
    Ok(child_binding)
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

/// Verifies that two already validated handles name the same checkpoint file.
pub(crate) fn verify_same_checkpoint_file(
    expected: &File,
    actual: &File,
    path: &Path,
    operation: &'static str,
) -> Result<(), StoreError> {
    let expected_identity = file_identity(expected, path, operation)?;
    let actual_identity = file_identity(actual, path, operation)?;
    if actual_identity != expected_identity {
        return Err(StoreError::UnsafeFilesystemObject {
            path: path.to_path_buf(),
            reason: "the reopened handle is not the validated checkpoint file",
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
    role: PublicationRole,
    install_mode: AtomicInstallMode,
    plan: &mut FaultPlan,
    faults: AtomicWriteFaults,
) -> Result<(), AtomicWriteError> {
    let temp_path = dir.join(temp_file_name(final_name, role));
    let final_path = dir.join(final_name);

    plan.check(faults.before_write)
        .map_err(AtomicWriteError::staged)?;

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

    install_temp_file(&temp_path, &final_path, install_mode)?;

    plan.check(faults.after_publish)
        .map_err(AtomicWriteError::published)
}

fn install_temp_file(
    temp_path: &Path,
    final_path: &Path,
    install_mode: AtomicInstallMode,
) -> Result<(), AtomicWriteError> {
    install_file(temp_path, final_path, install_mode).map_err(|failure| {
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
    })
}

/// Installs `temp_path` at `final_path` atomically on the current platform.
#[cfg(not(windows))]
fn install_file(
    temp_path: &Path,
    final_path: &Path,
    install_mode: AtomicInstallMode,
) -> Result<(), ReplaceFileError> {
    match install_mode {
        AtomicInstallMode::Replace => {
            std::fs::rename(temp_path, final_path).map_err(|source| ReplaceFileError {
                source,
                destination_may_have_changed: false,
            })
        }
        AtomicInstallMode::NoReplace => install_no_replace(temp_path, final_path),
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[allow(
    unsafe_code,
    reason = "renameat2 is the Linux atomic no-replace rename primitive"
)]
fn install_no_replace(temp_path: &Path, final_path: &Path) -> Result<(), ReplaceFileError> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let temp = CString::new(temp_path.as_os_str().as_bytes()).map_err(|_| ReplaceFileError {
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "checkpoint temporary path contains an embedded NUL",
        ),
        destination_may_have_changed: false,
    })?;
    let final_path =
        CString::new(final_path.as_os_str().as_bytes()).map_err(|_| ReplaceFileError {
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "checkpoint final path contains an embedded NUL",
            ),
            destination_may_have_changed: false,
        })?;
    // SAFETY: both C strings are NUL-terminated and alive for the call.
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            temp.as_ptr(),
            libc::AT_FDCWD,
            final_path.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(ReplaceFileError {
            source: std::io::Error::last_os_error(),
            destination_may_have_changed: false,
        })
    }
}

#[cfg(target_os = "macos")]
#[allow(
    unsafe_code,
    reason = "renamex_np is the macOS atomic no-replace rename primitive"
)]
fn install_no_replace(temp_path: &Path, final_path: &Path) -> Result<(), ReplaceFileError> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    unsafe extern "C" {
        fn renamex_np(
            from: *const libc::c_char,
            to: *const libc::c_char,
            flags: libc::c_uint,
        ) -> libc::c_int;
    }
    const RENAME_EXCL: libc::c_uint = 0x0000_0002;

    let temp = CString::new(temp_path.as_os_str().as_bytes()).map_err(|_| ReplaceFileError {
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "checkpoint temporary path contains an embedded NUL",
        ),
        destination_may_have_changed: false,
    })?;
    let final_path =
        CString::new(final_path.as_os_str().as_bytes()).map_err(|_| ReplaceFileError {
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "checkpoint final path contains an embedded NUL",
            ),
            destination_may_have_changed: false,
        })?;
    // SAFETY: both C strings are NUL-terminated and alive for the call.
    let result = unsafe { renamex_np(temp.as_ptr(), final_path.as_ptr(), RENAME_EXCL) };
    if result == 0 {
        Ok(())
    } else {
        Err(ReplaceFileError {
            source: std::io::Error::last_os_error(),
            destination_may_have_changed: false,
        })
    }
}

#[cfg(all(
    not(windows),
    not(any(target_os = "linux", target_os = "android", target_os = "macos"))
))]
fn install_no_replace(temp_path: &Path, final_path: &Path) -> Result<(), ReplaceFileError> {
    std::fs::hard_link(temp_path, final_path).map_err(|source| ReplaceFileError {
        source,
        destination_may_have_changed: false,
    })?;
    std::fs::remove_file(temp_path).map_err(|source| ReplaceFileError {
        source,
        destination_may_have_changed: true,
    })
}

/// Installs `temp_path` at `final_path` with Windows replacement semantics.
///
/// `std::fs::rename` cannot replace an existing destination on Windows.
/// `ReplaceFileW` is used only for later `CURRENT` replacement. Generation
/// artifacts and the initial marker use `MoveFileExW` with no replacement
/// flag and write-through semantics. Both handles created by this module are
/// closed before installation, satisfying `ReplaceFileW` sharing rules.
#[cfg(windows)]
pub(super) fn windows_replace_failure_may_have_changed(raw_os_error: Option<i32>) -> bool {
    use windows_sys::Win32::Foundation::{
        ERROR_UNABLE_TO_MOVE_REPLACEMENT, ERROR_UNABLE_TO_MOVE_REPLACEMENT_2,
        ERROR_UNABLE_TO_REMOVE_REPLACED,
    };

    raw_os_error
        .and_then(|code| u32::try_from(code).ok())
        .is_some_and(|code| {
            matches!(
                code,
                ERROR_UNABLE_TO_REMOVE_REPLACED
                    | ERROR_UNABLE_TO_MOVE_REPLACEMENT
                    | ERROR_UNABLE_TO_MOVE_REPLACEMENT_2
            )
        })
}

#[cfg(windows)]
fn install_file(
    temp_path: &Path,
    final_path: &Path,
    install_mode: AtomicInstallMode,
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

    if install_mode == AtomicInstallMode::NoReplace && destination_exists {
        return Err(ReplaceFileError {
            source: std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "checkpoint destination already exists",
            ),
            destination_may_have_changed: false,
        });
    }
    if install_mode == AtomicInstallMode::Replace && !destination_exists {
        return Err(ReplaceFileError {
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "checkpoint replacement destination does not exist",
            ),
            destination_may_have_changed: false,
        });
    }

    // SAFETY: both path buffers are NUL-terminated and remain alive for the
    // call. The exclusion pointers are null. The temporary file handle was
    // closed after sync_all.
    let succeeded = unsafe {
        if install_mode == AtomicInstallMode::Replace {
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
#[cfg(test)]
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
    let Some(file) = open_for_wal_repair_cancellable(path, &mut *cancelled)? else {
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

/// Opens an existing WAL with the write rights required for exact tail repair.
pub(crate) fn open_for_wal_repair_cancellable(
    path: &Path,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<Option<File>, StoreError> {
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
    Ok(Some(file))
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
