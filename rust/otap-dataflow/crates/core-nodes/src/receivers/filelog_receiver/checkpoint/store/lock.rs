// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Exclusive ownership of one checkpoint namespace.
//!
//! Appendix B (D15) requires the namespace lock to prevent two active
//! configurations that share a `checkpoint.id` from writing concurrently.
//! The lock is an advisory operating-system lock taken on a dedicated,
//! always-empty `ownership.lock` file:
//!
//! - POSIX: `flock(LOCK_EX | LOCK_NB)`. `flock` is associated with the open
//!   file description, not with the process, so a second store instance
//!   inside the *same* process is rejected exactly like one in another
//!   process. (`fcntl` byte-range locks would not be: they are per-process
//!   and would silently allow a second in-process writer.)
//! - Windows: `LockFileEx` with `LOCKFILE_EXCLUSIVE_LOCK |
//!   LOCKFILE_FAIL_IMMEDIATELY`.
//!
//! A small internal target-gated wrapper calls those operating-system APIs
//! directly. The lock is released when the file descriptor/handle is closed,
//! which happens when [`NamespaceLock`] is dropped, including on abnormal
//! process exit.
//!
//! Acquisition is bounded: it retries at a fixed interval until the
//! configured `checkpoint.ownership_timeout` elapses and then fails with
//! [`StoreError::NamespaceLocked`] rather than blocking the thread forever.
//! Blocking sleeps are acceptable here only because this type is used from
//! the dedicated read/checkpoint OS thread, never from async code.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::error::StoreError;
use super::fsio;
use super::layout::OWNERSHIP_LOCK_FILE_NAME;
use super::os_lock::{TryLockOutcome, try_lock_exclusive, unlock_exclusive};

/// Unix mode for the ownership lock file.
#[cfg(unix)]
const LOCK_FILE_MODE: u32 = 0o600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LockOpenMode {
    CreateOrOpen,
    ExistingOnly,
}

/// Exclusive ownership of a checkpoint namespace, held for the lifetime of
/// the value.
#[derive(Debug)]
pub struct NamespaceLock {
    /// Holding the open file keeps the advisory lock: closing the
    /// descriptor is what releases it.
    file: File,
    path: PathBuf,
    waited: Duration,
    contentions: u64,
}

impl NamespaceLock {
    /// Acquires exclusive ownership of the namespace rooted at `dir`,
    /// retrying every `retry_interval` until `timeout` elapses.
    ///
    /// The directory must already exist. A `timeout` of zero means a single
    /// attempt.
    pub fn acquire(
        dir: &Path,
        timeout: Duration,
        retry_interval: Duration,
    ) -> Result<Self, StoreError> {
        Self::acquire_cancellable(dir, timeout, retry_interval, || false)
            .map(|lock| lock.expect("non-cancellable namespace acquisition cannot be cancelled"))
    }

    /// Acquires exclusive ownership through an existing `ownership.lock`
    /// without creating the lock file or repairing its permissions.
    ///
    /// This is the administration path: the namespace and lock must already
    /// have been published by the runtime. A `timeout` of zero means a
    /// single lock attempt.
    pub fn acquire_existing(
        dir: &Path,
        timeout: Duration,
        retry_interval: Duration,
    ) -> Result<Self, StoreError> {
        Self::acquire_cancellable_with_mode(
            dir,
            timeout,
            retry_interval,
            LockOpenMode::ExistingOnly,
            || false,
        )
        .map(|lock| {
            lock.expect("non-cancellable existing namespace acquisition cannot be cancelled")
        })
    }

    /// Acquires exclusive namespace ownership, abandoning the wait when
    /// `cancelled` becomes true.
    ///
    /// Cancellation is checked around every potentially blocking filesystem
    /// operation and immediately before and after every lock attempt. A lock
    /// acquired concurrently with cancellation is dropped before returning.
    pub(crate) fn acquire_cancellable(
        dir: &Path,
        timeout: Duration,
        retry_interval: Duration,
        cancelled: impl FnMut() -> bool,
    ) -> Result<Option<Self>, StoreError> {
        Self::acquire_cancellable_with_mode(
            dir,
            timeout,
            retry_interval,
            LockOpenMode::CreateOrOpen,
            cancelled,
        )
    }

