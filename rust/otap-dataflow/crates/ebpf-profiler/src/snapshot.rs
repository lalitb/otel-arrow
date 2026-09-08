// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Owned transport-neutral profile snapshots.

use std::{
    mem::size_of,
    time::{Duration, SystemTime},
};

use crate::{ProfilerError, ProfilerStatistics, Result};

macro_rules! id_type {
    ($name:ident) => {
        #[doc = concat!("Checked snapshot-local identifier for `", stringify!($name), "`.")]
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(pub u32);
    };
}

id_type!(ProcessId);
id_type!(ThreadId);
id_type!(MappingId);
id_type!(FunctionId);
id_type!(LocationId);
id_type!(StackId);

/// Profiling mode represented by a snapshot.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ProfileKind {
    /// Periodic host-wide on-CPU samples.
    #[default]
    OnCpu,
}

/// Guaranteed native user-stack collection mode.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum UserUnwinding {
    /// Native frame-pointer-enabled code through the kernel stack helper.
    #[default]
    FramePointer,
}

/// Clock domain used by sample timestamps.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TimestampClock {
    /// Nanoseconds since boot, including suspend time.
    #[default]
    BootTime,
}

/// Fixed, transport-neutral profile attributes. Process and thread attributes
/// are typed columns in their respective tables rather than duplicated maps.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SnapshotAttributes {
    /// Sample mode.
    pub kind: ProfileKind,
    /// Guaranteed native user unwinding mode.
    pub user_unwinding: UserUnwinding,
    /// Domain of sample timestamps.
    pub timestamp_clock: TimestampClock,
}

/// Process row owned by a snapshot.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ProcessRecord {
    /// Operating-system process identifier.
    pub pid: u32,
    /// Procfs start-time ticks used to distinguish PID reuse.
    pub start_time_ticks: u64,
    /// Process command name.
    pub name: String,
    /// Executable path when accessible.
    pub executable: Option<String>,
}

/// Thread row owned by a snapshot.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ThreadRecord {
    /// Owning process row.
    pub process: ProcessId,
    /// Operating-system thread identifier.
    pub tid: u32,
    /// Procfs thread start ticks, distinguishing TID reuse within a process.
    pub start_time_ticks: u64,
    /// Thread name when accessible.
    pub name: Option<String>,
}

/// Executable mapping row owned by a snapshot.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct MappingRecord {
    /// Owning process row.
    pub process: ProcessId,
    /// Inclusive virtual-address start.
    pub start: u64,
    /// Exclusive virtual-address end.
    pub end: u64,
    /// File offset corresponding to `start`.
    pub file_offset: u64,
    /// Executable path or synthetic mapping label.
    pub path: Option<String>,
    /// Whether the mapping is executable.
    pub executable: bool,
    /// File-backed, deleted, anonymous, or special mapping classification.
    pub kind: crate::MappingKind,
    /// Device/inode identity observed in procfs.
    pub file_identity: crate::MappingFileIdentity,
    /// A shortened path is display-only and must never be opened.
    pub path_truncated: bool,
}

/// Resolved function row owned by a snapshot.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct FunctionRecord {
    /// Mapping that defines this function; equal names in different objects
    /// remain distinct.
    pub mapping: MappingId,
    /// File-relative start address of the symbol.
    pub address: u64,
    /// Symbol or function name.
    pub name: String,
    /// Source filename when available.
    pub filename: Option<String>,
    /// Source line when available.
    pub line: Option<u32>,
}

/// Normalized instruction location owned by a snapshot.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct LocationRecord {
    /// Process generation for a user address, including unmapped addresses.
    /// Kernel addresses have no process owner.
    pub process: Option<ProcessId>,
    /// Mapping containing the address, if any.
    pub mapping: Option<MappingId>,
    /// File-relative address when mapped; otherwise the absolute instruction
    /// address. Mapping start and file offset permit checked reconstruction.
    pub address: u64,
    /// Resolved function, if any.
    pub function: Option<FunctionId>,
    /// Whether the address came from a kernel stack.
    pub kernel: bool,
}

