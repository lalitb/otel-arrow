// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Bounded shard-local interning, sample admission, and owned window finalization.

use std::{
    collections::HashMap,
    mem::size_of,
    sync::Arc,
    time::{Duration, SystemTime},
};

use crate::{
    DropReason, EventFlags, FunctionId, FunctionRecord, Limits, LocationId, LocationRecord,
    MappingId, MappingRecord, ProcessId, ProcessIdentity, ProcessMetadata, ProcessProvider,
    ProcessRecord, ProfileSnapshot, ProfilerError, ProfilerStatistics, RawSample, Result,
    SampleRecord, StackId, StackRecord, SymbolResolver, ThreadId, ThreadRecord,
    error::bounded_message,
    limits::{hash_bytes, product, reserve_index, reserve_row, string_bytes, sum, vec_bytes},
    mappings::truncate_utf8,
};

#[derive(Debug)]
struct ProcessState {
    id: ProcessId,
    identity: ProcessIdentity,
    // References into the window-owned mapping table; no second copy of paths
    // or complete procfs mapping generations is retained on the hot path.
    mappings: Vec<(MappingId, bool)>,
    refreshed_ns: u64,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct LocationKey {
    process: Option<ProcessId>,
    mapping: Option<MappingId>,
    address: u64,
    kernel: bool,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct StackKey {
    process: Option<ProcessId>,
    kernel: bool,
    depth: usize,
    locations: [LocationId; crate::MAX_ABI_STACK_DEPTH],
    truncated: bool,
    error: Option<i32>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct SampleKey {
    process: ProcessId,
    thread: ThreadId,
    user_stack: StackId,
    kernel_stack: Option<StackId>,
    cpu: u32,
}

/// One bounded aggregation generation. All references become snapshot-local
/// owned values; caches and indexes never escape through the consumer API.
pub struct AggregationWindow {
    limits: Limits,
    snapshot: ProfileSnapshot,
    logical_bytes: usize,
    process_provider: Arc<dyn ProcessProvider>,
    symbol_resolver: Box<dyn SymbolResolver>,
    current_processes: HashMap<u32, Arc<ProcessState>>,
    processes: HashMap<ProcessIdentity, ProcessId>,
    threads: HashMap<(ProcessId, u32, u64), ThreadId>,
    mappings: HashMap<MappingRecord, MappingId>,
    functions: HashMap<(MappingId, u64), FunctionId>,
    locations: HashMap<LocationKey, LocationId>,
    stacks: HashMap<StackKey, StackId>,
    samples: HashMap<SampleKey, usize>,
    user_stack_count: usize,
    kernel_stack_count: usize,
}

impl std::fmt::Debug for AggregationWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AggregationWindow")
            .field("samples", &self.snapshot.samples.len())
            .field("logical_bytes", &self.logical_bytes)
            .finish_non_exhaustive()
    }
}

impl AggregationWindow {
    /// Creates an empty window after checking its limits. Tables grow only on
    /// admission, never eagerly in proportion to a caller-supplied capacity.
    pub fn new(
        limits: Limits,
        sampling_period: Duration,
        window_start: SystemTime,
        process_provider: Arc<dyn ProcessProvider>,
        symbol_resolver: Box<dyn SymbolResolver>,
    ) -> Result<Self> {
        limits.validate()?;
        if sampling_period.is_zero() {
            return Err(ProfilerError::invalid(
                "sampling_period",
                "must be non-zero",
            ));
        }
        if allocation_bound(&limits)? > limits.max_total_memory_bytes {
            return Err(ProfilerError::invalid(
                "limits",
                "aggregation exceeds memory budget",
            ));
        }
        let snapshot = ProfileSnapshot::empty(window_start, window_start, sampling_period);
        Ok(Self {
            logical_bytes: snapshot.logical_bytes(),
            snapshot,
            limits,
            process_provider,
            symbol_resolver,
            current_processes: HashMap::new(),
            processes: HashMap::new(),
            threads: HashMap::new(),
            mappings: HashMap::new(),
            functions: HashMap::new(),
            locations: HashMap::new(),
            stacks: HashMap::new(),
            samples: HashMap::new(),
            user_stack_count: 0,
            kernel_stack_count: 0,
        })
    }

