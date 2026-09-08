// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Typed backend configuration.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::artifact::Sha256Digest;
use crate::error::ConfigError;
use crate::limits::ResourceLimits;

/// CPU selection for system-wide sampling.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CpuSelection {
    /// Attach to every currently online CPU, subject to the configured limit.
    AllOnline,
    /// Attach only to the listed Linux CPU IDs.
    List(Vec<u32>),
}

/// PID namespace behavior.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PidNamespaceMode {
    /// Use host PID and TID values.
    Host,
    /// Ask the kernel program to translate IDs into the profiler's PID namespace.
    Current,
}

/// Configuration for artifact validation and native capture.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BackendConfig {
    /// External upstream eBPF ELF path.
    pub object_path: PathBuf,
    /// SHA-256 expected for the external artifact.
    pub expected_artifact_sha256: Sha256Digest,
    /// Pinned compatibility-version label.
    pub expected_compatibility_version: String,
    /// Frequency of software CPU-clock samples.
    pub sampling_frequency_hz: u32,
    /// CPUs on which sampling perf events are attached.
    pub cpus: CpuSelection,
    /// Whether kernel addresses are retained in output snapshots.
    ///
    /// The pinned kernel object always attempts kernel stack collection. Setting
    /// this to `false` only drops those addresses in Rust userspace.
    pub include_kernel_stacks: bool,
    /// Interval used to finalize bounded aggregation windows.
    pub reporting_interval: Duration,
    /// Maximum timer interval for draining the upstream NO_WAKEUP ring buffer.
    pub event_poll_interval: Duration,
    /// Procfs mount used for process metadata.
    pub procfs_root: PathBuf,
    /// Kernel BTF file used for compatibility probing.
    pub kernel_btf_path: PathBuf,
    /// PID namespace translation behavior.
    pub pid_namespace: PidNamespaceMode,
    /// Fail startup when any selected CPU cannot be attached.
    pub strict_cpu_attachment: bool,
    /// Optional inverse ARM pointer-authentication mask.
    pub inverse_pac_mask: Option<u64>,
    /// Explicit bounds for all host-influenced state.
    pub limits: ResourceLimits,
}

impl BackendConfig {
    /// Creates a configuration with conservative experimental defaults.
    pub fn new(
        object_path: PathBuf,
        expected_artifact_sha256: Sha256Digest,
        expected_compatibility_version: impl Into<String>,
    ) -> Self {
        Self {
            object_path,
            expected_artifact_sha256,
            expected_compatibility_version: expected_compatibility_version.into(),
            sampling_frequency_hz: 20,
            cpus: CpuSelection::AllOnline,
            include_kernel_stacks: false,
            reporting_interval: Duration::from_secs(10),
            event_poll_interval: Duration::from_millis(10),
            procfs_root: PathBuf::from("/proc"),
            kernel_btf_path: PathBuf::from("/sys/kernel/btf/vmlinux"),
            pid_namespace: PidNamespaceMode::Host,
            strict_cpu_attachment: true,
            inverse_pac_mask: None,
            limits: ResourceLimits::default(),
        }
    }

    /// Validates configuration without loading eBPF or requiring privileges.
    pub fn validate(&self) -> Result<(), ConfigError> {
        validate_absolute_path("object_path", &self.object_path)?;
        validate_absolute_path("procfs_root", &self.procfs_root)?;
        validate_absolute_path("kernel_btf_path", &self.kernel_btf_path)?;
        if self.expected_compatibility_version.trim().is_empty() {
            return Err(ConfigError::InvalidText {
                field: "expected_compatibility_version",
                detail: "must not be empty",
            });
        }
        validate_range(
            "sampling_frequency_hz",
            u64::from(self.sampling_frequency_hz),
            1,
            10_000,
        )?;
        let reporting_nanos = self.reporting_interval.as_nanos();
        if reporting_nanos == 0 {
            return Err(ConfigError::OutOfRange {
                field: "reporting_interval",
                minimum: 1,
                maximum: u64::MAX,
                actual: 0,
            });
        }
        if self.event_poll_interval.is_zero() || self.event_poll_interval > Duration::from_secs(1) {
            return Err(ConfigError::InvalidLimit {
                field: "event_poll_interval",
                detail: "must be non-zero and no greater than one second",
            });
        }
        if let CpuSelection::List(cpus) = &self.cpus {
            if cpus.is_empty() {
                return Err(ConfigError::InvalidCollection {
                    field: "cpus",
                    detail: "explicit CPU list must not be empty",
                });
            }
            let unique: BTreeSet<_> = cpus.iter().copied().collect();
            if unique.len() != cpus.len() {
                return Err(ConfigError::InvalidCollection {
                    field: "cpus",
                    detail: "explicit CPU list contains duplicates",
                });
            }
            if cpus.len() > self.limits.max_monitored_cpus {
                return Err(ConfigError::InvalidLimit {
                    field: "max_monitored_cpus",
                    detail: "is smaller than the explicit CPU list",
                });
            }
        }
        self.limits.validate()
    }
}

fn validate_absolute_path(field: &'static str, path: &Path) -> Result<(), ConfigError> {
    if path.as_os_str().is_empty() || !path.is_absolute() {
        return Err(ConfigError::InvalidPath {
            field,
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn validate_range(
    field: &'static str,
    actual: u64,
    minimum: u64,
    maximum: u64,
) -> Result<(), ConfigError> {
    if !(minimum..=maximum).contains(&actual) {
        return Err(ConfigError::OutOfRange {
            field,
            minimum,
            maximum,
            actual,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> BackendConfig {
        BackendConfig::new(
            PathBuf::from("/artifact.o"),
            Sha256Digest::ZERO,
            "test-version",
        )
    }

    /// Scenario: An explicit CPU list repeats the same CPU ID.
    /// Guarantees: Non-privileged validation rejects ambiguous duplicate
    /// attachments before any kernel resource is acquired.
    #[test]
    fn duplicate_cpus_are_rejected() {
        let mut config = config();
        config.cpus = CpuSelection::List(vec![1, 1]);
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidCollection { field: "cpus", .. })
        ));
    }

    /// Scenario: A caller configures a relative upstream artifact path.
    /// Guarantees: Validation requires an explicit absolute artifact boundary
    /// and never resolves it relative to a dataflow process working directory.
    #[test]
    fn relative_object_path_is_rejected() {
        let mut config = config();
        config.object_path = PathBuf::from("artifact.o");
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidPath {
                field: "object_path",
                ..
            })
        ));
    }
}
