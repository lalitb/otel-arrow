// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Cold-path graph remapping and equality-preserving shard finalization.

use std::{collections::HashMap, hash::Hash};

use crate::{
    DropReason, FunctionId, Limits, LocationId, MappingId, ProcessId, ProfileSnapshot,
    ProfilerError, Result, StackId, ThreadId,
    limits::{reserve_index, reserve_row},
};

#[cfg(test)]
#[path = "finalize_tests.rs"]
mod tests;

pub(crate) fn merge_snapshots(
    snapshots: Vec<ProfileSnapshot>,
    limits: &Limits,
) -> Result<ProfileSnapshot> {
    let first = snapshots
        .first()
        .ok_or_else(|| ProfilerError::InternalInvariant("empty snapshot generation".to_owned()))?;
    let mut merged =
        ProfileSnapshot::empty(first.window_start, first.window_end, first.sampling_period);
    let mut processes = HashMap::new();
    let mut threads = HashMap::new();
    let mut mappings = HashMap::new();
    let mut functions = HashMap::new();
    let mut locations = HashMap::new();
    let mut stacks = HashMap::new();
    let mut samples = HashMap::new();
    let mut symbol_occupancy = 0u64;

    for snapshot in snapshots {
        snapshot.validate()?;
        validate_table_limits(&snapshot, limits)?;
        if snapshot.sampling_period != merged.sampling_period
            || snapshot.attributes != merged.attributes
        {
            return Err(ProfilerError::InternalInvariant(
                "shard periods differ".to_owned(),
            ));
        }
        merged.window_start = merged.window_start.min(snapshot.window_start);
        merged.window_end = merged.window_end.max(snapshot.window_end);
        let mut process_remap = Vec::new();
        process_remap
            .try_reserve_exact(snapshot.processes.len())
            .map_err(|_| ProfilerError::Allocation("process remap"))?;
        for row in snapshot.processes {
            let key = (row.pid, row.start_time_ticks);
            let id = if let Some(id) = processes.get(&key) {
                *id
            } else {
                room(merged.processes.len(), limits.max_processes)?;
                let id = ProcessId(local_id(merged.processes.len())?);
                reserve_row(&mut merged.processes, limits.max_processes)?;
                reserve_index(&mut processes)?;
                merged.processes.push(row);
                let _old = processes.insert(key, id);
                id
            };
            process_remap.push(id);
        }
        let mut thread_remap = Vec::new();
        thread_remap
            .try_reserve_exact(snapshot.threads.len())
            .map_err(|_| ProfilerError::Allocation("thread remap"))?;
        for mut row in snapshot.threads {
            row.process = remap(&process_remap, row.process.0)?;
            let key = (row.process, row.tid, row.start_time_ticks);
            let id = if let Some(id) = threads.get(&key) {
                *id
            } else {
                room(merged.threads.len(), limits.max_threads)?;
                let id = ThreadId(local_id(merged.threads.len())?);
                reserve_row(&mut merged.threads, limits.max_threads)?;
                reserve_index(&mut threads)?;
                merged.threads.push(row);
                let _old = threads.insert(key, id);
                id
            };
            thread_remap.push(id);
        }
        let mut mapping_remap = Vec::new();
        mapping_remap
            .try_reserve_exact(snapshot.mappings.len())
            .map_err(|_| ProfilerError::Allocation("mapping remap"))?;
        for mut row in snapshot.mappings {
            row.process = remap(&process_remap, row.process.0)?;
            mapping_remap.push(MappingId(intern(
                row,
                &mut mappings,
                &mut merged.mappings,
                limits.max_mappings,
            )?));
        }
        let mut function_remap = Vec::new();
        function_remap
            .try_reserve_exact(snapshot.functions.len())
            .map_err(|_| ProfilerError::Allocation("function remap"))?;
        for mut row in snapshot.functions {
            row.mapping = remap(&mapping_remap, row.mapping.0)?;
            function_remap.push(FunctionId(intern(
                row,
                &mut functions,
                &mut merged.functions,
                limits.max_functions,
            )?));
        }
        let mut location_remap = Vec::new();
        location_remap
            .try_reserve_exact(snapshot.locations.len())
            .map_err(|_| ProfilerError::Allocation("location remap"))?;
        for mut row in snapshot.locations {
            row.process = row
                .process
                .map(|id| remap(&process_remap, id.0))
                .transpose()?;
            row.mapping = row
                .mapping
                .map(|id| remap(&mapping_remap, id.0))
                .transpose()?;
            row.function = row
                .function
                .map(|id| remap(&function_remap, id.0))
                .transpose()?;
            location_remap.push(LocationId(intern(
                row,
                &mut locations,
                &mut merged.locations,
                limits.max_locations,
            )?));
        }
        let mut stack_remap = Vec::new();
        stack_remap
            .try_reserve_exact(snapshot.stacks.len())
            .map_err(|_| ProfilerError::Allocation("stack remap"))?;
        let stack_limit = limits
            .max_unique_user_stacks
            .checked_add(limits.max_unique_kernel_stacks)
            .ok_or_else(|| ProfilerError::invalid("limits", "stack capacity overflow"))?;
        for mut row in snapshot.stacks {
            row.process = row
                .process
                .map(|id| remap(&process_remap, id.0))
                .transpose()?;
            for location in &mut row.locations {
                *location = remap(&location_remap, location.0)?;
            }
            stack_remap.push(StackId(intern(
                row,
                &mut stacks,
                &mut merged.stacks,
                stack_limit,
            )?));
        }
        for mut row in snapshot.samples {
            row.process = remap(&process_remap, row.process.0)?;
            row.thread = remap(&thread_remap, row.thread.0)?;
            row.user_stack = remap(&stack_remap, row.user_stack.0)?;
            row.kernel_stack = row
                .kernel_stack
                .map(|id| remap(&stack_remap, id.0))
                .transpose()?;
            let key = (
                row.process,
                row.thread,
                row.user_stack,
                row.kernel_stack,
                row.cpu,
            );
            if let Some(index) = samples.get(&key).copied() {
                let existing: &mut crate::SampleRecord =
                    merged.samples.get_mut(index).ok_or_else(|| {
                        ProfilerError::InternalInvariant("sample remap index invalid".to_owned())
                    })?;
                existing.count = existing
                    .count
                    .checked_add(row.count)
                    .ok_or(ProfilerError::Capacity(DropReason::CounterOverflow))?;
                existing.first_timestamp_ns =
                    existing.first_timestamp_ns.min(row.first_timestamp_ns);
                existing.last_timestamp_ns = existing.last_timestamp_ns.max(row.last_timestamp_ns);
            } else {
                room(merged.samples.len(), limits.max_samples_per_snapshot)?;
                reserve_row(&mut merged.samples, limits.max_samples_per_snapshot)?;
                reserve_index(&mut samples)?;
                let index = merged.samples.len();
                merged.samples.push(row);
                let _old = samples.insert(key, index);
            }
        }
        symbol_occupancy =
            symbol_occupancy.saturating_add(snapshot.statistics.symbol_cache_occupancy);
        merged.statistics.merge(&snapshot.statistics);
        for detail in snapshot.error_details {
            if merged.error_details.len() >= limits.max_error_details {
                merged.statistics.error_details_dropped =
                    merged.statistics.error_details_dropped.saturating_add(1);
                break;
            }
            reserve_row(&mut merged.error_details, limits.max_error_details)?;
            merged.error_details.push(detail);
        }
    }
    merged.statistics.snapshots_created = 1;
    merged.statistics.active_processes = merged.processes.len() as u64;
    merged.statistics.active_threads = merged.threads.len() as u64;
    merged.statistics.mapping_cache_occupancy = merged.mappings.len() as u64;
    merged.statistics.symbol_cache_occupancy = symbol_occupancy;
    merged.statistics.samples_aggregated = merged
        .statistics
        .samples_accepted
        .saturating_sub(merged.samples.len() as u64);
    merged.statistics.unique_user_stacks =
        merged.stacks.iter().filter(|row| !row.kernel).count() as u64;
    merged.statistics.unique_kernel_stacks =
        merged.stacks.iter().filter(|row| row.kernel).count() as u64;
    merged.samples.sort_unstable_by_key(|row| {
        (
            row.process.0,
            row.thread.0,
            row.user_stack.0,
            row.kernel_stack.map(|id| id.0),
            row.cpu,
        )
    });
    merged.compact();
    if merged.logical_bytes() > limits.max_snapshot_bytes {
        return Err(ProfilerError::Capacity(DropReason::SnapshotCapacity));
    }
    merged.statistics.snapshot_logical_bytes = merged.logical_bytes() as u64;
    merged.validate()?;
    Ok(merged)
}

