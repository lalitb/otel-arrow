// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Checked allocation-envelope arithmetic, separate from runtime probing.

use std::mem::size_of;

use crate::{Limits, ProfileSnapshot, ProfilerConfig, ProfilerError, RawSample, Result};

/// Conservative requested-memory envelope for the pinned Rust implementation.
/// Hash tables allow four slots per admitted entry, vectors two, and individual
/// allocations include bookkeeping allowance. Actual allocator fragmentation,
/// OS code pages, and consumer-retained snapshots are not controlled by this
/// library and are not an RSS guarantee.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryEstimate {
    /// Initialization peak before workers start: bounded kernel-BTF parsing,
    /// object-parser scratch, verifier retry buffers, and prepared maps/rings.
    pub initialization: usize,
    /// Selected CPU data rings plus their metadata pages (up to 64 KiB pages).
    pub perf_buffers: usize,
    /// Possible-CPU scratch/counter maps and the event-array descriptors.
    pub kernel_maps: usize,
    /// Collection/finalizer thread stacks and reusable decoded-event batches.
    pub workers: usize,
    /// All active shard aggregation tables, indexes, strings, and symbol caches.
    pub active_aggregation: usize,
    /// Frozen windows in queues or awaiting other shards; one shared permit
    /// budget covers both locations.
    pub pending_windows: usize,
    /// Merge tables, remaps, and output while frozen windows remain owned.
    pub finalization: usize,
    /// Completed snapshots waiting for ownership transfer to the consumer.
    pub completed_snapshots: usize,
    /// Concurrent bounded procfs/object reads, parsed metadata, loader scratch,
    /// and fixed control-plane state.
    pub scratch: usize,
}

impl MemoryEstimate {
    /// Maximum of initialization and running envelopes. The coordinator does
    /// not start workers until loader initialization has released its scratch.
    pub fn total_bytes(&self) -> Result<usize> {
        Ok(self.initialization.max(self.running_bytes()?))
    }

    /// Sum of the concurrently resident running categories.
    pub fn running_bytes(&self) -> Result<usize> {
        sum(&[
            self.perf_buffers,
            self.kernel_maps,
            self.workers,
            self.active_aggregation,
            self.pending_windows,
            self.finalization,
            self.completed_snapshots,
            self.scratch,
        ])
    }
}

pub(crate) fn sum(values: &[usize]) -> Result<usize> {
    values.iter().try_fold(0usize, |total, value| {
        total
            .checked_add(*value)
            .ok_or_else(|| ProfilerError::invalid("limits", "allocation envelope overflow"))
    })
}

pub(crate) fn product(left: usize, right: usize) -> Result<usize> {
    left.checked_mul(right)
        .filter(|bytes| *bytes <= isize::MAX as usize)
        .ok_or_else(|| ProfilerError::invalid("limits", "allocation envelope overflow"))
}

pub(crate) fn hash_bytes<K, V>(count: usize) -> Result<usize> {
    // Four slots conservatively cover the pinned SwissTable load factor,
    // power-of-two rounding, control bytes, and alignment.
    sum(&[
        product(
            count,
            product(4, sum(&[size_of::<K>(), size_of::<V>(), 32])?)?,
        )?,
        256,
    ])
}

pub(crate) fn vec_bytes<T>(count: usize) -> Result<usize> {
    sum(&[product(count, product(2, size_of::<T>())?)?, 64])
}

pub(crate) fn string_bytes(limits: &Limits, count: usize) -> Result<usize> {
    product(count, sum(&[limits.max_metadata_string_bytes, 64])?)
}

