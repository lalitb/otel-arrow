// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Linux NUMA topology discovery from sysfs.
//!
//! Phase 1 of the reuseport listener-manager work: provides a small
//! self-contained CPU-id to NUMA-node mapping built by parsing
//! `/sys/devices/system/node/node*/cpulist`. No `libnuma`/`hwloc`
//! dependency. Non-Linux hosts and unreadable sysfs degrade to an empty
//! mapping; lookups for unknown CPUs return `None`, and callers that need a
//! concrete node fall back to `0` to preserve today's behaviour.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Cached mapping from CPU id to NUMA node id.
///
/// The mapping is `Vec<Option<u32>>` indexed by CPU id; `None` means "no
/// information for this CPU id" (either out of range of the discovered
/// topology, or sysfs was unreadable).
#[derive(Clone, Debug, Default)]
pub struct CpuTopology {
    inner: Arc<TopologyInner>,
}

#[derive(Debug, Default)]
struct TopologyInner {
    /// CPU id -> NUMA node id, or `None` for unknown CPUs.
    mapping: Vec<Option<u32>>,
    /// Number of NUMA nodes seen during discovery (max node id + 1).
    node_count: u32,
}

impl CpuTopology {
    /// Builds an empty topology. Lookups return `None`.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Detects NUMA topology by scanning `/sys/devices/system/node`.
    ///
    /// Returns an empty topology on any of the following safe-degraded
    /// conditions:
    ///
    /// * non-Linux platforms (the sysfs path simply does not exist),
    /// * the sysfs root is unreadable,
    /// * any individual `nodeN/cpulist` file fails to read or parse.
    ///
    /// Never panics.
    #[must_use]
    pub fn detect() -> Self {
        Self::from_sysfs_root(Path::new("/sys/devices/system/node"))
    }

    /// Detects NUMA topology under an arbitrary sysfs root.
    ///
    /// Public for tests; production callers should use [`Self::detect`].
    #[must_use]
    pub fn from_sysfs_root(root: &Path) -> Self {
        match read_node_cpulists(root) {
            Ok(entries) => Self::from_node_cpulists(&entries),
            Err(_) => Self::empty(),
        }
    }

    /// Builds a topology from in-memory `(node_id, cpulist)` pairs.
    ///
    /// Useful for tests. Out-of-range or unparseable entries are skipped
    /// and contribute nothing to the mapping.
    #[must_use]
    pub fn from_node_cpulists(entries: &[(u32, String)]) -> Self {
        let mut mapping: Vec<Option<u32>> = Vec::new();
        let mut node_count: u32 = 0;
        for (node_id, cpulist) in entries {
            let cpus = match parse_cpu_list(cpulist) {
                Ok(cpus) => cpus,
                Err(_) => continue,
            };
            if !cpus.is_empty() {
                node_count = node_count.max(node_id.saturating_add(1));
            }
            for cpu in cpus {
                let cpu = cpu as usize;
                if cpu >= mapping.len() {
                    mapping.resize(cpu + 1, None);
                }
                mapping[cpu] = Some(*node_id);
            }
        }
        Self {
            inner: Arc::new(TopologyInner {
                mapping,
                node_count,
            }),
        }
    }

    /// Returns the NUMA node for the given CPU id, or `None` if unknown.
    #[must_use]
    pub fn numa_node(&self, cpu_id: u32) -> Option<u32> {
        self.inner
            .mapping
            .get(cpu_id as usize)
            .and_then(|slot| *slot)
    }

    /// Returns the NUMA node for the given CPU id, falling back to `0`
    /// when unknown. Use this from telemetry/attribute paths that
    /// require a concrete value to preserve pre-Phase-1 behaviour.
    #[must_use]
    pub fn numa_node_or_zero(&self, cpu_id: u32) -> u32 {
        self.numa_node(cpu_id).unwrap_or(0)
    }

    /// Returns the highest NUMA node id seen plus one. Zero means the
    /// topology is empty.
    #[must_use]
    pub fn node_count(&self) -> u32 {
        self.inner.node_count
    }

    /// Returns `true` when no topology information is available.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.mapping.is_empty()
    }
}