/// Interned stack row owned by a snapshot.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct StackRecord {
    /// Process generation owning a user stack. Kernel stacks have no owner.
    pub process: Option<ProcessId>,
    /// Distinguishes kernel and user stacks even when they have no frames.
    pub kernel: bool,
    /// Ordered leaf-to-root locations.
    pub locations: Vec<LocationId>,
    /// Whether collection truncated this stack.
    pub truncated: bool,
    /// Stable helper error code when collection failed.
    pub collection_error: Option<i32>,
}

/// Aggregated sample row owned by a snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SampleRecord {
    /// Process row.
    pub process: ProcessId,
    /// Thread row.
    pub thread: ThreadId,
    /// User stack row.
    pub user_stack: StackId,
    /// Optional kernel stack row.
    pub kernel_stack: Option<StackId>,
    /// Number of identical samples represented.
    pub count: u64,
    /// Earliest retained BOOTTIME timestamp in nanoseconds.
    pub first_timestamp_ns: u64,
    /// Latest retained BOOTTIME timestamp in nanoseconds.
    pub last_timestamp_ns: u64,
    /// Logical CPU shared by every event in this aggregation key.
    pub cpu: u32,
}

/// Complete owned profile data for one reporting window.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileSnapshot {
    /// Fixed collection attributes shared by every sample in the window.
    pub attributes: SnapshotAttributes,
    /// Wall-clock start of the reporting window.
    pub window_start: SystemTime,
    /// Wall-clock end of the reporting window.
    pub window_end: SystemTime,
    /// Target sampling period.
    pub sampling_period: Duration,
    /// Process table.
    pub processes: Vec<ProcessRecord>,
    /// Thread table.
    pub threads: Vec<ThreadRecord>,
    /// Executable mapping table.
    pub mappings: Vec<MappingRecord>,
    /// Resolved function table.
    pub functions: Vec<FunctionRecord>,
    /// Normalized location table.
    pub locations: Vec<LocationRecord>,
    /// Stack table.
    pub stacks: Vec<StackRecord>,
    /// Aggregated sample table.
    pub samples: Vec<SampleRecord>,
    /// Collection and loss statistics for this window.
    pub statistics: ProfilerStatistics,
    /// Bounded non-cardinal diagnostic details.
    pub error_details: Vec<String>,
}

impl ProfileSnapshot {
    /// Creates an empty owned window. Builders populate only bounded tables.
    #[must_use]
    pub fn empty(
        window_start: SystemTime,
        window_end: SystemTime,
        sampling_period: Duration,
    ) -> Self {
        Self {
            attributes: SnapshotAttributes::default(),
            window_start,
            window_end,
            sampling_period,
            processes: Vec::new(),
            threads: Vec::new(),
            mappings: Vec::new(),
            functions: Vec::new(),
            locations: Vec::new(),
            stacks: Vec::new(),
            samples: Vec::new(),
            statistics: ProfilerStatistics::default(),
            error_details: Vec::new(),
        }
    }

