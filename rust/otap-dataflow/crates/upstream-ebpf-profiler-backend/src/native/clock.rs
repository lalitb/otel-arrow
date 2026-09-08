// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Convert local monotonic observations to the kernel's initial-time-namespace clock.

use std::os::unix::fs::MetadataExt;
use std::path::Path;

use crate::error::BackendError;

#[derive(Clone, Copy, Debug)]
pub(crate) struct KernelClock {
    namespace: Option<(u64, u64)>,
    monotonic_offset: i128,
}

impl KernelClock {
    pub(crate) fn collect() -> Result<Self, BackendError> {
        let namespace = namespace_identity("/proc/self/ns/time")?;
        let children = namespace_identity("/proc/self/ns/time_for_children")?;
        if namespace != children {
            return Err(clock_error("current and child time namespaces differ"));
        }
        let monotonic_offset = if namespace.is_some() {
            let bytes = crate::input::read_file(Path::new("/proc/self/timens_offsets"), 1024)?;
            let text = std::str::from_utf8(&bytes)
                .map_err(|_| clock_error("invalid time-namespace encoding"))?;
            parse_monotonic_offset(text)?
        } else {
            // Kernels without CONFIG_TIME_NS have only the initial clock.
            0
        };
        Ok(Self {
            namespace,
            monotonic_offset,
        })
    }

    pub(crate) fn now(&self) -> Result<u64, BackendError> {
        if namespace_identity("/proc/self/ns/time")? != self.namespace {
            return Err(clock_error("time namespace changed during capture"));
        }
        let local = super::syscall::monotonic_nanos()
            .map_err(|error| BackendError::kernel("read monotonic clock", error.to_string()))?;
        u64::try_from(local - self.monotonic_offset)
            .map_err(|_| clock_error("kernel monotonic time is outside its u64 range"))
    }
}

fn namespace_identity(path: &str) -> Result<Option<(u64, u64)>, BackendError> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(Some((metadata.dev(), metadata.ino()))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(BackendError::Io {
            path: path.into(),
            source,
        }),
    }
}

fn parse_monotonic_offset(text: &str) -> Result<i128, BackendError> {
    let mut monotonic = None;
    for line in text.lines() {
        let fields: Vec<_> = line.split_whitespace().take(4).collect();
        if fields.len() != 3 || !matches!(fields[0], "monotonic" | "boottime") {
            return Err(clock_error("invalid time-namespace record"));
        }
        let seconds: i64 = fields[1]
            .parse()
            .map_err(|_| clock_error("invalid time-namespace seconds"))?;
        let nanos: u32 = fields[2]
            .parse()
            .map_err(|_| clock_error("invalid time-namespace nanoseconds"))?;
        if nanos >= 1_000_000_000 {
            return Err(clock_error("time-namespace nanoseconds exceed one second"));
        }
        if fields[0] == "monotonic" {
            if monotonic.is_some() {
                return Err(clock_error("duplicate monotonic time offset"));
            }
            monotonic = Some(i128::from(seconds) * 1_000_000_000 + i128::from(nanos));
        }
    }
    monotonic.ok_or_else(|| clock_error("monotonic time offset is absent"))
}

fn clock_error(detail: &'static str) -> BackendError {
    BackendError::kernel("kernel clock observation", detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: A container shifts monotonic time independently from boot time, including negative seconds.
    /// Guarantees: Kernel-clock conversion uses the exact signed monotonic offset rather than uptime or realtime.
    #[test]
    fn signed_time_offsets_are_parsed() {
        assert_eq!(
            parse_monotonic_offset("monotonic -2 500000000\nboottime 90 0\n").expect("offset"),
            -1_500_000_000
        );
        assert_eq!(
            parse_monotonic_offset("monotonic 172800 0\nboottime 604800 0\n").expect("offset"),
            172_800_000_000_000
        );
    }

    /// Scenario: Namespace records are missing, duplicated, truncated, or out of range.
    /// Guarantees: Invalid clock metadata cannot silently admit traces against the wrong generation interval.
    #[test]
    fn malformed_time_offsets_are_rejected() {
        for text in [
            "",
            "boottime 0 0",
            "monotonic 0",
            "monotonic 0 1000000000",
            "monotonic 0 0\nmonotonic 1 0",
        ] {
            assert!(parse_monotonic_offset(text).is_err());
        }
    }
}