    /// Admits an event or records exactly one userspace sample-drop reason.
    /// Optional mapping/symbol omissions are diagnostics, not dropped samples.
    pub fn record(&mut self, sample: &RawSample) -> Result<bool> {
        self.snapshot.statistics.raw_events_received = self
            .snapshot
            .statistics
            .raw_events_received
            .saturating_add(1);
        if let Err(error) = sample.validate() {
            self.snapshot
                .statistics
                .record_drop(DropReason::MalformedEvent, 1);
            self.record_error(&error);
            return Ok(false);
        }
        if sample.flags.contains(EventFlags::USER_STACK_ERROR) {
            self.snapshot.statistics.user_stack_failures = self
                .snapshot
                .statistics
                .user_stack_failures
                .saturating_add(1);
        }
        if sample.flags.contains(EventFlags::KERNEL_STACK_ERROR) {
            self.snapshot.statistics.kernel_stack_failures = self
                .snapshot
                .statistics
                .kernel_stack_failures
                .saturating_add(1);
        }
        if sample
            .flags
            .intersects(EventFlags::USER_TRUNCATED | EventFlags::KERNEL_TRUNCATED)
            || usize::from(sample.user_depth) > self.limits.max_stack_depth
            || usize::from(sample.kernel_depth) > self.limits.max_stack_depth
        {
            self.snapshot.statistics.truncated_stacks =
                self.snapshot.statistics.truncated_stacks.saturating_add(1);
        }
        match self.record_inner(sample) {
            Ok(()) => {
                self.snapshot.statistics.samples_accepted =
                    self.snapshot.statistics.samples_accepted.saturating_add(1);
                Ok(true)
            }
            Err(ProfilerError::Capacity(reason)) => {
                self.snapshot.statistics.record_drop(reason, 1);
                Ok(false)
            }
            Err(error @ ProfilerError::StaleProcess(_)) => {
                self.snapshot
                    .statistics
                    .record_drop(DropReason::StaleProcess, 1);
                self.record_error(&error);
                Ok(false)
            }
            Err(error @ (ProfilerError::ProcessMetadata { .. } | ProfilerError::Io { .. })) => {
                self.snapshot.statistics.process_lookup_failures = self
                    .snapshot
                    .statistics
                    .process_lookup_failures
                    .saturating_add(1);
                self.snapshot
                    .statistics
                    .record_drop(DropReason::MetadataUnavailable, 1);
                self.record_error(&error);
                Ok(false)
            }
            Err(error) => {
                self.snapshot
                    .statistics
                    .record_drop(DropReason::SourceFailure, 1);
                Err(error)
            }
        }
    }

    fn record_inner(&mut self, sample: &RawSample) -> Result<()> {
        let process = self.ensure_process(sample.pid, sample.timestamp_ns)?;
        let metadata = self.process_provider.thread(process.identity, sample.tid)?;
        if metadata.process != process.identity
            || metadata.tid != sample.tid
            || !self.process_provider.sample_matches(
                ProcessIdentity {
                    pid: metadata.tid,
                    start_time_ticks: metadata.start_time_ticks,
                },
                sample.timestamp_ns,
            )?
        {
            return Err(ProfilerError::StaleProcess(sample.pid));
        }
        self.record_issues(&metadata.issues);
        let thread_key = (process.id, sample.tid, metadata.start_time_ticks);
        let thread = if let Some(id) = self.threads.get(&thread_key) {
            *id
        } else {
            self.require_room(
                self.snapshot.threads.len(),
                self.limits.max_threads,
                DropReason::ThreadCapacity,
            )?;
            let name = metadata
                .name
                .as_deref()
                .map(|name| truncate_utf8(name, self.limits.max_metadata_string_bytes));
            self.charge(size_of::<ThreadRecord>() + name.as_ref().map_or(0, String::len))?;
            let id = ThreadId(local_id(self.snapshot.threads.len())?);
            reserve_row(&mut self.snapshot.threads, self.limits.max_threads)?;
            reserve_index(&mut self.threads)?;
            self.snapshot.threads.push(ThreadRecord {
                process: process.id,
                tid: sample.tid,
                start_time_ticks: metadata.start_time_ticks,
                name,
            });
            let _old = self.threads.insert(thread_key, id);
            id
        };

        let user_stack = self.intern_stack(&process, sample, false)?;
        let kernel_stack =
            if sample.kernel_depth != 0 || sample.flags.contains(EventFlags::KERNEL_STACK_ERROR) {
                Some(self.intern_stack(&process, sample, true)?)
            } else {
                None
            };
        let key = SampleKey {
            process: process.id,
            thread,
            user_stack,
            kernel_stack,
            cpu: sample.cpu,
        };
        if let Some(index) = self.samples.get(&key) {
            let row = &mut self.snapshot.samples[*index];
            row.count = row
                .count
                .checked_add(1)
                .ok_or(ProfilerError::Capacity(DropReason::CounterOverflow))?;
            row.first_timestamp_ns = row.first_timestamp_ns.min(sample.timestamp_ns);
            row.last_timestamp_ns = row.last_timestamp_ns.max(sample.timestamp_ns);
            self.snapshot.statistics.samples_aggregated = self
                .snapshot
                .statistics
                .samples_aggregated
                .saturating_add(1);
            return Ok(());
        }
        self.require_room(
            self.samples.len(),
            self.limits.max_aggregation_keys,
            DropReason::AggregationCapacity,
        )?;
        self.require_room(
            self.snapshot.samples.len(),
            self.limits.max_samples_per_snapshot,
            DropReason::SnapshotCapacity,
        )?;
        self.charge(size_of::<SampleRecord>())?;
        reserve_row(
            &mut self.snapshot.samples,
            self.limits.max_samples_per_snapshot,
        )?;
        reserve_index(&mut self.samples)?;
        let index = self.snapshot.samples.len();
        self.snapshot.samples.push(SampleRecord {
            process: process.id,
            thread,
            user_stack,
            kernel_stack,
            count: 1,
            first_timestamp_ns: sample.timestamp_ns,
            last_timestamp_ns: sample.timestamp_ns,
            cpu: sample.cpu,
        });
        let _old = self.samples.insert(key, index);
        Ok(())
    }

