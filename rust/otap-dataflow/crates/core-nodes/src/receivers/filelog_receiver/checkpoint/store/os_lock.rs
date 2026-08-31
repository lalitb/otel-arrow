// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Target-specific nonblocking advisory file-lock operations.

use std::fs::File;
use std::io;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TryLockOutcome {
    Acquired,
    WouldBlock,
}

#[cfg(unix)]
#[allow(unsafe_code)]
pub(super) fn try_lock_exclusive(file: &File) -> io::Result<TryLockOutcome> {
    use std::os::fd::AsRawFd as _;

    // SAFETY: `file` owns a live descriptor for the duration of this call.
    // `flock` does not retain the integer beyond the call.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        Ok(TryLockOutcome::Acquired)
    } else {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            Ok(TryLockOutcome::WouldBlock)
        } else {
            Err(error)
        }
    }
}

#[cfg(unix)]
#[allow(unsafe_code)]
pub(super) fn unlock_exclusive(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd as _;

    // SAFETY: `file` owns a live descriptor for the duration of this call.
    // `flock` does not retain the integer beyond the call.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
pub(super) fn try_lock_exclusive(file: &File) -> io::Result<TryLockOutcome> {
    use std::os::windows::io::AsRawHandle as _;

    use windows_sys::Win32::Foundation::{ERROR_LOCK_VIOLATION, HANDLE};
    use windows_sys::Win32::Storage::FileSystem::{
        LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
    };

    // SAFETY: `file` owns a live Windows file handle for the duration of the
    // call. An all-zero OVERLAPPED selects byte offset zero, and the API does
    // not retain the stack value after this synchronous call returns.
    let result = unsafe {
        let mut overlapped = std::mem::zeroed();
        LockFileEx(
            file.as_raw_handle() as HANDLE,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if result != 0 {
        Ok(TryLockOutcome::Acquired)
    } else {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
            Ok(TryLockOutcome::WouldBlock)
        } else {
            Err(error)
        }
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
pub(super) fn unlock_exclusive(file: &File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle as _;

    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::UnlockFileEx;

    // SAFETY: `file` owns the live handle. The zeroed OVERLAPPED selects the
    // same offset-zero range passed to LockFileEx, and this synchronous call
    // does not retain the stack value after returning.
    let result = unsafe {
        let mut overlapped = std::mem::zeroed();
        UnlockFileEx(
            file.as_raw_handle() as HANDLE,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if result != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(unix, windows)))]
pub(super) fn try_lock_exclusive(_file: &File) -> io::Result<TryLockOutcome> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "checkpoint namespace locking is unsupported on this platform",
    ))
}

#[cfg(not(any(unix, windows)))]
pub(super) fn unlock_exclusive(_file: &File) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "checkpoint namespace locking is unsupported on this platform",
    ))
}

#[cfg(all(test, any(unix, windows)))]
mod tests {
    use std::fs::OpenOptions;

    use super::{TryLockOutcome, try_lock_exclusive, unlock_exclusive};

    fn independent_open_contention() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("ownership.lock");
        let open = || {
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .expect("lock file opens")
        };
        let first = open();
        let second = open();

        assert_eq!(
            try_lock_exclusive(&first).expect("first lock attempt succeeds"),
            TryLockOutcome::Acquired
        );
        assert_eq!(
            try_lock_exclusive(&second).expect("second lock attempt is classified"),
            TryLockOutcome::WouldBlock
        );
        unlock_exclusive(&first).expect("first lock releases");
        assert_eq!(
            try_lock_exclusive(&second).expect("second lock acquires after release"),
            TryLockOutcome::Acquired
        );
        unlock_exclusive(&second).expect("second lock releases");
    }

    /// Scenario: two independently opened Unix file descriptions contend for
    /// one namespace lock.
    /// Guarantees: `flock(LOCK_EX | LOCK_NB)` rejects the second description
    /// and permits it after the first unlocks.
    #[cfg(unix)]
    #[test]
    fn unix_flock_conflicts_between_independent_descriptions() {
        independent_open_contention();
    }

    /// Scenario: two independently opened Windows file handles contend for one
    /// namespace lock.
    /// Guarantees: `LockFileEx` with exclusive and fail-immediately flags
    /// rejects the second handle and `UnlockFileEx` permits it after release.
    #[cfg(windows)]
    #[test]
    fn windows_lock_file_ex_conflicts_between_independent_handles() {
        independent_open_contention();
    }
}