/// Errors returned by [`parse_cpu_list`].
#[derive(Debug, Eq, PartialEq)]
pub enum ParseError {
    /// The input was empty or contained only whitespace.
    Empty,
    /// A token in the comma list was empty (e.g. leading/trailing `,`).
    EmptyToken,
    /// A range had a malformed `start-end` pair.
    BadRange(String),
    /// The range start was strictly greater than the range end.
    ReversedRange(u32, u32),
    /// A CPU id failed to parse as `u32`.
    BadCpu(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "empty cpulist"),
            Self::EmptyToken => write!(f, "empty token in cpulist"),
            Self::BadRange(s) => write!(f, "invalid range `{s}` in cpulist"),
            Self::ReversedRange(a, b) => {
                write!(f, "reversed range `{a}-{b}` in cpulist")
            }
            Self::BadCpu(s) => write!(f, "invalid cpu id `{s}` in cpulist"),
        }
    }
}

impl std::error::Error for ParseError {}

/// Parses a Linux cpulist string such as `"0-3,8,10-12"` into a sorted,
/// de-duplicated vector of CPU ids.
///
/// Whitespace around the input and around individual tokens is ignored,
/// matching the kernel's relaxed parsing.
///
/// # Errors
///
/// Returns [`ParseError`] for empty input, empty tokens, malformed ranges,
/// reversed ranges, or non-numeric CPU ids.
pub fn parse_cpu_list(input: &str) -> Result<Vec<u32>, ParseError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(ParseError::Empty);
    }
    let mut cpus: Vec<u32> = Vec::new();
    for raw in input.split(',') {
        let token = raw.trim();
        if token.is_empty() {
            return Err(ParseError::EmptyToken);
        }
        if let Some((lo, hi)) = token.split_once('-') {
            let lo = lo.trim();
            let hi = hi.trim();
            if lo.is_empty() || hi.is_empty() {
                return Err(ParseError::BadRange(token.to_string()));
            }
            let lo: u32 = lo
                .parse()
                .map_err(|_| ParseError::BadRange(token.to_string()))?;
            let hi: u32 = hi
                .parse()
                .map_err(|_| ParseError::BadRange(token.to_string()))?;
            if lo > hi {
                return Err(ParseError::ReversedRange(lo, hi));
            }
            for cpu in lo..=hi {
                cpus.push(cpu);
            }
        } else {
            let cpu: u32 = token
                .parse()
                .map_err(|_| ParseError::BadCpu(token.to_string()))?;
            cpus.push(cpu);
        }
    }
    cpus.sort_unstable();
    cpus.dedup();
    Ok(cpus)
}

