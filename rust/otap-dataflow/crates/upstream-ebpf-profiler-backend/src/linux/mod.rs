// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Non-privileged Linux environment probing.

pub mod capabilities;
pub mod kernel;
pub mod system_config;
pub mod topology;

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::BackendError;

use self::capabilities::CapabilityProbe;
use self::kernel::KernelInfo;
use self::topology::CpuTopology;

/// Evidence collected before any kernel object is loaded.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SystemProbe {
    /// Linux release and architecture.
    pub kernel: KernelInfo,
    /// Effective BPF-related capabilities.
    pub capabilities: CapabilityProbe,
    /// Possible and online CPUs.
    pub topology: CpuTopology,
    /// Whether the configured kernel BTF file is readable.
    pub kernel_btf_readable: bool,
    /// Current unprivileged-BPF sysctl value when available.
    pub unprivileged_bpf_disabled: Option<u32>,
}

impl SystemProbe {
    /// Collects non-privileged host evidence.
    pub fn collect(procfs_root: &Path, kernel_btf_path: &Path) -> Result<Self, BackendError> {
        let sysctl = procfs_root.join("sys/kernel/unprivileged_bpf_disabled");
        let unprivileged_bpf_disabled = match crate::input::read_file(&sysctl, 128) {
            Ok(value) => {
                let value = std::str::from_utf8(&value).map_err(|error| {
                    BackendError::ArtifactParse(format!("BPF sysctl encoding: {error}"))
                })?;
                Some(value.trim().parse().map_err(|error| {
                    BackendError::ArtifactParse(format!("BPF sysctl value: {error}"))
                })?)
            }
            Err(BackendError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                None
            }
            Err(error) => return Err(error),
        };
        Ok(Self {
            kernel: KernelInfo::collect(procfs_root)?,
            capabilities: CapabilityProbe::collect(procfs_root)?,
            topology: CpuTopology::collect()?,
            kernel_btf_readable: std::fs::File::open(kernel_btf_path).is_ok(),
            unprivileged_bpf_disabled,
        })
    }
}