    /// Returns an estimated logical byte count for bounded handoff decisions.
    #[must_use]
    pub fn logical_bytes(&self) -> usize {
        let fixed = size_of::<Self>()
            .saturating_add(
                self.processes
                    .len()
                    .saturating_mul(size_of::<ProcessRecord>()),
            )
            .saturating_add(self.threads.len().saturating_mul(size_of::<ThreadRecord>()))
            .saturating_add(
                self.mappings
                    .len()
                    .saturating_mul(size_of::<MappingRecord>()),
            )
            .saturating_add(
                self.functions
                    .len()
                    .saturating_mul(size_of::<FunctionRecord>()),
            )
            .saturating_add(
                self.locations
                    .len()
                    .saturating_mul(size_of::<LocationRecord>()),
            )
            .saturating_add(self.stacks.len().saturating_mul(size_of::<StackRecord>()))
            .saturating_add(self.samples.len().saturating_mul(size_of::<SampleRecord>()))
            .saturating_add(self.error_details.len().saturating_mul(size_of::<String>()));
        let strings = self
            .processes
            .iter()
            .map(|row| {
                row.name
                    .len()
                    .saturating_add(row.executable.as_ref().map_or(0, String::len))
            })
            .chain(
                self.threads
                    .iter()
                    .map(|row| row.name.as_ref().map_or(0, String::len)),
            )
            .chain(
                self.mappings
                    .iter()
                    .map(|row| row.path.as_ref().map_or(0, String::len)),
            )
            .chain(self.functions.iter().map(|row| {
                row.name
                    .len()
                    .saturating_add(row.filename.as_ref().map_or(0, String::len))
            }))
            .chain(self.error_details.iter().map(String::len))
            .fold(0_usize, usize::saturating_add);
        let stack_indexes = self
            .stacks
            .iter()
            .map(|stack| {
                stack
                    .locations
                    .len()
                    .saturating_mul(size_of::<LocationId>())
            })
            .fold(0_usize, usize::saturating_add);
        fixed.saturating_add(strings).saturating_add(stack_indexes)
    }

    /// Requested heap/table storage, including spare vector/string capacity.
    /// This excludes allocator metadata and the consumer's own copies.
    #[must_use]
    pub fn allocated_bytes(&self) -> usize {
        let table_bytes = self
            .processes
            .capacity()
            .saturating_mul(size_of::<ProcessRecord>())
            .saturating_add(
                self.threads
                    .capacity()
                    .saturating_mul(size_of::<ThreadRecord>()),
            )
            .saturating_add(
                self.mappings
                    .capacity()
                    .saturating_mul(size_of::<MappingRecord>()),
            )
            .saturating_add(
                self.functions
                    .capacity()
                    .saturating_mul(size_of::<FunctionRecord>()),
            )
            .saturating_add(
                self.locations
                    .capacity()
                    .saturating_mul(size_of::<LocationRecord>()),
            )
            .saturating_add(
                self.stacks
                    .capacity()
                    .saturating_mul(size_of::<StackRecord>()),
            )
            .saturating_add(
                self.samples
                    .capacity()
                    .saturating_mul(size_of::<SampleRecord>()),
            )
            .saturating_add(
                self.error_details
                    .capacity()
                    .saturating_mul(size_of::<String>()),
            );
        let dynamic_bytes = self
            .processes
            .iter()
            .map(|row| {
                row.name
                    .capacity()
                    .saturating_add(row.executable.as_ref().map_or(0, String::capacity))
            })
            .chain(
                self.threads
                    .iter()
                    .map(|row| row.name.as_ref().map_or(0, String::capacity)),
            )
            .chain(
                self.mappings
                    .iter()
                    .map(|row| row.path.as_ref().map_or(0, String::capacity)),
            )
            .chain(self.functions.iter().map(|row| {
                row.name
                    .capacity()
                    .saturating_add(row.filename.as_ref().map_or(0, String::capacity))
            }))
            .chain(self.stacks.iter().map(|row| {
                row.locations
                    .capacity()
                    .saturating_mul(size_of::<LocationId>())
            }))
            .chain(self.error_details.iter().map(String::capacity))
            .fold(0usize, usize::saturating_add);
        size_of::<Self>()
            .saturating_add(table_bytes)
            .saturating_add(dynamic_bytes)
    }

    pub(crate) fn compact(&mut self) {
        self.processes.shrink_to_fit();
        self.threads.shrink_to_fit();
        self.mappings.shrink_to_fit();
        self.functions.shrink_to_fit();
        self.locations.shrink_to_fit();
        self.stacks.shrink_to_fit();
        self.samples.shrink_to_fit();
        self.error_details.shrink_to_fit();
    }

