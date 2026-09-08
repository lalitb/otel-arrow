// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

use std::io;

pub(crate) fn thread_id() -> io::Result<u32> {
    // SAFETY: gettid has no pointer arguments and returns the calling thread's
    // identifier in its current PID namespace without retaining any state.
    let value = unsafe { libc::syscall(libc::SYS_gettid) };
    u32::try_from(value)
        .ok()
        .filter(|value| *value != 0)
        .ok_or_else(|| io::Error::other("gettid returned an invalid identifier"))
}

pub(crate) fn page_size() -> io::Result<usize> {
    // SAFETY: `_SC_PAGESIZE` takes no pointer arguments and has no side effects
    // beyond reading the process's kernel-provided page-size configuration.
    let value = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if value <= 0 {
        Err(io::Error::last_os_error())
    } else {
        usize::try_from(value).map_err(|_| io::Error::other("page size does not fit usize"))
    }
}

pub(crate) fn monotonic_nanos() -> io::Result<i128> {
    let mut value = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `value` is a writable, initialized timespec for the duration of
    // this synchronous call. CLOCK_MONOTONIC retains no userspace pointer.
    let result = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut value) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    if !(0..1_000_000_000).contains(&value.tv_nsec) {
        return Err(io::Error::other("invalid monotonic nanosecond field"));
    }
    Ok(i128::from(value.tv_sec) * 1_000_000_000 + i128::from(value.tv_nsec))
}