    fn ensure_process(&mut self, pid: u32, timestamp_ns: u64) -> Result<Arc<ProcessState>> {
        let identity = self.process_provider.identity(pid)?;
        if identity.pid != pid
            || !self
                .process_provider
                .sample_matches(identity, timestamp_ns)?
        {
            return Err(ProfilerError::StaleProcess(pid));
        }
        if let Some(current) = self.current_processes.get(&pid)
            && current.identity == identity
            && timestamp_ns.saturating_sub(current.refreshed_ns) < 1_000_000_000
        {
            return Ok(Arc::clone(current));
        }
        let metadata = self.process_provider.process(pid)?;
        if metadata.identity != identity {
            return Err(ProfilerError::StaleProcess(pid));
        }
        self.record_issues(&metadata.issues);
        let id = if let Some(id) = self.processes.get(&identity) {
            *id
        } else {
            self.require_room(
                self.processes.len(),
                self.limits.max_processes,
                DropReason::ProcessCapacity,
            )?;
            let name = truncate_utf8(&metadata.name, self.limits.max_metadata_string_bytes);
            let executable = metadata
                .executable
                .as_deref()
                .map(|path| truncate_utf8(path, self.limits.max_metadata_string_bytes));
            self.charge(
                size_of::<ProcessRecord>()
                    + name.len()
                    + executable.as_ref().map_or(0, String::len),
            )?;
            let id = ProcessId(local_id(self.snapshot.processes.len())?);
            reserve_row(&mut self.snapshot.processes, self.limits.max_processes)?;
            reserve_index(&mut self.processes)?;
            self.snapshot.processes.push(ProcessRecord {
                pid,
                start_time_ticks: identity.start_time_ticks,
                name,
                executable,
            });
            let _old = self.processes.insert(identity, id);
            id
        };
        let state = Arc::new(self.process_state(id, metadata, timestamp_ns)?);
        reserve_index(&mut self.current_processes)?;
        let _old = self.current_processes.insert(pid, Arc::clone(&state));
        Ok(state)
    }

    fn process_state(
        &mut self,
        process: ProcessId,
        metadata: ProcessMetadata,
        now_ns: u64,
    ) -> Result<ProcessState> {
        let mut mappings = Vec::new();
        for mapping in metadata
            .mappings
            .as_slice()
            .iter()
            .take(self.limits.max_mappings)
        {
            let path_fits = mapping
                .path
                .as_ref()
                .is_none_or(|path| path.len() <= self.limits.max_metadata_string_bytes);
            if !path_fits {
                self.snapshot.statistics.metadata_omissions = self
                    .snapshot
                    .statistics
                    .metadata_omissions
                    .saturating_add(1);
            }
            let row = MappingRecord {
                process,
                start: mapping.start,
                end: mapping.end,
                file_offset: mapping.file_offset,
                path: if path_fits {
                    mapping.path.clone()
                } else {
                    None
                },
                executable: true,
                kind: mapping.kind,
                file_identity: mapping.file_identity(),
                path_truncated: mapping.path_truncated || !path_fits,
            };
            let symbolizable = path_fits && mapping.symbol_path().is_some();
            let id = if let Some(id) = self.mappings.get(&row) {
                *id
            } else {
                let bytes = size_of::<MappingRecord>() + row.path.as_ref().map_or(0, String::len);
                if self.snapshot.mappings.len() >= self.limits.max_mappings || !self.fits(bytes) {
                    self.snapshot.statistics.capacity_rejections = self
                        .snapshot
                        .statistics
                        .capacity_rejections
                        .saturating_add(1);
                    self.snapshot.statistics.metadata_omissions = self
                        .snapshot
                        .statistics
                        .metadata_omissions
                        .saturating_add(1);
                    continue;
                }
                self.charge(bytes)?;
                let id = MappingId(local_id(self.snapshot.mappings.len())?);
                reserve_row(&mut self.snapshot.mappings, self.limits.max_mappings)?;
                reserve_index(&mut self.mappings)?;
                let _old = self.mappings.insert(row.clone(), id);
                self.snapshot.mappings.push(row);
                id
            };
            reserve_row(&mut mappings, self.limits.max_mappings)?;
            mappings.push((id, symbolizable));
        }
        Ok(ProcessState {
            id: process,
            identity: metadata.identity,
            mappings,
            refreshed_ns: now_ns,
        })
    }

    fn intern_stack(
        &mut self,
        process: &ProcessState,
        sample: &RawSample,
        kernel: bool,
    ) -> Result<StackId> {
        let (frames, truncated_flag, error_flag, code) = if kernel {
            (
                sample.kernel_stack(),
                EventFlags::KERNEL_TRUNCATED,
                EventFlags::KERNEL_STACK_ERROR,
                sample.kernel_stack_error,
            )
        } else {
            (
                sample.user_stack(),
                EventFlags::USER_TRUNCATED,
                EventFlags::USER_STACK_ERROR,
                sample.user_stack_error,
            )
        };
        let mut key = StackKey {
            process: (!kernel).then_some(process.id),
            kernel,
            depth: frames.len().min(self.limits.max_stack_depth),
            locations: [LocationId(0); crate::MAX_ABI_STACK_DEPTH],
            truncated: sample.flags.contains(truncated_flag)
                || frames.len() > self.limits.max_stack_depth,
            error: sample.flags.contains(error_flag).then_some(code),
        };
        for (slot, address) in key.locations[..key.depth].iter_mut().zip(frames) {
            *slot = self.intern_location(process, *address, kernel)?;
        }
        if let Some(stack) = self.stacks.get(&key) {
            return Ok(*stack);
        }
        let (count, maximum, reason) = if kernel {
            (
                self.kernel_stack_count,
                self.limits.max_unique_kernel_stacks,
                DropReason::KernelStackCapacity,
            )
        } else {
            (
                self.user_stack_count,
                self.limits.max_unique_user_stacks,
                DropReason::UserStackCapacity,
            )
        };
        self.require_room(count, maximum, reason)?;
        self.charge(size_of::<StackRecord>() + key.depth * size_of::<LocationId>())?;
        let max_stacks = self.limits.max_unique_user_stacks + self.limits.max_unique_kernel_stacks;
        let id = StackId(local_id(self.snapshot.stacks.len())?);
        reserve_row(&mut self.snapshot.stacks, max_stacks)?;
        reserve_index(&mut self.stacks)?;
        let mut locations = Vec::new();
        locations
            .try_reserve_exact(key.depth)
            .map_err(|_| ProfilerError::Allocation("stack frames"))?;
        locations.extend_from_slice(&key.locations[..key.depth]);
        self.snapshot.stacks.push(StackRecord {
            process: key.process,
            kernel,
            locations,
            truncated: key.truncated,
            collection_error: key.error,
        });
        let _old = self.stacks.insert(key, id);
        if kernel {
            self.kernel_stack_count += 1;
        } else {
            self.user_stack_count += 1;
        }
        Ok(id)
    }

