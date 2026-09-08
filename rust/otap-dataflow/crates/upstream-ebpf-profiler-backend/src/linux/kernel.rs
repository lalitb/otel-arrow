// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Linux release parsing.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::BackendError;

/// Parsed Linux version.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct KernelVersion {
    /// Major release.
    pub major: u32,
    /// Minor release.
    pub minor: u32,
    /// Patch release when present.
    pub patch: u32,
}

impl KernelVersion {
    /// Parses the numeric prefix of a Linux release string.
    pub fn parse(release: &str) -> Result<Self, BackendError> {
        let numeric = release.split('-').next().unwrap_or(release);
        let mut fields = numeric.split('.');
        let parse = |value: Option<&str>, label: &str| -> Result<u32, BackendError> {
            value
                .unwrap_or("0")
                .parse()
                .map_err(|error| BackendError::ArtifactParse(format!("kernel {label}: {error}")))
        };
        Ok(Self {
            major: parse(fields.next(), "major")?,
            minor: parse(fields.next(), "minor")?,
            patch: parse(fields.next(), "patch")?,
        })
    }

    /// Returns whether sched_process_free uses the Linux 6.16 layout.
    #[must_use]
    pub const fn uses_sched_process_free_v2(self) -> bool {
        self.major > 6 || (self.major == 6 && self.minor >= 16)
    }
}

/// Kernel evidence used by the loader.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct KernelInfo {
    /// Complete release string.
    pub release: String,
    /// Parsed numeric version.
    pub version: KernelVersion,
    /// Rust target architecture.
    pub architecture: String,
}

impl KernelInfo {
    /// Reads the kernel release from procfs.
    pub fn collect(procfs_root: &Path) -> Result<Self, BackendError> {
        let path = procfs_root.join("sys/kernel/osrelease");
        let release = crate::input::read_file(&path, 4096)?;
        let release = std::str::from_utf8(&release).map_err(|error| {
            BackendError::ArtifactParse(format!("kernel release encoding: {error}"))
        })?;
        let release = release.trim().to_owned();
        Ok(Self {
            version: KernelVersion::parse(&release)?,
            release,
            architecture: std::env::consts::ARCH.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: Kernel releases on both sides of the sched tracepoint ABI change are parsed.
    /// Guarantees: The loader selects exactly one upstream process-free program.
    #[test]
    fn sched_layout_boundary_is_616() {
        assert!(
            !KernelVersion::parse("6.15.9")
                .expect("version")
                .uses_sched_process_free_v2()
        );
        assert!(
            KernelVersion::parse("6.16.0-rc1")
                .expect("version")
                .uses_sched_process_free_v2()
        );
    }
}
