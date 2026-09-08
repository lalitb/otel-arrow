// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Injectable event-source and platform ownership boundaries.

use std::{collections::VecDeque, sync::Mutex, time::Duration};

use crate::{
    ProfilerError, ProfilerStatistics, RawSample, Result, ShardPlan, SystemTopology,
    ValidatedConfig,
};

/// Bounded result of one event-source drain.
#[derive(Clone, Debug, Default)]
pub struct EventBatch {
    /// Decoded samples, bounded by the caller-provided batch limit.
    pub samples: Vec<RawSample>,
    /// Kernel-reported events lost before userspace could read them.
    pub lost_events: u64,
    /// Malformed events drained and rejected.
    pub malformed_events: u64,
    /// Valid events drained after the caller's sample capacity was full.
    pub capacity_drops: u64,
    /// More events remain buffered after this bounded drain.
    pub has_more: bool,
}

impl EventBatch {
    /// Clears a batch without freeing its reusable sample storage.
    pub fn clear(&mut self) {
        self.samples.clear();
        self.lost_events = 0;
        self.malformed_events = 0;
        self.capacity_drops = 0;
        self.has_more = false;
    }
}

/// One shard's kernel or deterministic event source.
pub trait EventSource: Send {
    /// Clears and fills the caller's reusable batch. Implementations must
    /// return within `timeout` plus bounded copying work, retain at most
    /// `max_samples` samples, and stop draining after a bounded record count.
    /// `has_more` permits shutdown to drain buffered events without waiting.
    fn read_batch(
        &mut self,
        timeout: Duration,
        max_samples: usize,
        batch: &mut EventBatch,
    ) -> Result<()>;
}

/// Fixed startup coverage diagnostics returned after workers are ready.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlatformStart {
    /// Selected CPUs with active sampling attachments.
    pub attached_cpus: usize,
    /// Selected CPUs that failed in best-effort mode.
    pub unavailable_cpus: usize,
}

/// Owns loaded programs, maps, links, and other process-wide platform state.
pub trait PlatformGuard: Send {
    /// Enables sampling only after all workers have completed initialization.
    fn start_sampling(&mut self) -> Result<PlatformStart> {
        Ok(PlatformStart::default())
    }
    /// Stops new kernel sampling while leaving buffers available for bounded drain.
    fn stop_sampling(&mut self) -> Result<()>;
    /// Returns final kernel sampling counters after ingress is disabled.
    fn statistics(&mut self) -> Result<ProfilerStatistics> {
        Ok(ProfilerStatistics::default())
    }
    /// Releases remaining maps, programs, and platform resources.
    fn cleanup(&mut self) -> Result<()>;
}

/// Successfully prepared platform resources and one source per shard.
pub struct PreparedPlatform {
    /// Event sources in the same order as the requested shard plans.
    pub sources: Vec<Box<dyn EventSource>>,
    /// Process-wide resource owner.
    pub guard: Box<dyn PlatformGuard>,
}

impl std::fmt::Debug for PreparedPlatform {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedPlatform")
            .field("sources", &self.sources.len())
            .finish_non_exhaustive()
    }
}

/// Platform discovery, probing, loading, and attachment boundary.
pub trait Platform: Send + Sync {
    /// Discovers online CPU and NUMA topology.
    fn discover_topology(&self, config: &ValidatedConfig) -> Result<SystemTopology>;
    /// Loads bounded maps and buffers without enabling sampling. The returned
    /// guard must release all resources on drop, including partial startup.
    fn prepare(&self, config: &ValidatedConfig, shards: &[ShardPlan]) -> Result<PreparedPlatform>;
}

/// Deterministic bounded source used by non-privileged lifecycle tests.
#[derive(Debug)]
pub struct SyntheticEventSource {
    batches: VecDeque<EventBatch>,
}

impl SyntheticEventSource {
    /// Creates a source that returns batches in insertion order.
    #[must_use]
    pub fn new(batches: Vec<EventBatch>) -> Self {
        Self {
            batches: batches.into(),
        }
    }
}

impl EventSource for SyntheticEventSource {
    fn read_batch(
        &mut self,
        timeout: Duration,
        max_samples: usize,
        batch: &mut EventBatch,
    ) -> Result<()> {
        batch.clear();
        if max_samples == 0 {
            return Err(ProfilerError::invalid("max_samples", "must be non-zero"));
        }
        if let Some(front) = self.batches.front_mut() {
            let count = max_samples.min(front.samples.len());
            batch.samples.extend(front.samples.drain(..count));
            batch.lost_events = std::mem::take(&mut front.lost_events);
            batch.malformed_events = std::mem::take(&mut front.malformed_events);
            batch.capacity_drops = std::mem::take(&mut front.capacity_drops);
            if front.samples.is_empty() {
                let _completed = self.batches.pop_front();
            }
            batch.has_more = !self.batches.is_empty();
            return Ok(());
        }
        std::thread::sleep(timeout);
        Ok(())
    }
}

#[derive(Debug, Default)]
struct SyntheticGuard;

impl PlatformGuard for SyntheticGuard {
    fn stop_sampling(&mut self) -> Result<()> {
        Ok(())
    }

    fn cleanup(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Deterministic platform whose prepared sources are consumed once.
#[derive(Debug)]
pub struct SyntheticPlatform {
    topology: SystemTopology,
    sources: Mutex<Option<Vec<SyntheticEventSource>>>,
}

impl SyntheticPlatform {
    /// Creates a synthetic platform with one source definition per planned shard.
    #[must_use]
    pub fn new(topology: SystemTopology, sources: Vec<SyntheticEventSource>) -> Self {
        Self {
            topology,
            sources: Mutex::new(Some(sources)),
        }
    }
}

impl Platform for SyntheticPlatform {
    fn discover_topology(&self, _config: &ValidatedConfig) -> Result<SystemTopology> {
        Ok(self.topology.clone())
    }

    fn prepare(&self, _config: &ValidatedConfig, shards: &[ShardPlan]) -> Result<PreparedPlatform> {
        let mut sources = self
            .sources
            .lock()
            .map_err(|_| {
                ProfilerError::InternalInvariant("synthetic platform lock poisoned".to_owned())
            })?
            .take()
            .ok_or_else(|| {
                ProfilerError::PartialStartup(
                    "synthetic platform can only be prepared once".to_owned(),
                )
            })?;
        if sources.len() != shards.len() {
            return Err(ProfilerError::PartialStartup(format!(
                "synthetic source count {} does not match shard count {}",
                sources.len(),
                shards.len()
            )));
        }
        Ok(PreparedPlatform {
            sources: sources
                .drain(..)
                .map(|source| Box::new(source) as Box<dyn EventSource>)
                .collect(),
            guard: Box::new(SyntheticGuard),
        })
    }
}
