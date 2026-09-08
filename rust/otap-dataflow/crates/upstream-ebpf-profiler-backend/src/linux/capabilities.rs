// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Linux capability probing from procfs.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::BackendError;

const CAP_SYS_ADMIN: u32 = 21;
const CAP_PERFMON: u32 = 38;
const CAP_BPF: u32 = 39;

/// Effective capabilities relevant to BPF loading and perf attachment.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CapabilityProbe {
    /// Raw effective capability bitset.
    pub effective: u64,
    /// `CAP_SYS_ADMIN` is effective.
    pub sys_admin: bool,
    /// `CAP_PERFMON` is effective.
    pub perfmon: bool,
    /// `CAP_BPF` is effective.
    pub bpf: bool,
}

impl CapabilityProbe {
    /// Reads `/proc/self/status`.
    pub fn collect(procfs_root: &Path) -> Result<Self, BackendError> {
        let path = procfs_root.join("self/status");
        let contents = crate::input::read_file(&path, 64 * 1024)?;
        let contents = std::str::from_utf8(&contents).map_err(|error| {
            BackendError::ArtifactParse(format!("process status encoding: {error}"))
        })?;
        let value = contents
            .lines()
            .find_map(|line| line.strip_prefix("CapEff:"))
            .ok_or_else(|| BackendError::ArtifactParse("CapEff is absent".to_owned()))?
            .trim();
        let effective = u64::from_str_radix(value, 16)
            .map_err(|error| BackendError::ArtifactParse(format!("invalid CapEff: {error}")))?;
        Ok(Self {
            effective,
            sys_admin: bit(effective, CAP_SYS_ADMIN),
            perfmon: bit(effective, CAP_PERFMON),
            bpf: bit(effective, CAP_BPF),
        })
    }

    /// Returns whether the process has the usual capability set for this backend.
    #[must_use]
    pub const fn can_attempt_native_load(self) -> bool {
        self.sys_admin || (self.bpf && self.perfmon)
    }
}

const fn bit(value: u64, index: u32) -> bool {
    value & (1_u64 << index) != 0
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// Scenario: The current process capability set is inspected as plain procfs text.
    /// Guarantees: Probing itself does not require root or issue a BPF syscall.
    #[test]
    fn current_capabilities_are_readable() {
        let probe = CapabilityProbe::collect(Path::new("/proc")).expect("capabilities");
        assert_eq!(probe.bpf, bit(probe.effective, CAP_BPF));
    }
}
