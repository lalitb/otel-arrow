// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! CPU and NUMA topology models.

use crate::{ProfilerError, Result};

/// Stable NUMA node identifier.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NumaNodeId(pub u32);

/// One logical CPU discovered from the host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CpuInfo {
    /// Linux logical CPU identifier.
    pub id: u32,
    /// Whether the CPU is currently online.
    pub online: bool,
    /// NUMA node when reported by sysfs.
    pub numa_node: Option<NumaNodeId>,
}

/// Immutable CPU and NUMA topology snapshot used for planning.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SystemTopology {
    cpus: Vec<CpuInfo>,
}

impl SystemTopology {
    /// Creates a validated topology snapshot.
    pub fn new(mut cpus: Vec<CpuInfo>) -> Result<Self> {
        cpus.sort_unstable_by_key(|cpu| cpu.id);
        if cpus.is_empty() {
            return Err(ProfilerError::TopologyDiscovery(
                "no logical CPUs were discovered".to_owned(),
            ));
        }
        for pair in cpus.windows(2) {
            if pair[0].id == pair[1].id {
                return Err(ProfilerError::TopologyDiscovery(format!(
                    "duplicate logical CPU {}",
                    pair[0].id
                )));
            }
        }
        Ok(Self { cpus })
    }

    /// Returns every discovered logical CPU.
    #[must_use]
    pub fn cpus(&self) -> &[CpuInfo] {
        &self.cpus
    }

    /// Returns online logical CPU identifiers.
    #[must_use]
    pub fn online_cpu_ids(&self) -> Vec<u32> {
        self.cpus
            .iter()
            .filter(|cpu| cpu.online)
            .map(|cpu| cpu.id)
            .collect()
    }

    /// Returns one CPU record by identifier.
    #[must_use]
    pub fn cpu(&self, id: u32) -> Option<&CpuInfo> {
        self.cpus
            .binary_search_by_key(&id, |cpu| cpu.id)
            .ok()
            .map(|index| &self.cpus[index])
    }
}

/// Injectable topology discovery boundary.
pub trait TopologyProvider: Send + Sync {
    /// Discovers a stable topology snapshot.
    fn discover(&self) -> Result<SystemTopology>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: Topology input contains multiple records for one logical CPU.
    /// Guarantees: CPU assignment starts from a unique identity set.
    #[test]
    fn duplicate_cpu_is_rejected() {
        let cpus = vec![
            CpuInfo {
                id: 0,
                online: true,
                numa_node: None,
            },
            CpuInfo {
                id: 0,
                online: false,
                numa_node: None,
            },
        ];
        assert!(SystemTopology::new(cpus).is_err());
    }

    /// Scenario: Online and offline CPUs coexist in one topology snapshot.
    /// Guarantees: Default CPU selection excludes offline CPUs deterministically.
    #[test]
    fn online_cpu_ids_exclude_offline_cpus() {
        let topology = SystemTopology::new(vec![
            CpuInfo {
                id: 1,
                online: false,
                numa_node: None,
            },
            CpuInfo {
                id: 0,
                online: true,
                numa_node: None,
            },
        ])
        .expect("topology should be valid");
        assert_eq!(topology.online_cpu_ids(), vec![0]);
    }
}