    fn intern_location(
        &mut self,
        process: &ProcessState,
        address: u64,
        kernel: bool,
    ) -> Result<LocationId> {
        let found = if kernel {
            None
        } else {
            let index = process
                .mappings
                .partition_point(|(id, _)| self.snapshot.mappings[id.0 as usize].start <= address);
            index
                .checked_sub(1)
                .and_then(|index| process.mappings.get(index))
                .copied()
                .filter(|(id, _)| address < self.snapshot.mappings[id.0 as usize].end)
        };
        let (mapping, normalized, symbolizable) = match found {
            Some((id, symbolizable)) => {
                let row = &self.snapshot.mappings[id.0 as usize];
                let normalized = address
                    .checked_sub(row.start)
                    .and_then(|offset| offset.checked_add(row.file_offset))
                    .ok_or_else(|| {
                        ProfilerError::InternalInvariant("mapping address overflow".to_owned())
                    })?;
                (Some(id), normalized, symbolizable)
            }
            None => (None, address, false),
        };
        let key = LocationKey {
            process: (!kernel).then_some(process.id),
            mapping,
            address: normalized,
            kernel,
        };
        if let Some(id) = self.locations.get(&key) {
            return Ok(*id);
        }
        self.require_room(
            self.locations.len(),
            self.limits.max_locations,
            DropReason::LocationCapacity,
        )?;
        self.charge(size_of::<LocationRecord>())?;
        let function = if symbolizable {
            let id = mapping.ok_or_else(|| {
                ProfilerError::InternalInvariant("symbol mapping missing".to_owned())
            })?;
            self.resolve_function(process.identity.pid, id, normalized)?
        } else {
            if !kernel && mapping.is_none() {
                self.snapshot.statistics.mapping_lookup_failures = self
                    .snapshot
                    .statistics
                    .mapping_lookup_failures
                    .saturating_add(1);
            }
            None
        };
        let id = LocationId(local_id(self.snapshot.locations.len())?);
        reserve_row(&mut self.snapshot.locations, self.limits.max_locations)?;
        reserve_index(&mut self.locations)?;
        self.snapshot.locations.push(LocationRecord {
            process: key.process,
            mapping,
            address: normalized,
            function,
            kernel,
        });
        let _old = self.locations.insert(key, id);
        Ok(id)
    }

    fn resolve_function(
        &mut self,
        pid: u32,
        mapping: MappingId,
        address: u64,
    ) -> Result<Option<FunctionId>> {
        let Some(path) = self.snapshot.mappings[mapping.0 as usize].path.as_deref() else {
            return Ok(None);
        };
        let path = self
            .process_provider
            .symbol_path(pid, std::path::Path::new(path));
        let identity = self.snapshot.mappings[mapping.0 as usize].file_identity;
        let symbol = match self
            .symbol_resolver
            .resolve_backing(&path, address, identity)
        {
            Ok(Some(symbol)) => symbol,
            Ok(None) => {
                self.snapshot.statistics.symbol_lookup_failures = self
                    .snapshot
                    .statistics
                    .symbol_lookup_failures
                    .saturating_add(1);
                return Ok(None);
            }
            Err(error) => {
                self.snapshot.statistics.symbol_lookup_failures = self
                    .snapshot
                    .statistics
                    .symbol_lookup_failures
                    .saturating_add(1);
                self.record_error(&error);
                return Ok(None);
            }
        };
        let start = address.checked_sub(symbol.offset).ok_or_else(|| {
            ProfilerError::InternalInvariant("symbol offset exceeds address".to_owned())
        })?;
        if let Some(id) = self.functions.get(&(mapping, start)) {
            return Ok(Some(*id));
        }
        let name = truncate_utf8(&symbol.name, self.limits.max_metadata_string_bytes);
        let bytes = size_of::<FunctionRecord>() + name.len();
        if self.functions.len() >= self.limits.max_functions || !self.fits(bytes) {
            self.snapshot.statistics.capacity_rejections = self
                .snapshot
                .statistics
                .capacity_rejections
                .saturating_add(1);
            self.snapshot.statistics.metadata_omissions = self
                .snapshot
                .statistics
                .metadata_omissions
                .saturating_add(1);
            return Ok(None);
        }
        self.charge(bytes)?;
        let id = FunctionId(local_id(self.snapshot.functions.len())?);
        reserve_row(&mut self.snapshot.functions, self.limits.max_functions)?;
        reserve_index(&mut self.functions)?;
        self.snapshot.functions.push(FunctionRecord {
            mapping,
            address: start,
            name,
            filename: None,
            line: None,
        });
        let _old = self.functions.insert((mapping, start), id);
        Ok(Some(id))
    }