    fn acquire_cancellable_with_mode(
        dir: &Path,
        timeout: Duration,
        retry_interval: Duration,
        mode: LockOpenMode,
        mut cancelled: impl FnMut() -> bool,
    ) -> Result<Option<Self>, StoreError> {
        if cancelled() {
            return Ok(None);
        }
        let path = dir.join(OWNERSHIP_LOCK_FILE_NAME);
        let mut options = OpenOptions::new();
        let _ = options.read(true).truncate(false);
        if mode == LockOpenMode::CreateOrOpen {
            let _ = options.write(true).create(true);
        }
        #[cfg(windows)]
        if mode == LockOpenMode::ExistingOnly {
            // LockFileEx requires a handle opened with compatible data
            // access. Unix deliberately keeps this administration path
            // read-only so opening it cannot change lock-file metadata.
            let _ = options.write(true);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            let _ = options
                .mode(LOCK_FILE_MODE)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt as _;
            use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
            let _ = options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        }
        let file = options.open(&path);
        if cancelled() {
            return Ok(None);
        }
        let file = file.map_err(|source| StoreError::Io {
            operation: "open the checkpoint namespace ownership lock",
            path: path.clone(),
            source,
        })?;
        let validated = match mode {
            LockOpenMode::CreateOrOpen => fsio::secure_checkpoint_file_cancellable(
                &file,
                &path,
                "validate the checkpoint namespace ownership lock",
                &mut cancelled,
            )?,
            LockOpenMode::ExistingOnly => fsio::validate_checkpoint_file_cancellable(
                &file,
                &path,
                "validate the checkpoint namespace ownership lock",
                &mut cancelled,
            )?,
        };
        let Some(()) = validated else {
            return Ok(None);
        };

        let started = Instant::now();
        let mut contentions = 0u64;
        loop {
            if cancelled() {
                return Ok(None);
            }
            let attempt = try_lock_exclusive(&file);
            if cancelled() {
                return Ok(None);
            }
            match attempt {
                Ok(TryLockOutcome::Acquired) => {
                    return Ok(Some(Self {
                        file,
                        path,
                        waited: started.elapsed(),
                        contentions,
                    }));
                }
                Ok(TryLockOutcome::WouldBlock) => {
                    contentions = contentions.saturating_add(1);
                }
                Err(source) => {
                    return Err(StoreError::Io {
                        operation: "lock the checkpoint namespace ownership lock",
                        path,
                        source,
                    });
                }
            }
            let waited = started.elapsed();
            let Some(remaining) = timeout.checked_sub(waited) else {
                return Err(StoreError::NamespaceLocked {
                    path,
                    waited,
                    timeout,
                });
            };
            if remaining.is_zero() {
                return Err(StoreError::NamespaceLocked {
                    path,
                    waited,
                    timeout,
                });
            }
            std::thread::sleep(retry_interval.min(remaining));
        }
    }

    /// The lock file this ownership is held on.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Time spent acquiring this namespace lock.
    #[must_use]
    pub const fn waited(&self) -> Duration {
        self.waited
    }

    /// Failed immediate attempts before this lock was acquired.
    #[must_use]
    pub const fn contentions(&self) -> u64 {
        self.contentions
    }

    /// Verifies that the retained lock handle is still reachable through the
    /// canonical namespace path used to acquire it.
    pub(crate) fn verify_path_binding(&self) -> Result<(), StoreError> {
        fsio::verify_checkpoint_file_path_binding(
            &self.file,
            &self.path,
            "verify the checkpoint namespace ownership-lock path binding",
        )
    }

    /// Releases the lock explicitly, reporting a failure instead of hiding
    /// it in `Drop`.
    ///
    /// Dropping the value without calling this still releases the lock,
    /// because the operating system releases an advisory lock when the
    /// descriptor is closed.
    pub fn release(self) -> Result<(), StoreError> {
        unlock_exclusive(&self.file).map_err(|source| StoreError::Io {
            operation: "release the checkpoint namespace ownership lock",
            path: self.path.clone(),
            source,
        })
    }
}

#[cfg(all(test, windows))]
mod tests {
    use std::time::Duration;

    use super::NamespaceLock;
    use crate::receivers::filelog_receiver::checkpoint::store::layout::OWNERSHIP_LOCK_FILE_NAME;

    /// Scenario: administration acquires an already-existing Windows
    /// ownership lock without recreating the file.
    /// Guarantees: the existing-only handle has access rights compatible
    /// with exclusive LockFileEx acquisition.
    #[test]
    fn existing_windows_lock_uses_lockfileex_compatible_access() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join(OWNERSHIP_LOCK_FILE_NAME), []).unwrap();

        NamespaceLock::acquire_existing(
            directory.path(),
            Duration::from_millis(100),
            Duration::from_millis(10),
        )
        .unwrap()
        .release()
        .unwrap();
    }
}
