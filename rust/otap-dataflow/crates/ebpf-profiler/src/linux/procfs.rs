// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Bounded Linux sysfs CPU and NUMA topology discovery.

use std::{
    fs::{self, File},
    io::{self, Read},
    path::Path,
};

use crate::{CpuInfo, Limits, NumaNodeId, ProfilerError, Result, SystemTopology};

pub(crate) fn read_bounded(path: &Path, max_bytes: usize) -> Result<Vec<u8>> {
    let file =
        File::open(path).map_err(|source| io_error("open bounded system file", path, source))?;
    read_handle(file, path, max_bytes)
}

pub(crate) fn read_handle(file: File, path: &Path, max_bytes: usize) -> Result<Vec<u8>> {
    let limit = max_bytes
        .checked_add(1)
        .ok_or_else(|| ProfilerError::TopologyDiscovery("file byte limit overflow".to_owned()))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(limit)
        .map_err(|_| ProfilerError::Allocation("bounded file"))?;
    let _read = file
        .take(limit as u64)
        .read_to_end(&mut bytes)
        .map_err(|source| io_error("read bounded system file", path, source))?;
    if bytes.len() > max_bytes {
        return Err(ProfilerError::TopologyDiscovery(format!(
            "{} exceeds {max_bytes} bytes",
            path.display()
        )));
    }
    Ok(bytes)
}

fn io_error(operation: &'static str, path: &Path, source: io::Error) -> ProfilerError {
    ProfilerError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

pub(crate) fn discover(limits: &Limits) -> Result<SystemTopology> {
    discover_at(Path::new("/sys/devices/system/cpu"), limits)
}

fn cpu_ids(path: &Path, limits: &Limits) -> Result<Vec<u32>> {
    let bytes = read_bounded(path, limits.max_procfs_file_bytes)?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|error| ProfilerError::TopologyDiscovery(error.to_string()))?;
    parse_cpu_list(text.trim(), limits.max_possible_cpus, limits.max_cpu_id)
}

fn discover_at(root: &Path, limits: &Limits) -> Result<SystemTopology> {
    let possible = cpu_ids(&root.join("possible"), limits)?;
    let online = cpu_ids(&root.join("online"), limits)?;
    if online
        .iter()
        .any(|cpu| possible.binary_search(cpu).is_err())
    {
        return Err(ProfilerError::TopologyDiscovery(
            "online CPU is not possible".to_owned(),
        ));
    }
    let mut cpus = Vec::with_capacity(possible.len());
    for id in possible {
        let online = online.binary_search(&id).is_ok();
        cpus.push(CpuInfo {
            id,
            online,
            numa_node: if online {
                discover_numa_node(&root.join(format!("cpu{id}")), limits.max_procfs_file_bytes)?
            } else {
                None
            },
        });
    }
    SystemTopology::new(cpus)
}

fn parse_cpu_list(input: &str, max_cpus: usize, max_id: u32) -> Result<Vec<u32>> {
    let mut cpus = Vec::new();
    for part in input.split(',') {
        let (start, end) = match part.split_once('-') {
            Some((start, end)) => (parse_cpu(start)?, parse_cpu(end)?),
            None => {
                let cpu = parse_cpu(part)?;
                (cpu, cpu)
            }
        };
        if end < start || end > max_id {
            return Err(ProfilerError::TopologyDiscovery(format!(
                "invalid or out-of-bound CPU range {part}"
            )));
        }
        let count = u64::from(end) - u64::from(start) + 1;
        if count > max_cpus.saturating_sub(cpus.len()) as u64 {
            return Err(ProfilerError::TopologyDiscovery(format!(
                "possible/online CPU count exceeds {max_cpus}"
            )));
        }
        cpus.extend(start..=end);
    }
    cpus.sort_unstable();
    if cpus.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(ProfilerError::TopologyDiscovery(
            "duplicate CPU identifier".to_owned(),
        ));
    }
    Ok(cpus)
}

fn parse_cpu(value: &str) -> Result<u32> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ProfilerError::TopologyDiscovery(format!(
            "invalid CPU {value}"
        )));
    }
    value
        .parse()
        .map_err(|error| ProfilerError::TopologyDiscovery(format!("invalid CPU {value}: {error}")))
}

fn discover_numa_node(path: &Path, max_bytes: usize) -> Result<Option<NumaNodeId>> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(io_error("read CPU NUMA directory", path, source)),
    };
    let mut remaining = max_bytes;
    let mut node = None;
    for entry in entries {
        let entry = entry.map_err(|source| io_error("read CPU NUMA entry", path, source))?;
        let name = entry.file_name();
        remaining = remaining
            .checked_sub(name.as_encoded_bytes().len() + 1)
            .ok_or_else(|| {
                ProfilerError::TopologyDiscovery("CPU directory exceeds byte limit".to_owned())
            })?;
        let Some(suffix) = name.to_str().and_then(|name| name.strip_prefix("node")) else {
            continue;
        };
        if suffix.is_empty() || !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let value = suffix
            .parse::<u32>()
            .map_err(|error| ProfilerError::TopologyDiscovery(error.to_string()))?;
        if node.replace(NumaNodeId(value)).is_some() {
            return Err(ProfilerError::TopologyDiscovery(
                "CPU has multiple NUMA nodes".to_owned(),
            ));
        }
    }
    Ok(node)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: Sysfs exposes compact ranges and sparse identifiers.
    /// Guarantees: Count and identifier limits are independently enforced.
    #[test]
    fn bounded_sparse_cpu_lists() {
        assert_eq!(
            parse_cpu_list("8,0-2,4095", 5, 4095).expect("CPU list"),
            vec![0, 1, 2, 8, 4095]
        );
        for value in ["", "0-8", "4096", "0,0", "3-1", "0-4294967295", "+1"] {
            assert!(parse_cpu_list(value, 8, 4095).is_err(), "{value}");
        }
    }

    /// Scenario: A host has more possible CPUs than selected monitoring slots.
    /// Guarantees: Discovery permits later Include selection and preserves offline CPUs.
    #[test]
    fn topology_bounds_possible_not_selected_cpus() {
        let directory = tempfile::tempdir_in(".").expect("fixture directory");
        fs::write(directory.path().join("possible"), "0-3").expect("possible");
        fs::write(directory.path().join("online"), "0,3").expect("online");
        fs::create_dir(directory.path().join("cpu3")).expect("CPU directory");
        fs::create_dir(directory.path().join("cpu3/node2")).expect("node");
        let limits = Limits {
            max_monitored_cpus: 1,
            max_possible_cpus: 4,
            ..Limits::default()
        };
        let topology = discover_at(directory.path(), &limits).expect("topology");
        assert_eq!(topology.online_cpu_ids(), vec![0, 3]);
        assert_eq!(
            topology.cpu(3).expect("CPU3").numa_node,
            Some(NumaNodeId(2))
        );
        assert_eq!(topology.cpu(0).expect("CPU0").numa_node, None);
        assert!(!topology.cpu(1).expect("CPU1").online);
    }

    /// Scenario: A sysfs file exceeds its byte budget or is unreadable as a file.
    /// Guarantees: Bounded reads fail explicitly instead of allocating or hiding I/O errors.
    #[test]
    fn bounded_reads_and_directory_errors_are_reported() {
        let directory = tempfile::tempdir_in(".").expect("fixture directory");
        let path = directory.path().join("file");
        fs::write(&path, "12345").expect("file");
        assert!(read_bounded(&path, 4).is_err());
        assert!(matches!(
            discover_numa_node(&path, 100),
            Err(ProfilerError::Io { .. })
        ));
    }
}
