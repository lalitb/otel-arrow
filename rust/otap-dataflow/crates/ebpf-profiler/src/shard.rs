// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Stable CPU-to-shard planning.

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    CpuSelection, ProfilerError, Result, ShardingStrategy, SystemTopology, ValidatedConfig,
};

/// CPU assignment for one collection and aggregation worker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShardPlan {
    /// Stable zero-based shard identifier.
    pub id: usize,
    /// Disjoint sorted logical CPU identifiers.
    pub cpus: Vec<u32>,
    /// Preferred worker CPU, chosen from the shard's CPU set.
    pub affinity_cpu: u32,
}

/// Creates stable bounded shard plans.
#[derive(Debug, Default)]
pub struct ShardPlanner;

impl ShardPlanner {
    /// Resolves CPU selection and sharding against one topology snapshot.
    pub fn plan(topology: &SystemTopology, config: &ValidatedConfig) -> Result<Vec<ShardPlan>> {
        let config = config.get();
        if topology.cpus().len() > config.limits.max_possible_cpus
            || topology
                .cpus()
                .iter()
                .any(|cpu| cpu.id > config.limits.max_cpu_id)
        {
            return Err(ProfilerError::TopologyDiscovery(
                "CPU topology exceeds configured bounds".to_owned(),
            ));
        }
        let mut selected = match &config.cpu_selection {
            CpuSelection::AllOnline => topology.online_cpu_ids(),
            CpuSelection::Include(cpus) => {
                let mut selected = Vec::with_capacity(cpus.len());
                for cpu_id in cpus {
                    let cpu = topology.cpu(*cpu_id).ok_or_else(|| {
                        ProfilerError::CpuSelection(format!("CPU {cpu_id} was not discovered"))
                    })?;
                    if !cpu.online {
                        return Err(ProfilerError::CpuSelection(format!(
                            "CPU {cpu_id} is offline"
                        )));
                    }
                    selected.push(*cpu_id);
                }
                selected
            }
        };
        selected.sort_unstable();
        if selected.is_empty() {
            return Err(ProfilerError::CpuSelection(
                "selection resolved to no online CPUs".to_owned(),
            ));
        }
        if selected.len() > config.limits.max_monitored_cpus {
            return Err(ProfilerError::CpuSelection(format!(
                "{} selected CPUs exceeds limit {}",
                selected.len(),
                config.limits.max_monitored_cpus
            )));
        }

        let groups = match config.sharding {
            ShardingStrategy::Single => vec![selected.clone()],
            ShardingStrategy::PerNuma => group_by_numa(topology, &selected),
            ShardingStrategy::Fixed { workers } => group_fixed(&selected, workers.get()),
            ShardingStrategy::PerCore => {
                return Err(ProfilerError::invalid(
                    "sharding",
                    "per-core mode is not implemented",
                ));
            }
        };
        if groups.len() > config.limits.max_shards {
            return Err(ProfilerError::CpuSelection(format!(
                "{} planned shards exceeds limit {}",
                groups.len(),
                config.limits.max_shards
            )));
        }
        let plans: Vec<_> = groups
            .into_iter()
            .enumerate()
            .map(|(id, cpus)| ShardPlan {
                id,
                affinity_cpu: cpus[0],
                cpus,
            })
            .collect();
        validate_assignment(&selected, &plans)?;
        Ok(plans)
    }
}

fn group_by_numa(topology: &SystemTopology, selected: &[u32]) -> Vec<Vec<u32>> {
    let mut groups: BTreeMap<Option<u32>, Vec<u32>> = BTreeMap::new();
    for cpu_id in selected {
        let node = topology
            .cpu(*cpu_id)
            .and_then(|cpu| cpu.numa_node)
            .map(|node| node.0);
        groups.entry(node).or_default().push(*cpu_id);
    }
    groups.into_values().collect()
}

fn group_fixed(selected: &[u32], workers: usize) -> Vec<Vec<u32>> {
    let worker_count = workers.min(selected.len());
    let mut groups = vec![Vec::new(); worker_count];
    for (index, cpu_id) in selected.iter().enumerate() {
        groups[index % worker_count].push(*cpu_id);
    }
    groups
}