pub(crate) fn estimate(config: &ProfilerConfig) -> Result<MemoryEstimate> {
    let limits = &config.limits;
    limits.validate()?;
    config.program.validate()?;
    let aggregate = crate::aggregate::allocation_bound(limits)?;
    let symbol_cache = sum(&[
        product(
            limits.max_symbols,
            sum(&[128, product(2, limits.max_metadata_string_bytes)?])?,
        )?,
        product(limits.max_processes, sum(&[512, product(2, 4096)?])?)?,
    ])?;
    let active = sum(&[aggregate, product(2, symbol_cache)?])?;
    let snapshot = sum(&[
        product(2, limits.max_snapshot_bytes)?,
        product(limits.max_shards, size_of::<ProfileSnapshot>())?,
    ])?;
    let perf_buffers = product(
        limits.max_monitored_cpus,
        sum(&[limits.perf_buffer_bytes_per_cpu, 65_536])?,
    )?;
    let kernel_maps = sum(&[
        product(limits.max_possible_cpus, crate::ABI_EVENT_SIZE + 4096)?,
        product(limits.max_cpu_id as usize + 1, 32)?,
    ])?;
    let kernel_btf = sum(&[
        product(2, config.program.max_kernel_btf_bytes)?,
        // Contract tests constrain Aya's enum/member sizes to 128/32 bytes.
        // Three capacities cover vector reallocation; per-type bookkeeping
        // includes optional nested allocations and the temporary type header.
        product(config.program.max_kernel_btf_types, 3 * 128 + 64)?,
        product(config.program.max_kernel_btf_members, 3 * 32)?,
        65_536,
    ])?;
    let estimate = MemoryEstimate {
        initialization: sum(&[
            kernel_btf,
            32 * 1024 * 1024,
            product(32, config.program.max_object_bytes)?,
            perf_buffers,
            kernel_maps,
            product(limits.max_possible_cpus + limits.max_shards, 1024)?,
        ])?,
        perf_buffers,
        kernel_maps,
        workers: sum(&[
            product(
                limits.max_shards + 1,
                sum(&[limits.worker_stack_bytes, 65_536])?,
            )?,
            product(
                limits.max_shards,
                product(limits.max_raw_events_queued, size_of::<RawSample>())?,
            )?,
        ])?,
        active_aggregation: active,
        // Use the single-shard maximum, not max_snapshot_bytes/max_shards.
        // A user may choose Single on a machine with many allowed shards.
        pending_windows: product(limits.max_pending_shard_windows, active)?,
        finalization: sum(&[product(2, aggregate)?, snapshot])?,
        completed_snapshots: product(limits.max_pending_snapshots, snapshot)?,
        scratch: sum(&[
            product(
                limits.max_shards,
                sum(&[
                    product(2, limits.max_procfs_file_bytes)?,
                    limits.max_symbol_file_bytes,
                    65_536,
                ])?,
            )?,
            // One temporary metadata generation per worker; production
            // providers use disjoint per-shard mapping quotas.
            product(
                limits.max_mappings,
                sum(&[256, product(2, limits.max_metadata_string_bytes)?])?,
            )?,
            product(3, config.program.max_object_bytes)?,
            product(limits.max_shards + limits.max_possible_cpus, 1024)?,
        ])?,
    };
    let _checked_total = estimate.total_bytes()?;
    Ok(estimate)
}

pub(crate) fn reserve_row<T>(rows: &mut Vec<T>, maximum: usize) -> Result<()> {
    if rows.len() >= maximum {
        return Err(ProfilerError::Allocation("bounded table capacity"));
    }
    if rows.len() == rows.capacity() {
        let capacity = rows
            .capacity()
            .max(4)
            .checked_mul(2)
            .ok_or(ProfilerError::Allocation("table growth"))?
            .min(maximum);
        rows.try_reserve_exact(capacity - rows.len())
            .map_err(|_| ProfilerError::Allocation("table rows"))?;
    }
    Ok(())
}

pub(crate) fn reserve_index<K, V>(index: &mut std::collections::HashMap<K, V>) -> Result<()>
where
    K: std::hash::Hash + Eq,
{
    index
        .try_reserve(1)
        .map_err(|_| ProfilerError::Allocation("table index"))
}
