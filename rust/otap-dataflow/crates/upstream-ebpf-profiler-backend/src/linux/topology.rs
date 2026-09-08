// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Linux CPU-list parsing.

use serde::{Deserialize, Serialize};

use crate::error::BackendError;

/// Possible and online CPU IDs.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CpuTopology {
    /// CPU IDs that the kernel may make available.
    pub possible: Vec<u32>,
    /// CPU IDs currently online.
    pub online: Vec<u32>,
}

impl CpuTopology {
    /// Reads sysfs CPU-list files.
    pub fn collect() -> Result<Self, BackendError> {
        Ok(Self {
            possible: read_cpu_list("/sys/devices/system/cpu/possible")?,
            online: read_cpu_list("/sys/devices/system/cpu/online")?,
        })
    }

    /// Returns one plus the greatest possible CPU ID.
    #[must_use]
    pub fn cpu_slots(&self) -> usize {
        self.possible
            .last()
            .and_then(|cpu| usize::try_from(*cpu).ok())
            .and_then(|cpu| cpu.checked_add(1))
            .unwrap_or(0)
    }
}

/// Parses Linux CPU-list syntax such as `0-3,8`.
pub fn parse_cpu_list(value: &str) -> Result<Vec<u32>, BackendError> {
    const MAX_CPU_IDS: usize = 65_536;
    let mut cpus = Vec::new();
    for part in value.trim().split(',').filter(|part| !part.is_empty()) {
        if let Some((start, end)) = part.split_once('-') {
            let start = parse_cpu(start)?;
            let end = parse_cpu(end)?;
            if end < start {
                return Err(BackendError::ArtifactParse(format!(
                    "reversed CPU range {part}"
                )));
            }
            for cpu in start..=end {
                if cpus.len() == MAX_CPU_IDS {
                    return Err(BackendError::Capacity("sysfs CPU list"));
                }
                cpus.push(cpu);
            }
        } else {
            if cpus.len() == MAX_CPU_IDS {
                return Err(BackendError::Capacity("sysfs CPU list"));
            }
            cpus.push(parse_cpu(part)?);
        }
    }
    cpus.sort_unstable();
    cpus.dedup();
    if cpus.is_empty() {
        return Err(BackendError::ArtifactParse("CPU list is empty".to_owned()));
    }
    Ok(cpus)
}

fn read_cpu_list(path: &str) -> Result<Vec<u32>, BackendError> {
    let path = std::path::PathBuf::from(path);
    let value = crate::input::read_file(&path, 64 * 1024)?;
    let value = std::str::from_utf8(&value)
        .map_err(|error| BackendError::ArtifactParse(format!("CPU list encoding: {error}")))?;
    parse_cpu_list(value)
}

fn parse_cpu(value: &str) -> Result<u32, BackendError> {
    value
        .parse()
        .map_err(|error| BackendError::ArtifactParse(format!("invalid CPU {value:?}: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: Sysfs reports disjoint CPU ranges and one singleton.
    /// Guarantees: Parsing expands, sorts, and deduplicates a deterministic CPU list.
    #[test]
    fn parses_linux_cpu_list() {
        assert_eq!(
            parse_cpu_list("0-2,4,6-7").expect("CPU list"),
            [0, 1, 2, 4, 6, 7]
        );
    }
}
