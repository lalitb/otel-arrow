// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Controller-owned runtime placement metadata.

use core_affinity::CoreId;
use otap_df_config::policy::LoadBalancingPolicy;
use otap_df_config::{PipelineGroupId, PipelineId};
use otap_df_engine::topology::{NumaTopology, TopologyCompleteness};

/// Stable placement snapshot for a controller deployment generation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlacementSnapshot {
    /// Monotonic placement generation. Startup uses generation `0`.
    pub generation: u64,
    /// Per-pipeline placements in controller launch order.
    pub pipelines: Vec<PipelinePlacement>,
}

impl PlacementSnapshot {
    /// Creates a placement snapshot from resolved per-pipeline assignments.
    #[must_use]
    pub fn from_assignments(generation: u64, pipelines: Vec<PipelinePlacement>) -> Self {
        Self {
            generation,
            pipelines,
        }
    }
}

/// Resolved placement for one logical pipeline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PipelinePlacement {
    /// Pipeline group id.
    pub pipeline_group_id: PipelineGroupId,
    /// Pipeline id.
    pub pipeline_id: PipelineId,
    /// Load-balancing policy resolved for this pipeline.
    pub load_balancing: LoadBalancingPolicy,
    /// Assigned cores with NUMA metadata.
    pub cores: Vec<CorePlacement>,
}

impl PipelinePlacement {
    /// Returns the number of worker cores in this pipeline placement.
    #[must_use]
    pub fn core_count(&self) -> usize {
        self.cores.len()
    }
}

/// Resolved placement for one pipeline worker core.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CorePlacement {
    /// CPU core selected by the controller.
    pub core_id: CoreId,
    /// NUMA node for `core_id`; unknown topology falls back to `0`.
    pub numa_node: u32,
    /// Completeness of the topology used for this placement.
    pub topology_completeness: TopologyCompleteness,
}

impl CorePlacement {
    /// Creates a core placement using the controller-owned topology snapshot.
    #[must_use]
    pub fn from_core_id(core_id: CoreId, topology: &NumaTopology) -> Self {
        Self {
            core_id,
            numa_node: topology.numa_node_or_zero(core_id.id as u32),
            topology_completeness: topology.completeness(),
        }
    }
}