    /// Validates every snapshot-local reference and reporting-window invariant.
    pub fn validate(&self) -> Result<()> {
        if self.window_end < self.window_start {
            return Err(ProfilerError::InternalInvariant(
                "snapshot window end precedes start".to_owned(),
            ));
        }
        if self.sampling_period.is_zero() {
            return Err(ProfilerError::InternalInvariant(
                "snapshot sampling period must be non-zero".to_owned(),
            ));
        }
        for thread in &self.threads {
            check_index(thread.process.0, self.processes.len(), "thread.process")?;
        }
        for mapping in &self.mappings {
            check_index(mapping.process.0, self.processes.len(), "mapping.process")?;
            if mapping.end <= mapping.start {
                return Err(ProfilerError::InternalInvariant(
                    "mapping end must exceed start".to_owned(),
                ));
            }
            let _last_file_byte = mapping
                .file_offset
                .checked_add(mapping.end - mapping.start - 1)
                .ok_or_else(|| {
                    ProfilerError::InternalInvariant("mapping file extent overflow".to_owned())
                })?;
        }
        for function in &self.functions {
            check_index(function.mapping.0, self.mappings.len(), "function.mapping")?;
        }
        for location in &self.locations {
            if location.kernel {
                if location.process.is_some()
                    || location.mapping.is_some()
                    || location.function.is_some()
                {
                    return Err(ProfilerError::InternalInvariant(
                        "kernel location contains user metadata".to_owned(),
                    ));
                }
            } else {
                let process = location.process.ok_or_else(|| {
                    ProfilerError::InternalInvariant(
                        "user location has no process generation".to_owned(),
                    )
                })?;
                check_index(process.0, self.processes.len(), "location.process")?;
            }
            if let Some(mapping) = location.mapping {
                check_index(mapping.0, self.mappings.len(), "location.mapping")?;
                let row = &self.mappings[mapping.0 as usize];
                if Some(row.process) != location.process
                    || location
                        .address
                        .checked_sub(row.file_offset)
                        .is_none_or(|relative| relative >= row.end - row.start)
                {
                    return Err(ProfilerError::InternalInvariant(
                        "location does not belong to its mapping".to_owned(),
                    ));
                }
            }
            if let Some(function) = location.function {
                check_index(function.0, self.functions.len(), "location.function")?;
                if Some(self.functions[function.0 as usize].mapping) != location.mapping {
                    return Err(ProfilerError::InternalInvariant(
                        "location/function mappings disagree".to_owned(),
                    ));
                }
            }
        }
        for stack in &self.stacks {
            if stack.kernel != stack.process.is_none()
                || stack.locations.len() > crate::MAX_ABI_STACK_DEPTH
            {
                return Err(ProfilerError::InternalInvariant(
                    "invalid stack kind, owner, or depth".to_owned(),
                ));
            }
            if let Some(process) = stack.process {
                check_index(process.0, self.processes.len(), "stack.process")?;
            }
            if stack.collection_error.is_some_and(|code| code >= 0)
                || (stack.collection_error.is_some() && !stack.locations.is_empty())
            {
                return Err(ProfilerError::InternalInvariant(
                    "stack collection error has invalid frames or code".to_owned(),
                ));
            }
            for location in &stack.locations {
                check_index(location.0, self.locations.len(), "stack.location")?;
                let row = &self.locations[location.0 as usize];
                if row.kernel != stack.kernel || row.process != stack.process {
                    return Err(ProfilerError::InternalInvariant(
                        "stack/frame ownership disagrees".to_owned(),
                    ));
                }
            }
        }
        let mut represented = 0u64;
        for sample in &self.samples {
            check_index(sample.process.0, self.processes.len(), "sample.process")?;
            check_index(sample.thread.0, self.threads.len(), "sample.thread")?;
            check_index(sample.user_stack.0, self.stacks.len(), "sample.user_stack")?;
            if let Some(stack) = sample.kernel_stack {
                check_index(stack.0, self.stacks.len(), "sample.kernel_stack")?;
                if !self.stacks[stack.0 as usize].kernel {
                    return Err(ProfilerError::InternalInvariant(
                        "kernel sample references a user stack".to_owned(),
                    ));
                }
            }
            if self.threads[sample.thread.0 as usize].process != sample.process
                || self.stacks[sample.user_stack.0 as usize].process != Some(sample.process)
                || self.stacks[sample.user_stack.0 as usize].kernel
                || sample.count == 0
                || sample.last_timestamp_ns < sample.first_timestamp_ns
            {
                return Err(ProfilerError::InternalInvariant(
                    "sample identity, timestamps, or count is invalid".to_owned(),
                ));
            }
            represented = represented.checked_add(sample.count).ok_or_else(|| {
                ProfilerError::InternalInvariant("snapshot sample count overflow".to_owned())
            })?;
        }
        if represented != self.statistics.samples_in_snapshots
            || represented > self.statistics.samples_accepted
            || self.statistics.samples_accepted > self.statistics.raw_events_received
        {
            return Err(ProfilerError::InternalInvariant(
                "snapshot sample accounting is inconsistent".to_owned(),
            ));
        }
        Ok(())
    }
}