fn read_node_cpulists(root: &Path) -> std::io::Result<Vec<(u32, String)>> {
    let mut entries = Vec::new();
    for dir_entry in fs::read_dir(root)? {
        let dir_entry = dir_entry?;
        let path = dir_entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(node_id_str) = name.strip_prefix("node") else {
            continue;
        };
        let Ok(node_id) = node_id_str.parse::<u32>() else {
            continue;
        };
        let cpulist_path: PathBuf = path.join("cpulist");
        let Ok(contents) = fs::read_to_string(&cpulist_path) else {
            continue;
        };
        entries.push((node_id, contents));
    }
    Ok(entries)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_cpu() {
        assert_eq!(parse_cpu_list("0").unwrap(), vec![0]);
        assert_eq!(parse_cpu_list("  17  ").unwrap(), vec![17]);
    }

    #[test]
    fn parse_range() {
        assert_eq!(parse_cpu_list("0-3").unwrap(), vec![0, 1, 2, 3]);
        assert_eq!(parse_cpu_list("5-5").unwrap(), vec![5]);
    }

    #[test]
    fn parse_comma_list() {
        assert_eq!(parse_cpu_list("0,2,4").unwrap(), vec![0, 2, 4]);
    }

    #[test]
    fn parse_mixed() {
        assert_eq!(
            parse_cpu_list("0-3,8,10-12").unwrap(),
            vec![0, 1, 2, 3, 8, 10, 11, 12]
        );
    }

    #[test]
    fn parse_dedups_and_sorts() {
        assert_eq!(parse_cpu_list("3,1,2,1,3").unwrap(), vec![1, 2, 3]);
        assert_eq!(parse_cpu_list("0-2,1-3").unwrap(), vec![0, 1, 2, 3]);
    }

    #[test]
    fn parse_rejects_empty_input() {
        assert_eq!(parse_cpu_list("").unwrap_err(), ParseError::Empty);
        assert_eq!(parse_cpu_list("   ").unwrap_err(), ParseError::Empty);
    }

    #[test]
    fn parse_rejects_empty_token() {
        assert_eq!(parse_cpu_list(",0").unwrap_err(), ParseError::EmptyToken);
        assert_eq!(parse_cpu_list("0,,2").unwrap_err(), ParseError::EmptyToken);
        assert_eq!(parse_cpu_list("0,").unwrap_err(), ParseError::EmptyToken);
    }

    #[test]
    fn parse_rejects_bad_range() {
        match parse_cpu_list("0-").unwrap_err() {
            ParseError::BadRange(_) => {}
            other => panic!("unexpected: {other:?}"),
        }
        match parse_cpu_list("-3").unwrap_err() {
            ParseError::BadRange(_) => {}
            other => panic!("unexpected: {other:?}"),
        }
        match parse_cpu_list("a-b").unwrap_err() {
            ParseError::BadRange(_) => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_rejects_reversed_range() {
        assert_eq!(
            parse_cpu_list("5-2").unwrap_err(),
            ParseError::ReversedRange(5, 2)
        );
    }

    #[test]
    fn parse_rejects_non_numeric() {
        match parse_cpu_list("0,abc,2").unwrap_err() {
            ParseError::BadCpu(s) => assert_eq!(s, "abc"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn topology_from_node_cpulists_builds_correct_mapping() {
        let topo =
            CpuTopology::from_node_cpulists(&[(0, "0-3".to_string()), (1, "4-7".to_string())]);
        assert_eq!(topo.numa_node(0), Some(0));
        assert_eq!(topo.numa_node(3), Some(0));
        assert_eq!(topo.numa_node(4), Some(1));
        assert_eq!(topo.numa_node(7), Some(1));
        assert_eq!(topo.node_count(), 2);
    }

    #[test]
    fn topology_returns_none_for_unknown_cpu() {
        let topo = CpuTopology::from_node_cpulists(&[(0, "0-1".to_string())]);
        assert_eq!(topo.numa_node(99), None);
        assert_eq!(topo.numa_node_or_zero(99), 0);
    }

    #[test]
    fn topology_handles_sparse_cpus() {
        let topo =
            CpuTopology::from_node_cpulists(&[(0, "0,8".to_string()), (1, "16".to_string())]);
        assert_eq!(topo.numa_node(0), Some(0));
        assert_eq!(topo.numa_node(8), Some(0));
        assert_eq!(topo.numa_node(16), Some(1));
        assert_eq!(topo.numa_node(4), None);
    }

    #[test]
    fn topology_ignores_unparseable_entries() {
        let topo =
            CpuTopology::from_node_cpulists(&[(0, "garbage,,".to_string()), (1, "5".to_string())]);
        assert_eq!(topo.numa_node(5), Some(1));
        assert_eq!(topo.numa_node(0), None);
    }

    #[test]
    fn empty_topology_is_safe() {
        let topo = CpuTopology::empty();
        assert!(topo.is_empty());
        assert_eq!(topo.numa_node(0), None);
        assert_eq!(topo.numa_node_or_zero(0), 0);
        assert_eq!(topo.node_count(), 0);
    }

    #[test]
    fn detect_on_missing_root_returns_empty() {
        let topo = CpuTopology::from_sysfs_root(Path::new("/this/path/should/not/exist/anywhere"));
        assert!(topo.is_empty());
    }

    #[test]
    fn from_sysfs_root_reads_synthetic_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for (node, cpus) in [(0u32, "0-1"), (1u32, "2,3")] {
            let dir = root.join(format!("node{node}"));
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("cpulist"), cpus).unwrap();
        }
        // A non-node directory should be ignored.
        fs::create_dir_all(root.join("possible")).unwrap();
        // A nodeN directory missing cpulist should be ignored.
        fs::create_dir_all(root.join("node9")).unwrap();

        let topo = CpuTopology::from_sysfs_root(root);
        assert_eq!(topo.numa_node(0), Some(0));
        assert_eq!(topo.numa_node(1), Some(0));
        assert_eq!(topo.numa_node(2), Some(1));
        assert_eq!(topo.numa_node(3), Some(1));
        assert_eq!(topo.node_count(), 2);
    }
}
