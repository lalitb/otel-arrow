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
//! Both are reached through the `fs4` crate, which maps directly onto those
//! two APIs and adds no lock implementation of its own. The lock is released
//! when the file descriptor/handle is closed, which happens when
//! [`NamespaceLock`] is dropped, including on abnormal process exit.
//!
//! Acquisition is bounded: it retries at a fixed interval until the
//! configured `checkpoint.ownership_timeout` elapses and then fails with
//! [`StoreError::NamespaceLocked`] rather than blocking the thread forever.
//! Blocking sleeps are acceptable here only because this type is used from
//! the dedicated read/checkpoint OS thread, never from async code.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fs4::{FileExt, TryLockError};

use super::error::StoreError;
use super::fsio;
use super::layout::OWNERSHIP_LOCK_FILE_NAME;

/// Unix mode for the ownership lock file.
#[cfg(unix)]
const LOCK_FILE_MODE: u32 = 0o600;

/// Exclusive ownership of a checkpoint namespace, held for the lifetime of
/// the value.
#[derive(Debug)]
pub struct NamespaceLock {
    /// Holding the open file keeps the advisory lock: closing the
    /// descriptor is what releases it.
    file: File,
    path: PathBuf,
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
        let path = dir.join(OWNERSHIP_LOCK_FILE_NAME);
        let mut options = OpenOptions::new();
        let _ = options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            let _ = options.mode(LOCK_FILE_MODE).custom_flags(libc::O_NOFOLLOW);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt as _;
            use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
            let _ = options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        }
        let file = options.open(&path).map_err(|source| StoreError::Io {
            operation: "open the checkpoint namespace ownership lock",
            path: path.clone(),
            source,
        })?;
        fsio::secure_checkpoint_file(
            &file,
            &path,
            "validate the checkpoint namespace ownership lock",
        )?;

        let started = Instant::now();
        loop {
            // Called as an explicit trait call rather than `file.try_lock()`:
            // `std::fs::File` gained an inherent `try_lock` in Rust 1.89,
            // which would silently take precedence over the trait method on
            // a newer toolchain while failing to compile at this
            // workspace's 1.87 MSRV.
            match FileExt::try_lock(&file) {
                Ok(()) => return Ok(Self { file, path }),
                Err(TryLockError::WouldBlock) => {}
                Err(TryLockError::Error(source)) => {
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

    /// Releases the lock explicitly, reporting a failure instead of hiding
    /// it in `Drop`.
    ///
    /// Dropping the value without calling this still releases the lock,
    /// because the operating system releases an advisory lock when the
    /// descriptor is closed.
    pub fn release(self) -> Result<(), StoreError> {
        FileExt::unlock(&self.file).map_err(|source| StoreError::Io {
            operation: "release the checkpoint namespace ownership lock",
            path: self.path.clone(),
            source,
        })
    }
}
