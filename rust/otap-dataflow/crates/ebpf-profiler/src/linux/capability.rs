// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Linux runtime prerequisite probes; UID zero does not imply capabilities.

use super::procfs::read_bounded;
use crate::{ProfilerError, Result, ValidatedConfig};
use std::{io, path::Path};

const CAP_SYS_ADMIN: u64 = 1 << 21;
const CAP_PERFMON: u64 = 1 << 38;
const CAP_BPF: u64 = 1 << 39;

pub(crate) fn probe(config: &ValidatedConfig) -> Result<()> {
    validate_architecture(std::env::consts::ARCH)?;
    let release = read_bounded(Path::new("/proc/sys/kernel/osrelease"), 4096)?;
    validate_release(
        std::str::from_utf8(&release)
            .map_err(|error| ProfilerError::UnsupportedKernel(error.to_string()))?,
    )?;
    let status = read_bounded(
        Path::new("/proc/self/status"),
        config.get().limits.max_procfs_file_bytes,
    )?;
    validate_capabilities(
        std::str::from_utf8(&status)
            .map_err(|error| ProfilerError::InsufficientPrivileges(error.to_string()))?,
    )?;
    if config.get().program.require_btf {
        let path = Path::new("/sys/kernel/btf/vmlinux");
        match std::fs::metadata(path) {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => return Err(ProfilerError::UnavailableBtf(path.into())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(ProfilerError::UnavailableBtf(path.into()));
            }
            Err(source) => {
                return Err(ProfilerError::Io {
                    operation: "inspect required BTF",
                    path: path.into(),
                    source,
                });
            }
        }
    }
    Ok(())
}

fn validate_architecture(architecture: &str) -> Result<()> {
    if !matches!(architecture, "x86_64" | "aarch64") || cfg!(target_endian = "big") {
        return Err(ProfilerError::UnsupportedArchitecture(
            architecture.to_owned(),
        ));
    }
    Ok(())
}

fn validate_release(release: &str) -> Result<()> {
    let mut parts = release.trim().split('.');
    let major = parts.next().and_then(|part| part.parse::<u32>().ok());
    let minor = parts.next().and_then(|part| part.parse::<u32>().ok());
    match (major, minor) {
        (Some(major), Some(minor)) if (major, minor) >= (5, 8) => Ok(()),
        _ => Err(ProfilerError::UnsupportedKernel(format!(
            "Linux >=5.8 is required for BOOTTIME stack samples; release {}",
            release.trim()
        ))),
    }
}

fn validate_capabilities(status: &str) -> Result<()> {
    let bits = status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:"))
        .and_then(|value| u64::from_str_radix(value.trim(), 16).ok())
        .ok_or_else(|| {
            ProfilerError::InsufficientPrivileges("cannot parse effective capabilities".to_owned())
        })?;
    if bits & CAP_SYS_ADMIN != 0 || bits & (CAP_BPF | CAP_PERFMON) == CAP_BPF | CAP_PERFMON {
        return Ok(());
    }
    Err(ProfilerError::InsufficientPrivileges(format!(
        "requires effective CAP_BPF + CAP_PERFMON or CAP_SYS_ADMIN; CapEff={bits:016x}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: Root or a partial capability set attempts privileged sampling.
    /// Guarantees: Only CAP_SYS_ADMIN or both modern capabilities pass the gate.
    #[test]
    fn capabilities_not_uid_control_probe() {
        for bits in [0, CAP_BPF, CAP_PERFMON] {
            assert!(validate_capabilities(&format!("Uid:\t0\nCapEff:\t{bits:016x}")).is_err());
        }
        for bits in [CAP_SYS_ADMIN, CAP_BPF | CAP_PERFMON] {
            assert!(validate_capabilities(&format!("CapEff:\t{bits:016x}")).is_ok());
        }
        assert!(validate_capabilities("CapEff: invalid").is_err());
    }

    /// Scenario: Linux versions straddle the BOOTTIME helper minimum.
    /// Guarantees: Old/malformed kernels and unsupported architectures are typed failures.
    #[test]
    fn minimum_kernel_and_architecture() {
        for release in ["5.8.0", "6.6.114.1-microsoft-standard-WSL2", "7.0.0"] {
            assert!(validate_release(release).is_ok());
        }
        for release in ["5.7.99", "4.19.0", "invalid", "6"] {
            assert!(matches!(
                validate_release(release),
                Err(ProfilerError::UnsupportedKernel(_))
            ));
        }
        assert!(matches!(
            validate_architecture("riscv64"),
            Err(ProfilerError::UnsupportedArchitecture(_))
        ));
    }
}