    fn record_issues(&mut self, issues: &crate::MetadataIssues) {
        let omitted = [
            issues.executable_unavailable,
            issues.mappings_unavailable,
            issues.mappings_truncated,
            issues.strings_truncated,
            issues.name_unavailable,
        ]
        .into_iter()
        .filter(|issue| *issue)
        .count() as u64;
        self.snapshot.statistics.metadata_omissions = self
            .snapshot
            .statistics
            .metadata_omissions
            .saturating_add(omitted);
        if issues.mappings_unavailable {
            self.snapshot.statistics.mapping_lookup_failures = self
                .snapshot
                .statistics
                .mapping_lookup_failures
                .saturating_add(1);
        }
        if issues.mappings_truncated || issues.strings_truncated {
            self.snapshot.statistics.capacity_rejections = self
                .snapshot
                .statistics
                .capacity_rejections
                .saturating_add(1);
        }
    }

    fn require_room(&self, count: usize, max: usize, reason: DropReason) -> Result<()> {
        if count >= max {
            Err(ProfilerError::Capacity(reason))
        } else {
            Ok(())
        }
    }

    fn fits(&self, bytes: usize) -> bool {
        self.logical_bytes
            .checked_add(bytes)
            .is_some_and(|total| total <= self.limits.max_snapshot_bytes)
    }

    fn charge(&mut self, bytes: usize) -> Result<()> {
        if !self.fits(bytes) {
            return Err(ProfilerError::Capacity(DropReason::SnapshotCapacity));
        }
        self.logical_bytes += bytes;
        Ok(())
    }

    fn record_error(&mut self, error: &ProfilerError) {
        if self.snapshot.error_details.len() >= self.limits.max_error_details {
            self.snapshot.statistics.error_details_dropped = self
                .snapshot
                .statistics
                .error_details_dropped
                .saturating_add(1);
            return;
        }
        let message = bounded_message(error, self.limits.max_metadata_string_bytes);
        let bytes = size_of::<String>() + message.len();
        if self.fits(bytes) {
            self.logical_bytes += bytes;
            self.snapshot.error_details.push(message);
        } else {
            self.snapshot.statistics.error_details_dropped = self
                .snapshot
                .statistics
                .error_details_dropped
                .saturating_add(1);
        }
    }

    /// Records bounded transport losses separately from decoded sample admission.
    pub fn record_source_loss(&mut self, kernel_lost: u64, malformed: u64, capacity_drops: u64) {
        let statistics = &mut self.snapshot.statistics;
        statistics.kernel_lost_events = statistics.kernel_lost_events.saturating_add(kernel_lost);
        statistics.raw_events_received = statistics
            .raw_events_received
            .saturating_add(malformed)
            .saturating_add(capacity_drops);
        statistics.record_drop(DropReason::KernelBufferFull, kernel_lost);
        statistics.record_drop(DropReason::MalformedEvent, malformed);
        statistics.record_drop(DropReason::RawEventCapacity, capacity_drops);
    }

    /// Counts samples intentionally discarded while draining after a deadline.
    pub fn discard(&mut self, count: u64, reason: DropReason) {
        self.snapshot.statistics.raw_events_received = self
            .snapshot
            .statistics
            .raw_events_received
            .saturating_add(count);
        self.snapshot.statistics.record_drop(reason, count);
    }

    pub(crate) fn record_wakeup(&mut self) {
        self.snapshot.statistics.worker_wakeups =
            self.snapshot.statistics.worker_wakeups.saturating_add(1);
    }

    pub(crate) fn record_latency(&mut self, nanos: u64) {
        self.snapshot.statistics.event_to_aggregation_ns_max = self
            .snapshot
            .statistics
            .event_to_aggregation_ns_max
            .max(Some(nanos));
    }

    /// Current counters and bounded table/cache occupancy.
    #[must_use]
    pub fn statistics(&self) -> ProfilerStatistics {
        let mut stats = self.snapshot.statistics.clone();
        stats.active_processes = self.snapshot.processes.len() as u64;
        stats.active_threads = self.snapshot.threads.len() as u64;
        stats.unique_user_stacks = self.user_stack_count as u64;
        stats.unique_kernel_stacks = self.kernel_stack_count as u64;
        stats.mapping_cache_occupancy = self.snapshot.mappings.len() as u64;
        let symbols = self.symbol_resolver.statistics();
        stats.symbol_cache_occupancy = symbols.cache_entries as u64;
        stats.symbol_cache_hits = symbols.cache_hits;
        stats.cache_evictions = symbols.evictions;
        stats.capacity_rejections = stats
            .capacity_rejections
            .saturating_add(symbols.capacity_rejections);
        stats
    }