fn check_index(index: u32, len: usize, field: &'static str) -> Result<()> {
    let index = usize::try_from(index)
        .map_err(|_| ProfilerError::InternalInvariant(format!("{field} cannot fit in usize")))?;
    if index >= len {
        return Err(ProfilerError::InternalInvariant(format!(
            "{field} index {index} is outside table length {len}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: A consumer takes ownership of a complete snapshot graph.
    /// Guarantees: Consuming rows does not require cloning live profiler caches.
    #[test]
    fn snapshot_can_be_consumed_by_ownership() {
        let snapshot = ProfileSnapshot {
            attributes: SnapshotAttributes::default(),
            window_start: SystemTime::UNIX_EPOCH,
            window_end: SystemTime::UNIX_EPOCH + Duration::from_secs(1),
            sampling_period: Duration::from_millis(10),
            processes: vec![ProcessRecord {
                pid: 1,
                start_time_ticks: 1,
                name: "workload".to_owned(),
                executable: None,
            }],
            threads: vec![ThreadRecord {
                process: ProcessId(0),
                tid: 1,
                start_time_ticks: 1,
                name: None,
            }],
            mappings: Vec::new(),
            functions: Vec::new(),
            locations: vec![LocationRecord {
                process: Some(ProcessId(0)),
                mapping: None,
                address: 0x1000,
                function: None,
                kernel: false,
            }],
            stacks: vec![StackRecord {
                process: Some(ProcessId(0)),
                kernel: false,
                locations: vec![LocationId(0)],
                truncated: false,
                collection_error: None,
            }],
            samples: vec![SampleRecord {
                process: ProcessId(0),
                thread: ThreadId(0),
                user_stack: StackId(0),
                kernel_stack: None,
                count: 1,
                first_timestamp_ns: 1,
                last_timestamp_ns: 1,
                cpu: 0,
            }],
            statistics: ProfilerStatistics {
                raw_events_received: 1,
                samples_accepted: 1,
                samples_in_snapshots: 1,
                ..ProfilerStatistics::default()
            },
            error_details: Vec::new(),
        };
        snapshot.validate().expect("snapshot should be valid");
        let owned_samples = snapshot.samples;
        assert_eq!(owned_samples.len(), 1);
    }

    /// Scenario: A snapshot contains an out-of-range stack-to-location index.
    /// Guarantees: Graph corruption is detected before consumer conversion.
    #[test]
    fn invalid_snapshot_reference_is_rejected() {
        let snapshot = ProfileSnapshot {
            attributes: SnapshotAttributes::default(),
            window_start: SystemTime::UNIX_EPOCH,
            window_end: SystemTime::UNIX_EPOCH,
            sampling_period: Duration::from_secs(1),
            processes: Vec::new(),
            threads: Vec::new(),
            mappings: Vec::new(),
            functions: Vec::new(),
            locations: Vec::new(),
            stacks: vec![StackRecord {
                process: None,
                kernel: true,
                locations: vec![LocationId(0)],
                truncated: false,
                collection_error: None,
            }],
            samples: Vec::new(),
            statistics: ProfilerStatistics::default(),
            error_details: Vec::new(),
        };
        assert!(snapshot.validate().is_err());
    }
}