fn validate_assignment(selected: &[u32], plans: &[ShardPlan]) -> Result<()> {
    let expected: BTreeSet<_> = selected.iter().copied().collect();
    let mut assigned = BTreeSet::new();
    for plan in plans {
        for cpu in &plan.cpus {
            if !assigned.insert(*cpu) {
                return Err(ProfilerError::InternalInvariant(format!(
                    "CPU {cpu} assigned to multiple shards"
                )));
            }
        }
    }
    if assigned != expected {
        return Err(ProfilerError::InternalInvariant(
            "not every selected CPU was assigned exactly once".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use std::num::NonZeroUsize;

    use crate::{CpuInfo, NumaNodeId, ProfilerConfig};

    use super::*;

    fn topology() -> SystemTopology {
        SystemTopology::new(vec![
            CpuInfo {
                id: 0,
                online: true,
                numa_node: Some(NumaNodeId(0)),
            },
            CpuInfo {
                id: 1,
                online: true,
                numa_node: Some(NumaNodeId(0)),
            },
            CpuInfo {
                id: 2,
                online: true,
                numa_node: Some(NumaNodeId(1)),
            },
            CpuInfo {
                id: 3,
                online: true,
                numa_node: None,
            },
        ])
        .expect("topology should be valid")
    }

    /// Scenario: Single-shard mode monitors several CPUs.
    /// Guarantees: Every selected CPU is owned by exactly one worker.
    #[test]
    fn single_shard_assigns_every_cpu() {
        let mut config = ProfilerConfig::default();
        config.sharding = ShardingStrategy::Single;
        let config = config.validate().expect("config should validate");
        let plan = ShardPlanner::plan(&topology(), &config).expect("plan should succeed");
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].cpus, vec![0, 1, 2, 3]);
    }

    /// Scenario: Selected CPUs span known and unknown NUMA nodes.
    /// Guarantees: NUMA planning remains stable and never loses unknown-node CPUs.
    #[test]
    fn per_numa_groups_unknown_node_cpus() {
        let config = ProfilerConfig::default()
            .validate()
            .expect("config should validate");
        let plan = ShardPlanner::plan(&topology(), &config).expect("plan should succeed");
        assert_eq!(plan.len(), 3);
        assert_eq!(plan[0].cpus, vec![3]);
        assert_eq!(plan[1].cpus, vec![0, 1]);
        assert_eq!(plan[2].cpus, vec![2]);
    }

    /// Scenario: A fixed worker count is smaller than the selected CPU count.
    /// Guarantees: Round-robin assignment is deterministic and disjoint.
    #[test]
    fn fixed_workers_are_stable() {
        let mut config = ProfilerConfig::default();
        config.sharding = ShardingStrategy::Fixed {
            workers: NonZeroUsize::new(2).expect("2 is non-zero"),
        };
        let config = config.validate().expect("config should validate");
        let plan = ShardPlanner::plan(&topology(), &config).expect("plan should succeed");
        assert_eq!(plan[0].cpus, vec![0, 2]);
        assert_eq!(plan[1].cpus, vec![1, 3]);
    }

    /// Scenario: Explicit CPUs arrive in different configuration orders.
    /// Guarantees: Sorted topology assignment yields identical shard ownership.
    #[test]
    fn explicit_cpu_order_does_not_change_assignment() {
        let first = ProfilerConfig {
            cpu_selection: CpuSelection::Include(vec![3, 1, 0]),
            ..ProfilerConfig::default()
        }
        .validate()
        .expect("first config is valid");
        let second = ProfilerConfig {
            cpu_selection: CpuSelection::Include(vec![0, 1, 3]),
            ..ProfilerConfig::default()
        }
        .validate()
        .expect("second config is valid");
        assert_eq!(
            ShardPlanner::plan(&topology(), &first).expect("first plan"),
            ShardPlanner::plan(&topology(), &second).expect("second plan")
        );
    }

    /// Scenario: A later topology snapshot reports a selected CPU offline.
    /// Guarantees: Restart planning rejects explicit offline selection, rather
    /// than silently changing the requested CPU set.
    #[test]
    fn offline_hotplug_snapshot_is_rejected_for_explicit_selection() {
        let changed = SystemTopology::new(vec![
            CpuInfo {
                id: 0,
                online: true,
                numa_node: None,
            },
            CpuInfo {
                id: 1,
                online: false,
                numa_node: None,
            },
        ])
        .expect("snapshot is valid");
        let config = ProfilerConfig {
            cpu_selection: CpuSelection::Include(vec![1]),
            ..ProfilerConfig::default()
        }
        .validate()
        .expect("static selection is valid");
        assert!(ShardPlanner::plan(&changed, &config).is_err());
    }
}
