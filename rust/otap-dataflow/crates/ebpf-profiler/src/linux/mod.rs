// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Linux Aya backend and sysfs topology discovery.

mod btf_preflight;
mod capability;
mod kernel;
mod perf;
mod procfs;
mod program;

pub(crate) use program::LinuxPlatform;

pub(crate) fn boot_time_ns() -> crate::Result<u64> {
    let time = nix::time::clock_gettime(nix::time::ClockId::CLOCK_BOOTTIME)
        .map_err(|error| crate::ProfilerError::UnsupportedKernel(error.to_string()))?;
    let seconds = u64::try_from(time.tv_sec())
        .map_err(|_| crate::ProfilerError::UnsupportedKernel("negative BOOTTIME".to_owned()))?;
    let nanoseconds = u64::try_from(time.tv_nsec())
        .map_err(|_| crate::ProfilerError::UnsupportedKernel("negative BOOTTIME".to_owned()))?;
    seconds
        .checked_mul(1_000_000_000)
        .and_then(|value| value.checked_add(nanoseconds))
        .ok_or_else(|| crate::ProfilerError::UnsupportedKernel("BOOTTIME overflow".to_owned()))
}