fn intern<T: Clone + Eq + Hash>(
    row: T,
    index: &mut HashMap<T, u32>,
    rows: &mut Vec<T>,
    maximum: usize,
) -> Result<u32> {
    if let Some(id) = index.get(&row) {
        return Ok(*id);
    }
    room(rows.len(), maximum)?;
    reserve_index(index)?;
    reserve_row(rows, maximum)?;
    let id = local_id(rows.len())?;
    let _old = index.insert(row.clone(), id);
    rows.push(row);
    Ok(id)
}

fn room(actual: usize, maximum: usize) -> Result<()> {
    if actual >= maximum {
        Err(ProfilerError::Capacity(DropReason::SnapshotCapacity))
    } else {
        Ok(())
    }
}

fn local_id(index: usize) -> Result<u32> {
    u32::try_from(index)
        .map_err(|_| ProfilerError::InternalInvariant("snapshot ID overflow".to_owned()))
}

fn remap<T: Copy>(values: &[T], index: u32) -> Result<T> {
    values
        .get(index as usize)
        .copied()
        .ok_or_else(|| ProfilerError::InternalInvariant("snapshot remap out of bounds".to_owned()))
}

fn validate_table_limits(snapshot: &ProfileSnapshot, limits: &Limits) -> Result<()> {
    for (actual, maximum) in [
        (snapshot.processes.len(), limits.max_processes),
        (snapshot.threads.len(), limits.max_threads),
        (snapshot.mappings.len(), limits.max_mappings),
        (snapshot.functions.len(), limits.max_functions),
        (snapshot.locations.len(), limits.max_locations),
        (
            snapshot.stacks.len(),
            limits.max_unique_user_stacks + limits.max_unique_kernel_stacks,
        ),
        (snapshot.samples.len(), limits.max_samples_per_snapshot),
        (snapshot.error_details.len(), limits.max_error_details),
    ] {
        if actual > maximum {
            return Err(ProfilerError::Capacity(DropReason::SnapshotCapacity));
        }
    }
    Ok(())
}