    /// Moves window data by ownership. Sorting, compaction and graph validation
    /// run on the finalizer, not on the CPU-buffer drain loop.
    pub fn finish(mut self, window_end: SystemTime) -> Result<ProfileSnapshot> {
        self.snapshot.statistics = self.statistics();
        self.snapshot.window_end = window_end;
        self.snapshot.samples.sort_unstable_by_key(|row| {
            (
                row.process.0,
                row.thread.0,
                row.user_stack.0,
                row.kernel_stack.map(|id| id.0),
                row.cpu,
            )
        });
        self.snapshot.statistics.samples_in_snapshots =
            self.snapshot.samples.iter().try_fold(0u64, |total, row| {
                total
                    .checked_add(row.count)
                    .ok_or(ProfilerError::Capacity(DropReason::CounterOverflow))
            })?;
        self.snapshot.statistics.snapshots_created = 1;
        self.snapshot.compact();
        let bytes = self.snapshot.logical_bytes();
        if bytes > self.limits.max_snapshot_bytes {
            return Err(ProfilerError::InternalInvariant(
                "snapshot exceeded admission budget".to_owned(),
            ));
        }
        self.snapshot.statistics.snapshot_logical_bytes = bytes as u64;
        self.snapshot.validate()?;
        Ok(self.snapshot)
    }
}

fn local_id(index: usize) -> Result<u32> {
    u32::try_from(index)
        .map_err(|_| ProfilerError::InternalInvariant("local ID overflow".to_owned()))
}

pub(crate) fn allocation_bound(limits: &Limits) -> Result<usize> {
    let stack_count = sum(&[
        limits.max_unique_user_stacks,
        limits.max_unique_kernel_stacks,
    ])?;
    sum(&[
        product(limits.max_shards, size_of::<AggregationWindow>() + 4096)?,
        hash_bytes::<u32, Arc<ProcessState>>(limits.max_processes)?,
        hash_bytes::<ProcessIdentity, ProcessId>(limits.max_processes)?,
        product(limits.max_processes, size_of::<ProcessState>() + 64)?,
        product(2, vec_bytes::<(MappingId, bool)>(limits.max_mappings)?)?,
        hash_bytes::<(ProcessId, u32, u64), ThreadId>(limits.max_threads)?,
        hash_bytes::<MappingRecord, MappingId>(limits.max_mappings)?,
        hash_bytes::<(MappingId, u64), FunctionId>(limits.max_functions)?,
        hash_bytes::<LocationKey, LocationId>(limits.max_locations)?,
        hash_bytes::<StackKey, StackId>(stack_count)?,
        hash_bytes::<SampleKey, usize>(limits.max_aggregation_keys)?,
        vec_bytes::<ProcessRecord>(limits.max_processes)?,
        vec_bytes::<ThreadRecord>(limits.max_threads)?,
        vec_bytes::<MappingRecord>(limits.max_mappings)?,
        vec_bytes::<FunctionRecord>(limits.max_functions)?,
        vec_bytes::<LocationRecord>(limits.max_locations)?,
        vec_bytes::<StackRecord>(stack_count)?,
        vec_bytes::<SampleRecord>(limits.max_samples_per_snapshot)?,
        product(
            stack_count,
            sum(&[
                product(limits.max_stack_depth, size_of::<LocationId>())?,
                64,
            ])?,
        )?,
        string_bytes(limits, product(2, limits.max_processes)?)?,
        string_bytes(limits, limits.max_threads)?,
        string_bytes(limits, product(2, limits.max_mappings)?)?,
        string_bytes(limits, limits.max_functions)?,
        string_bytes(
            limits,
            product(limits.max_error_details, limits.max_shards)?,
        )?,
        vec_bytes::<String>(product(limits.max_error_details, limits.max_shards)?)?,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ExecutableMapping, MappingKind, MappingTable, ResolvedSymbol, ThreadMetadata};
    use std::{
        path::Path,
        sync::atomic::{AtomicU64, Ordering},
    };

    struct Processes {
        generation: AtomicU64,
    }
    impl ProcessProvider for Processes {
        fn process(&self, pid: u32) -> Result<ProcessMetadata> {
            Ok(ProcessMetadata {
                identity: ProcessIdentity {
                    pid,
                    start_time_ticks: self.generation.load(Ordering::Relaxed),
                },
                name: format!("process-{pid}"),
                executable: Some("/fake/workload".to_owned()),
                mappings: MappingTable::new(
                    vec![ExecutableMapping {
                        start: 0x1000,
                        end: 0x4000,
                        file_offset: 0,
                        path: Some("/fake/workload".to_owned()),
                        kind: MappingKind::File,
                        device_major: 0,
                        device_minor: 0,
                        inode: 1,
                        path_truncated: false,
                    }],
                    4,
                )?,
                issues: crate::MetadataIssues::default(),
            })
        }
        fn thread(&self, process: ProcessIdentity, tid: u32) -> Result<ThreadMetadata> {
            Ok(ThreadMetadata {
                process,
                tid,
                start_time_ticks: process.start_time_ticks,
                name: Some(format!("thread-{tid}")),
                issues: crate::MetadataIssues::default(),
            })
        }
    }
    struct Symbols;
    impl SymbolResolver for Symbols {
        fn resolve(&mut self, _path: &Path, address: u64) -> Result<Option<ResolvedSymbol>> {
            Ok(Some(ResolvedSymbol {
                name: format!("function-{address:x}"),
                offset: 0,
            }))
        }
    }
    fn event(pid: u32, tid: u32, cpu: u32, timestamp_ns: u64) -> RawSample {
        let mut user_frames = [0; crate::MAX_ABI_STACK_DEPTH];
        user_frames[..2].copy_from_slice(&[0x1100, 0x1200]);
        RawSample {
            pid,
            tid,
            cpu,
            timestamp_ns,
            flags: EventFlags::empty(),
            user_stack_error: 0,
            kernel_stack_error: 0,
            user_depth: 2,
            kernel_depth: 0,
            user_frames,
            kernel_frames: [0; crate::MAX_ABI_STACK_DEPTH],
        }
    }
    fn window(limits: Limits, processes: Arc<dyn ProcessProvider>) -> AggregationWindow {
        AggregationWindow::new(
            limits,
            Duration::from_millis(10),
            SystemTime::UNIX_EPOCH,
            processes,
            Box::new(Symbols),
        )
        .expect("fixture limits are valid")
    }
    fn processes() -> Arc<Processes> {
        Arc::new(Processes {
            generation: AtomicU64::new(1),
        })
    }
    fn finish(window: AggregationWindow) -> ProfileSnapshot {
        window
            .finish(SystemTime::UNIX_EPOCH + Duration::from_secs(1))
            .expect("fixture window is valid")
    }

    /// Scenario: A thread migrates between CPUs while producing an identical stack.
    /// Guarantees: CPU is a sample dimension; stack storage remains shared.
    #[test]
    fn identical_samples_preserve_cpu_dimension() {
        let mut window = window(Limits::default(), processes());
        assert!(
            window
                .record(&event(10, 11, 0, 1))
                .expect("record succeeds")
        );
        assert!(
            window
                .record(&event(10, 11, 2, 2))
                .expect("record succeeds")
        );
        let snapshot = finish(window);
        assert_eq!(snapshot.samples.len(), 2);
        assert_eq!(snapshot.stacks.len(), 1);
    }

    /// Scenario: Equal stacks arrive from distinct thread IDs.
    /// Guarantees: Samples never merge across threads.
    #[test]
    fn distinct_threads_remain_distinct() {
        let mut window = window(Limits::default(), processes());
        assert!(
            window
                .record(&event(10, 11, 0, 1))
                .expect("record succeeds")
        );
        assert!(
            window
                .record(&event(10, 12, 0, 2))
                .expect("record succeeds")
        );
        let snapshot = finish(window);
        assert_eq!(snapshot.samples.len(), 2);
        assert_eq!(snapshot.threads.len(), 2);
    }

    /// Scenario: PID reuse occurs less than a second after a cached sample.
    /// Guarantees: Identity is checked independently of mapping refresh cadence.
    #[test]
    fn immediate_pid_reuse_creates_new_generation() {
        let processes = processes();
        let mut window = window(Limits::default(), processes.clone());
        assert!(
            window
                .record(&event(10, 11, 0, 1))
                .expect("record succeeds")
        );
        processes.generation.store(2, Ordering::Relaxed);
        assert!(
            window
                .record(&event(10, 11, 0, 2))
                .expect("record succeeds")
        );
        let snapshot = finish(window);
        assert_eq!(snapshot.processes.len(), 2);
        assert_eq!(snapshot.samples.len(), 2);
    }

    /// Scenario: A new sample key reaches the aggregation cardinality limit.
    /// Guarantees: Existing keys still aggregate and exactly one new sample is lost.
    #[test]
    fn aggregation_capacity_preserves_existing_keys() {
        let limits = Limits {
            max_aggregation_keys: 1,
            max_samples_per_snapshot: 1,
            ..Limits::default()
        };
        let mut window = window(limits, processes());
        assert!(
            window
                .record(&event(10, 11, 0, 1))
                .expect("record succeeds")
        );
        assert!(
            !window
                .record(&event(10, 12, 0, 2))
                .expect("bounded rejection")
        );
        assert!(
            window
                .record(&event(10, 11, 0, 3))
                .expect("existing key fits")
        );
        let snapshot = finish(window);
        assert_eq!(snapshot.samples[0].count, 2);
        assert_eq!(
            snapshot.statistics.dropped_by_reason[DropReason::AggregationCapacity as usize],
            1
        );
    }

    /// Scenario: Empty failing stacks have different helper codes.
    /// Guarantees: Error stacks count toward the user stack capacity.
    #[test]
    fn empty_error_stacks_are_bounded() {
        let limits = Limits {
            max_unique_user_stacks: 1,
            ..Limits::default()
        };
        let mut window = window(limits, processes());
        let mut sample = event(10, 11, 0, 1);
        sample.user_depth = 0;
        sample.user_frames.fill(0);
        sample.flags = EventFlags::USER_STACK_ERROR;
        sample.user_stack_error = -14;
        assert!(window.record(&sample).expect("first error fits"));
        sample.user_stack_error = -22;
        assert!(!window.record(&sample).expect("second error is bounded"));
        assert_eq!(finish(window).stacks.len(), 1);
    }

    /// Scenario: One event carries both user and kernel frames.
    /// Guarantees: Stack kinds and process ownership remain distinct.
    #[test]
    fn user_and_kernel_stacks_are_preserved() {
        let mut window = window(Limits::default(), processes());
        let mut sample = event(10, 11, 0, 1);
        sample.kernel_depth = 1;
        sample.kernel_frames[0] = 0xffff_0000;
        assert!(window.record(&sample).expect("record succeeds"));
        let snapshot = finish(window);
        let id = snapshot.samples[0]
            .kernel_stack
            .expect("kernel stack exists");
        assert!(snapshot.stacks[id.0 as usize].kernel);
    }

    /// Scenario: Events arrive out of timestamp order for an existing sample.
    /// Guarantees: First/last timestamps bound every represented event.
    #[test]
    fn reordered_timestamps_preserve_extrema() {
        let mut window = window(Limits::default(), processes());
        for timestamp in [30, 10, 20] {
            assert!(
                window
                    .record(&event(10, 11, 0, timestamp))
                    .expect("record succeeds")
            );
        }
        let snapshot = finish(window);
        assert_eq!(
            (
                snapshot.samples[0].first_timestamp_ns,
                snapshot.samples[0].last_timestamp_ns
            ),
            (10, 30)
        );
    }

    /// Scenario: A public RawSample advertises depth beyond its fixed arrays.
    /// Guarantees: Invalid injected input is rejected without indexing panic.
    #[test]
    fn malformed_depth_does_not_panic() {
        let mut window = window(Limits::default(), processes());
        let mut sample = event(10, 11, 0, 1);
        sample.user_depth = u16::MAX;
        assert!(!window.record(&sample).expect("malformed sample is counted"));
        assert_eq!(
            window.statistics().dropped_by_reason[DropReason::MalformedEvent as usize],
            1
        );
    }

    /// Scenario: New process, thread, location, user-stack, or kernel-stack
    /// cardinality reaches its configured bound.
    /// Guarantees: Every table stays bounded and loses exactly one sample with
    /// the corresponding reason while keeping the first valid sample.
    #[test]
    fn individual_table_capacities_are_enforced() {
        for reason in [
            DropReason::ProcessCapacity,
            DropReason::ThreadCapacity,
            DropReason::LocationCapacity,
            DropReason::UserStackCapacity,
            DropReason::KernelStackCapacity,
        ] {
            let mut limits = Limits::default();
            match reason {
                DropReason::ProcessCapacity => limits.max_processes = 1,
                DropReason::ThreadCapacity => limits.max_threads = 1,
                DropReason::LocationCapacity => limits.max_locations = 1,
                DropReason::UserStackCapacity => limits.max_unique_user_stacks = 1,
                DropReason::KernelStackCapacity => limits.max_unique_kernel_stacks = 1,
                _ => unreachable!("fixed fixture cases"),
            }
            let mut first = event(10, 11, 0, 1);
            first.user_depth = 1;
            first.user_frames[1] = 0;
            if reason == DropReason::KernelStackCapacity {
                first.kernel_depth = 1;
                first.kernel_frames[0] = 0xffff_1000;
            }
            let mut second = first.clone();
            match reason {
                DropReason::ProcessCapacity => {
                    second.pid = 20;
                    second.tid = 20;
                }
                DropReason::ThreadCapacity => second.tid = 12,
                DropReason::LocationCapacity | DropReason::UserStackCapacity => {
                    second.user_frames[0] = 0x1200
                }
                DropReason::KernelStackCapacity => second.kernel_frames[0] = 0xffff_2000,
                _ => unreachable!("fixed fixture cases"),
            }
            let mut window = window(limits, processes());
            assert!(
                window.record(&first).expect("first sample fits"),
                "{reason:?}"
            );
            assert!(
                !window.record(&second).expect("new cardinality is bounded"),
                "{reason:?}"
            );
            let snapshot = finish(window);
            assert_eq!(
                snapshot.statistics.dropped_by_reason[reason as usize], 1,
                "{reason:?}"
            );
            assert_eq!(snapshot.statistics.samples_in_snapshots, 1);
        }
    }

    /// Scenario: Optional mapping or function capacity is exhausted.
    /// Guarantees: Addresses remain collectable, omissions are explicit, and
    /// neither optional table grows past its cap.
    #[test]
    fn optional_metadata_capacity_preserves_samples() {
        for functions in [false, true] {
            let mut limits = Limits::default();
            if functions {
                limits.max_functions = 1;
            } else {
                limits.max_mappings = 1;
            }
            let mut first = event(10, 11, 0, 1);
            first.user_depth = 1;
            first.user_frames[1] = 0;
            let mut second = first.clone();
            if functions {
                second.user_frames[0] = 0x1200;
            } else {
                second.pid = 20;
                second.tid = 20;
            }
            let mut window = window(limits, processes());
            assert!(window.record(&first).expect("first sample fits"));
            assert!(
                window
                    .record(&second)
                    .expect("sample survives metadata omission")
            );
            let snapshot = finish(window);
            assert_eq!(snapshot.statistics.samples_in_snapshots, 2);
            assert!(snapshot.statistics.metadata_omissions > 0);
            if functions {
                assert_eq!(snapshot.functions.len(), 1);
            } else {
                assert_eq!(snapshot.mappings.len(), 1);
            }
        }
    }

    /// Scenario: Configuration retains fewer frames than the ABI event carries.
    /// Guarantees: The stack is explicitly truncated and remains graph-valid.
    #[test]
    fn configured_depth_truncation_is_explicit() {
        let limits = Limits {
            max_stack_depth: 1,
            ..Limits::default()
        };
        let mut window = window(limits, processes());
        assert!(window.record(&event(10, 11, 0, 1)).expect("sample fits"));
        let snapshot = finish(window);
        assert_eq!(snapshot.stacks[0].locations.len(), 1);
        assert!(snapshot.stacks[0].truncated);
        assert_eq!(snapshot.statistics.truncated_stacks, 1);
    }

    /// Scenario: Snapshot bytes, rather than a row cardinality, fill a window.
    /// Guarantees: Byte admission drops samples before exceeding the output bound.
    #[test]
    fn snapshot_bytes_are_enforced_during_admission() {
        let limits = Limits {
            max_snapshot_bytes: 2048,
            ..Limits::default()
        };
        let mut window = window(limits, processes());
        for tid in 1..100 {
            let _accepted = window
                .record(&event(10, tid, 0, 1))
                .expect("bounded admission");
        }
        let snapshot = finish(window);
        assert!(snapshot.logical_bytes() <= 2048);
        assert!(snapshot.statistics.dropped_by_reason[DropReason::SnapshotCapacity as usize] > 0);
    }
}
