// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};
use std::io::Cursor;
use std::ops::Bound::{Excluded, Unbounded};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::error::BackendError;
use crate::limits::ResourceLimits;
use crate::mappings::{LpmPrefix, ProcessMapping, calculate_prefixes};
use crate::process::{ProcessIdentity, ProcessState, ProcessTable, read_process_state};
use crate::statistics::{DropReason, ErrorDetails, LossCounters};
use crate::unwind::{ExecutableId, ExecutableLayout, FramePointerUnwindPlan};

use super::clock::KernelClock;
use super::maps::{PidPageKey, ProcessMaps};

#[derive(Debug)]
struct UploadedExecutable {
    id: ExecutableId,
    plan: FramePointerUnwindPlan,
    references: usize,
}

struct ProcessKernelState {
    state: ProcessState,
    complete: bool,
    keys: Vec<PidPageKey>,
    executable_ids: Vec<u64>,
    admitted_at_ns: u64,
    verified_until_ns: u64,
}

pub(crate) struct ProcessSynchronizer {
    procfs_root: PathBuf,
    page_size: u64,
    limits: ResourceLimits,
    clock: KernelClock,
    verified_pids: BTreeSet<u32>,
    table: ProcessTable,
    kernel_state: BTreeMap<u32, ProcessKernelState>,
    executables: BTreeMap<u64, UploadedExecutable>,
    mapping_count: usize,
    pid_page_entries: usize,
    unwind_bytes: usize,
    stack_delta_records: usize,
    stack_page_entries: usize,
    remaining_updates: usize,
    cleanup_cursor: Option<u32>,
    losses: LossCounters,
    errors: ErrorDetails,
}

impl ProcessSynchronizer {
    pub(crate) fn new(
        procfs_root: PathBuf,
        limits: ResourceLimits,
        page_size: usize,
        clock: KernelClock,
    ) -> Self {
        Self {
            table: ProcessTable::new(limits.max_processes),
            kernel_state: BTreeMap::new(),
            executables: BTreeMap::new(),
            mapping_count: 0,
            pid_page_entries: 0,
            unwind_bytes: 0,
            stack_delta_records: 0,
            stack_page_entries: 0,
            remaining_updates: limits.max_metadata_updates_per_drain,
            cleanup_cursor: None,
            losses: LossCounters::default(),
            errors: ErrorDetails::new(limits.max_error_details),
            procfs_root,
            page_size: page_size as u64,
            limits,
            clock,
            verified_pids: BTreeSet::new(),
        }
    }

    pub(crate) fn drain_pid_events(
        &mut self,
        maps: &mut impl ProcessMaps,
    ) -> Result<(), BackendError> {
        let events = maps.drain_pid_events(self.remaining_updates)?;
        for pid_tid in events {
            let pid = (pid_tid >> 32) as u32;
            if pid != 0 {
                if let Err(error) = self.synchronize_pid(pid, maps) {
                    if matches!(error, BackendError::Kernel { .. }) {
                        return Err(error);
                    }
                    self.errors.push(error.to_string());
                }
            }
        }
        Ok(())
    }

    pub(crate) fn synchronize_pid(
        &mut self,
        pid: u32,
        maps: &mut impl ProcessMaps,
    ) -> Result<(), BackendError> {
        if self.remaining_updates == 0 {
            return Err(BackendError::Capacity("metadata updates per drain"));
        }
        self.remaining_updates -= 1;
        let observation_started = self.clock.now()?;
        let read_state = || {
            read_process_state(
                &self.procfs_root,
                pid,
                self.limits.max_executable_mappings,
                self.limits.max_procfs_file_bytes,
            )
        };
        let mut result = read_state();
        for _ in 0..self.limits.max_retry_attempts {
            let retryable = match &result {
                Err(BackendError::ProcessChanged { .. }) => true,
                Err(BackendError::Io { source, .. }) => {
                    source.kind() == std::io::ErrorKind::Interrupted
                }
                _ => false,
            };
            if !retryable {
                break;
            }
            result = read_state();
        }
        let (state, parse_errors, capacity_drops) = match result {
            Ok(state) => state,
            Err(BackendError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                self.remove_process(pid, maps)?;
                maps.remove_reported_pid(pid)?;
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        if parse_errors != 0 {
            self.errors
                .push(format!("PID {pid}: {parse_errors} malformed maps lines"));
        }
        if capacity_drops != 0 {
            self.losses.add(DropReason::MappingCapacity, capacity_drops);
        }
        if let Some(existing) = self.kernel_state.get_mut(&pid) {
            if existing.complete && existing.state == state {
                existing.verified_until_ns = existing.verified_until_ns.max(observation_started);
                let _inserted = self.verified_pids.insert(pid);
                maps.remove_reported_pid(pid)?;
                return Ok(());
            }
        }

        let previous_window = self
            .kernel_state
            .get(&pid)
            .filter(|entry| entry.state == state)
            .map(|entry| (entry.admitted_at_ns, entry.verified_until_ns));
        self.remove_process(pid, maps)?;
        if self.table.len() == self.limits.max_processes {
            self.losses.add(DropReason::ProcessCapacity, 1);
            maps.remove_reported_pid(pid)?;
            return Err(BackendError::Capacity("process capacity"));
        }
        if self.mapping_count.saturating_add(state.mappings.len())
            > self.limits.max_executable_mappings
        {
            self.losses.add(DropReason::MappingCapacity, 1);
            maps.remove_reported_pid(pid)?;
            return Err(BackendError::Capacity("mapping capacity"));
        }

        let mut keys = Vec::new();
        let mut executable_ids = Vec::new();
        if self.pid_page_entries >= self.limits.max_unwind_map_entries {
            self.losses.add(DropReason::MappingCapacity, 1);
            return Err(BackendError::Capacity("PID page capacity"));
        }
        let dummy = maps.insert_pid_mapping(pid, LpmPrefix { key: 0, length: 64 }, 0, 0)?;
        keys.push(dummy);
        self.pid_page_entries = self.pid_page_entries.saturating_add(1);

        let mut complete = parse_errors == 0 && capacity_drops == 0;
        for mapping in &state.mappings {
            match self.synchronize_mapping(pid, mapping, maps, &mut keys, &mut executable_ids) {
                Ok(()) => {}
                Err(error @ BackendError::Kernel { .. }) => {
                    // Unexpected map failures invalidate the current synchronization.
                    // The caller must stop the session rather than continue with stale maps.
                    return Err(error);
                }
                Err(error @ BackendError::Capacity(_)) => {
                    complete = false;
                    self.errors.push(error.to_string());
                }
                Err(error) => {
                    complete = false;
                    self.losses.add(DropReason::ProcessMetadataUnavailable, 1);
                    self.errors.push(error.to_string());
                }
            }
        }

        self.mapping_count = self.mapping_count.saturating_add(state.mappings.len());
        let _update = self
            .table
            .upsert(state.clone())
            .map_err(|reason| BackendError::Capacity(drop_reason_label(reason)))?;
        let _previous = self.kernel_state.insert(
            pid,
            ProcessKernelState {
                state,
                complete,
                keys,
                executable_ids,
                admitted_at_ns: u64::MAX,
                verified_until_ns: 0,
            },
        );
        // File reads and uploads can outlive the procfs observation that began
        // this update. Do not publish a generation until its mappings still agree.
        let confirmation = read_process_state(
            &self.procfs_root,
            pid,
            self.limits.max_executable_mappings,
            self.limits.max_procfs_file_bytes,
        );
        match confirmation {
            Ok((confirmed, _, _))
                if self
                    .kernel_state
                    .get(&pid)
                    .is_some_and(|entry| entry.state == confirmed) => {}
            Ok(_) => {
                self.remove_process(pid, maps)?;
                return Err(BackendError::ProcessChanged { pid });
            }
            Err(error) => {
                self.remove_process(pid, maps)?;
                return Err(error);
            }
        }
        let (admitted_at, verified_until) = if let Some((admitted, verified)) = previous_window {
            // Completing missing metadata does not create a new PID/mapping
            // generation or invalidate already queued samples from valid maps.
            (admitted, verified.max(observation_started))
        } else {
            let admitted = self.clock.now()?;
            (admitted, admitted)
        };
        if let Some(entry) = self.kernel_state.get_mut(&pid) {
            entry.admitted_at_ns = admitted_at;
            entry.verified_until_ns = verified_until;
        }
        let _inserted = self.verified_pids.insert(pid);
        maps.remove_reported_pid(pid)?;
        Ok(())
    }

    pub(crate) fn identity_for_trace(
        &mut self,
        pid: u32,
        ktime_ns: u64,
        maps: &mut impl ProcessMaps,
        allow_metadata: bool,
    ) -> Result<Option<ProcessIdentity>, BackendError> {
        if allow_metadata && !self.verified_pids.contains(&pid) {
            if self.remaining_updates == 0 {
                self.losses.add(DropReason::ProcessMetadataUnavailable, 1);
                return Ok(None);
            }
            self.remaining_updates -= 1;
            // This timestamp precedes the identity observation but follows the
            // bounded kernel drain. It cannot authorize a later, unseen sample.
            let observed_before = self.clock.now()?;
            let state = read_process_state(
                &self.procfs_root,
                pid,
                self.limits.max_executable_mappings,
                self.limits.max_procfs_file_bytes,
            );
            match state {
                Ok((state, parse_errors, capacity_drops))
                    if parse_errors == 0
                        && capacity_drops == 0
                        && self
                            .kernel_state
                            .get(&pid)
                            .is_some_and(|entry| entry.state == state) =>
                {
                    if let Some(entry) = self.kernel_state.get_mut(&pid) {
                        entry.verified_until_ns = entry.verified_until_ns.max(observed_before);
                    }
                    let _inserted = self.verified_pids.insert(pid);
                }
                Ok(_) => {
                    if let Err(error) = self.synchronize_pid(pid, maps) {
                        if matches!(error, BackendError::Kernel { .. }) {
                            return Err(error);
                        }
                        self.record_error(&error);
                        self.losses.add(DropReason::ProcessMetadataUnavailable, 1);
                        return Ok(None);
                    }
                }
                Err(error) => {
                    if matches!(&error, BackendError::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound)
                    {
                        self.remove_process(pid, maps)?;
                    }
                    self.record_error(&error);
                    self.losses.add(DropReason::ProcessMetadataUnavailable, 1);
                    return Ok(None);
                }
            }
        }
        let Some(entry) = self.kernel_state.get(&pid) else {
            self.losses.add(DropReason::ProcessMetadataUnavailable, 1);
            return Ok(None);
        };
        if ktime_ns < entry.admitted_at_ns || ktime_ns > entry.verified_until_ns {
            self.losses.add(DropReason::ProcessGenerationMismatch, 1);
            return Ok(None);
        }
        Ok(Some(entry.state.identity.clone()))
    }

    pub(crate) fn cleanup_processes(
        &mut self,
        maps: &mut impl ProcessMaps,
    ) -> Result<(), BackendError> {
        let start = self.cleanup_cursor.map_or(Unbounded, Excluded);
        let mut pids: Vec<_> = self
            .kernel_state
            .range((start, Unbounded))
            .take(self.remaining_updates)
            .map(|(&pid, _)| pid)
            .collect();
        if pids.is_empty() {
            pids = self
                .kernel_state
                .keys()
                .take(self.remaining_updates)
                .copied()
                .collect();
        }
        for pid in pids {
            self.cleanup_cursor = Some(pid);
            if let Err(error) = self.synchronize_pid(pid, maps) {
                if matches!(error, BackendError::Kernel { .. }) {
                    return Err(error);
                }
                self.errors.push(error.to_string());
            }
        }
        Ok(())
    }

    pub(crate) fn begin_drain(&mut self) {
        self.remaining_updates = self.limits.max_metadata_updates_per_drain;
        self.verified_pids.clear();
    }

    pub(crate) fn take_losses(&mut self) -> LossCounters {
        std::mem::take(&mut self.losses)
    }

    pub(crate) fn errors(&self) -> &ErrorDetails {
        &self.errors
    }

    pub(crate) fn record_error(&mut self, error: &BackendError) {
        self.errors.push(error.to_string());
    }

    pub(crate) fn release_userspace(&mut self) {
        self.table = ProcessTable::new(self.limits.max_processes);
        self.kernel_state.clear();
        self.executables.clear();
        self.mapping_count = 0;
        self.pid_page_entries = 0;
        self.stack_page_entries = 0;
        self.unwind_bytes = 0;
        self.stack_delta_records = 0;
        self.verified_pids.clear();
    }

    pub(crate) fn remove_process(
        &mut self,
        pid: u32,
        maps: &mut impl ProcessMaps,
    ) -> Result<(), BackendError> {
        let _removed = self.verified_pids.remove(&pid);
        let Some(state) = self.kernel_state.remove(&pid) else {
            let _removed = self.table.remove(pid);
            return Ok(());
        };
        for key in &state.keys {
            maps.remove_pid_mapping(key)?;
        }
        self.pid_page_entries = self.pid_page_entries.saturating_sub(state.keys.len());
        self.mapping_count = self
            .mapping_count
            .saturating_sub(state.state.mappings.len());
        for executable_id in state.executable_ids {
            self.release_executable(executable_id, maps)?;
        }
        let _removed = self.table.remove(pid);
        Ok(())
    }

    fn synchronize_mapping(
        &mut self,
        pid: u32,
        mapping: &ProcessMapping,
        maps: &mut impl ProcessMaps,
        keys: &mut Vec<PidPageKey>,
        executable_ids: &mut Vec<u64>,
    ) -> Result<(), BackendError> {
        let (id, layout) = self.read_executable(pid, mapping)?;
        let kernel_id = id.kernel_id();
        let elf_virtual = layout
            .virtual_address_for_file_offset(mapping.file_offset, self.page_size)
            .ok_or_else(|| {
                BackendError::process(
                    pid,
                    format!(
                        "file offset {:#x} is not covered by {}",
                        mapping.file_offset, mapping.path
                    ),
                )
            })?;
        let bias = mapping.start.checked_sub(elf_virtual).ok_or_else(|| {
            BackendError::process(
                pid,
                format!(
                    "mapping start {:#x} precedes ELF address {elf_virtual:#x}",
                    mapping.start
                ),
            )
        })?;
        let prefixes = calculate_prefixes(mapping.start, mapping.end)
            .map_err(|error| BackendError::process(pid, error.to_string()))?;
        if self.pid_page_entries.saturating_add(prefixes.len()) > self.limits.max_unwind_map_entries
        {
            self.losses.add(DropReason::MappingCapacity, 1);
            return Err(BackendError::Capacity("PID page capacity"));
        }
        let uploaded_new = if let Some(existing) = self.executables.get(&kernel_id) {
            if existing.id != id {
                return Err(BackendError::process(
                    pid,
                    format!("64-bit executable ID collision for {id}"),
                ));
            }
            false
        } else {
            if self.executables.len() == self.limits.max_executable_ids {
                self.losses.add(DropReason::ExecutableCapacity, 1);
                return Err(BackendError::Capacity("executable capacity"));
            }
            let remaining_deltas = self
                .limits
                .max_stack_delta_records
                .saturating_sub(self.stack_delta_records);
            let remaining_pages = self
                .limits
                .max_unwind_map_entries
                .saturating_sub(self.stack_page_entries);
            let remaining_bytes = self
                .limits
                .max_native_unwind_bytes
                .saturating_sub(self.unwind_bytes);
            let plan = FramePointerUnwindPlan::from_layout(
                &layout,
                remaining_deltas.min(256),
                remaining_pages,
                remaining_bytes,
            )
            .map_err(|reason| {
                self.losses.add(reason, 1);
                BackendError::Capacity(drop_reason_label(reason))
            })?;
            maps.upload_unwind_plan(id, &plan)?;
            self.unwind_bytes = self.unwind_bytes.saturating_add(plan.logical_bytes);
            self.stack_delta_records = self.stack_delta_records.saturating_add(plan.deltas.len());
            self.stack_page_entries = self.stack_page_entries.saturating_add(plan.pages.len());
            let _previous = self.executables.insert(
                kernel_id,
                UploadedExecutable {
                    id,
                    plan,
                    references: 0,
                },
            );
            true
        };

        let initial_key_count = keys.len();
        for prefix in prefixes {
            match maps.insert_pid_mapping(pid, prefix, kernel_id, bias) {
                Ok(key) => keys.push(key),
                Err(error) => {
                    for key in keys.drain(initial_key_count..) {
                        maps.remove_pid_mapping(&key)?;
                    }
                    if uploaded_new {
                        self.release_executable(kernel_id, maps)?;
                    }
                    return Err(error);
                }
            }
        }
        let inserted = keys.len() - initial_key_count;
        self.pid_page_entries = self.pid_page_entries.saturating_add(inserted);
        let executable = self
            .executables
            .get_mut(&kernel_id)
            .ok_or_else(|| BackendError::process(pid, "executable state disappeared"))?;
        executable.references = executable.references.saturating_add(1);
        executable_ids.push(kernel_id);
        Ok(())
    }

    fn read_executable(
        &self,
        pid: u32,
        mapping: &ProcessMapping,
    ) -> Result<(ExecutableId, ExecutableLayout), BackendError> {
        let (mut file, source) = open_mapping_file(&self.procfs_root, pid, mapping)?;
        let metadata = file
            .metadata()
            .map_err(|error| BackendError::process(pid, format!("mapping metadata: {error}")))?;
        let size = metadata.len();
        let matches_path_identity = metadata.ino() == mapping.inode
            && libc::major(metadata.dev()) == mapping.device_major
            && libc::minor(metadata.dev()) == mapping.device_minor;
        // map_files is the kernel's handle for this exact VMA. Overlayfs can
        // report its virtual device in fstat while maps reports the backing
        // device. Only a pathname fallback needs the device/inode comparison.
        if !metadata.is_file() || (source == MappingFileSource::RootPath && !matches_path_identity)
        {
            return Err(BackendError::process(
                pid,
                "opened file does not match mapped device and inode",
            ));
        }
        if size > self.limits.max_executable_file_bytes as u64 {
            return Err(BackendError::process(
                pid,
                format!(
                    "{} is {size} bytes, executable read limit is {}",
                    mapping.path, self.limits.max_executable_file_bytes
                ),
            ));
        }
        let bytes = crate::input::read_limited(
            &mut file,
            Path::new(&mapping.path),
            self.limits.max_executable_file_bytes,
        )?;
        let after = file.metadata().map_err(|error| {
            BackendError::process(pid, format!("mapping metadata after read: {error}"))
        })?;
        if metadata.len() != after.len()
            || metadata.mtime() != after.mtime()
            || metadata.mtime_nsec() != after.mtime_nsec()
            || metadata.ctime() != after.ctime()
            || metadata.ctime_nsec() != after.ctime_nsec()
        {
            return Err(BackendError::ProcessChanged { pid });
        }
        let id = ExecutableId::from_reader(&mut Cursor::new(&bytes))?;
        let layout = ExecutableLayout::parse(&bytes)?;
        Ok((id, layout))
    }

    fn release_executable(
        &mut self,
        kernel_id: u64,
        maps: &mut impl ProcessMaps,
    ) -> Result<(), BackendError> {
        let remove = if let Some(executable) = self.executables.get_mut(&kernel_id) {
            executable.references = executable.references.saturating_sub(1);
            executable.references == 0
        } else {
            false
        };
        if !remove {
            return Ok(());
        }
        if let Some(executable) = self.executables.remove(&kernel_id) {
            maps.remove_unwind_plan(executable.id, &executable.plan)?;
            self.unwind_bytes = self
                .unwind_bytes
                .saturating_sub(executable.plan.logical_bytes);
            self.stack_delta_records = self
                .stack_delta_records
                .saturating_sub(executable.plan.deltas.len());
            self.stack_page_entries = self
                .stack_page_entries
                .saturating_sub(executable.plan.pages.len());
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MappingFileSource {
    MapFiles,
    RootPath,
}

fn open_mapping_file(
    procfs_root: &Path,
    pid: u32,
    mapping: &ProcessMapping,
) -> Result<(std::fs::File, MappingFileSource), BackendError> {
    let pid_root = procfs_root.join(pid.to_string());
    let map_file = pid_root
        .join("map_files")
        .join(format!("{:x}-{:x}", mapping.start, mapping.end));
    match std::fs::File::open(&map_file) {
        Ok(file) => return Ok((file, MappingFileSource::MapFiles)),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
            ) => {}
        Err(source) => {
            return Err(BackendError::Io {
                path: map_file,
                source,
            });
        }
    }
    let relative = mapping.path.strip_prefix('/').unwrap_or(&mapping.path);
    let root_path = pid_root.join("root").join(relative);
    std::fs::File::open(&root_path)
        .map(|file| (file, MappingFileSource::RootPath))
        .map_err(|source| BackendError::Io {
            path: root_path,
            source,
        })
}

const fn drop_reason_label(reason: DropReason) -> &'static str {
    match reason {
        DropReason::ProcessCapacity => "process capacity",
        DropReason::MappingCapacity => "mapping capacity",
        DropReason::ExecutableCapacity => "executable capacity",
        DropReason::UnwindByteCapacity => "native unwind byte capacity",
        DropReason::StackDeltaCapacity => "stack delta capacity",
        _ => "bounded native state",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[derive(Default)]
    struct MemoryMaps {
        mappings: BTreeMap<(u32, [u8; 12]), (u64, u64)>,
        uploads: BTreeSet<ExecutableId>,
    }

    impl ProcessMaps for MemoryMaps {
        fn drain_pid_events(&mut self, _maximum: usize) -> Result<Vec<u64>, BackendError> {
            Ok(Vec::new())
        }

        fn remove_reported_pid(&mut self, _pid: u32) -> Result<(), BackendError> {
            Ok(())
        }

        fn insert_pid_mapping(
            &mut self,
            pid: u32,
            prefix: LpmPrefix,
            executable_id: u64,
            bias: u64,
        ) -> Result<PidPageKey, BackendError> {
            let mut data = [0_u8; 12];
            data[..4].copy_from_slice(&pid.to_be_bytes());
            data[4..].copy_from_slice(&prefix.key.to_be_bytes());
            let key = PidPageKey::new(32 + prefix.length, data);
            let _previous = self
                .mappings
                .insert((key.prefix_len(), key.data()), (executable_id, bias));
            Ok(key)
        }

        fn remove_pid_mapping(&mut self, key: &PidPageKey) -> Result<(), BackendError> {
            let _removed = self.mappings.remove(&(key.prefix_len(), key.data()));
            Ok(())
        }

        fn upload_unwind_plan(
            &mut self,
            executable_id: ExecutableId,
            _plan: &FramePointerUnwindPlan,
        ) -> Result<(), BackendError> {
            assert!(
                self.uploads.insert(executable_id),
                "duplicate executable upload"
            );
            Ok(())
        }

        fn remove_unwind_plan(
            &mut self,
            executable_id: ExecutableId,
            _plan: &FramePointerUnwindPlan,
        ) -> Result<(), BackendError> {
            assert!(
                self.uploads.remove(&executable_id),
                "missing executable upload"
            );
            Ok(())
        }
    }

    fn executable_fixture(path: &Path, marker: u8) {
        let mut bytes = vec![0_u8; 4096];
        bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
        bytes[16..18].copy_from_slice(&3_u16.to_le_bytes());
        bytes[18..20].copy_from_slice(&62_u16.to_le_bytes());
        bytes[20..24].copy_from_slice(&1_u32.to_le_bytes());
        bytes[24..32].copy_from_slice(&4096_u64.to_le_bytes());
        bytes[32..40].copy_from_slice(&64_u64.to_le_bytes());
        bytes[52..54].copy_from_slice(&64_u16.to_le_bytes());
        bytes[54..56].copy_from_slice(&56_u16.to_le_bytes());
        bytes[56..58].copy_from_slice(&1_u16.to_le_bytes());
        bytes[64..68].copy_from_slice(&1_u32.to_le_bytes());
        bytes[68..72].copy_from_slice(&5_u32.to_le_bytes());
        for offset in [80, 88, 96, 104, 112] {
            bytes[offset..offset + 8].copy_from_slice(&4096_u64.to_le_bytes());
        }
        bytes[256] = marker;
        std::fs::write(path, bytes).expect("synthetic executable");
    }

    fn process_fixture(procfs: &Path, pid: u32, executable: &Path) {
        let directory = procfs.join(pid.to_string());
        std::fs::create_dir_all(&directory).expect("process directory");
        std::fs::write(
            directory.join("stat"),
            format!("{pid} (fixture) R {}100\n", "0 ".repeat(18)),
        )
        .expect("stat fixture");
        std::os::unix::fs::symlink(executable, directory.join("exe")).expect("exe link");
        std::os::unix::fs::symlink("/", directory.join("root")).expect("read-only root view");
        mapping_fixture(procfs, pid, executable);
    }

    fn mapping_fixture(procfs: &Path, pid: u32, executable: &Path) {
        let directory = procfs.join(pid.to_string());
        let metadata = std::fs::metadata(executable).expect("executable metadata");
        std::fs::write(
            directory.join("maps"),
            format!(
                "10001000-10002000 r-xp 00000000 {:x}:{:x} {} {}\n",
                libc::major(metadata.dev()),
                libc::minor(metadata.dev()),
                metadata.ino(),
                executable.display(),
            ),
        )
        .expect("maps fixture");
    }

    /// Scenario: The current process's mapped ELF is opened through procfs without BPF privileges.
    /// Guarantees: File identity and executable layout derive from the same bounded bytes and match the mapped inode.
    #[test]
    fn mapped_executable_is_read_without_kernel_loading() {
        let pid = std::process::id();
        let root = Path::new("/proc");
        let (state, _, _) = read_process_state(root, pid, 4096, 8 * 1024 * 1024).expect("process");
        let executable = std::env::current_exe().expect("test executable");
        let mapping = state
            .mappings
            .iter()
            .find(|mapping| Path::new(&mapping.path) == executable)
            .expect("mapped test executable");
        let synchronizer = ProcessSynchronizer::new(
            root.to_owned(),
            ResourceLimits::default(),
            super::super::syscall::page_size().expect("page size"),
            KernelClock::collect().expect("kernel clock"),
        );
        let (id, layout) = synchronizer
            .read_executable(pid, mapping)
            .expect("bounded metadata");
        assert_eq!(id, ExecutableId::from_path(&executable).expect("file ID"));
        assert!(!layout.executable_ranges().is_empty());
    }

    /// Scenario: P2's metadata cannot fit until P1 exits, while P2's procfs mappings stay unchanged.
    /// Guarantees: A later bounded refresh retries P2's incomplete upload after capacity becomes available.
    #[test]
    fn incomplete_metadata_recovers_after_capacity_is_freed() {
        let temporary = tempfile::tempdir().expect("fixture directory");
        let root = temporary.path();
        let first = root.join("first.elf");
        let second = root.join("second.elf");
        executable_fixture(&first, 1);
        executable_fixture(&second, 2);
        process_fixture(root, 1, &first);
        process_fixture(root, 2, &second);
        let limits = ResourceLimits {
            max_executable_ids: 1,
            ..ResourceLimits::default()
        };
        let mut synchronizer = ProcessSynchronizer::new(
            root.to_owned(),
            limits,
            4096,
            KernelClock::collect().expect("kernel clock"),
        );
        let mut maps = MemoryMaps::default();
        synchronizer
            .synchronize_pid(1, &mut maps)
            .expect("first process");
        synchronizer
            .synchronize_pid(2, &mut maps)
            .expect("partial second process");
        assert!(synchronizer.kernel_state[&1].complete);
        assert!(!synchronizer.kernel_state[&2].complete);
        assert_eq!(maps.uploads.len(), 1);
        synchronizer
            .remove_process(1, &mut maps)
            .expect("free first process");
        synchronizer.begin_drain();
        synchronizer
            .synchronize_pid(2, &mut maps)
            .expect("retry unchanged process");
        assert!(synchronizer.kernel_state[&2].complete);
        assert_eq!(maps.uploads.len(), 1);
        assert!(
            maps.uploads
                .contains(&ExecutableId::from_path(&second).expect("second ID"))
        );
        assert_eq!(
            synchronizer
                .take_losses()
                .get(DropReason::ExecutableCapacity),
            1
        );
    }

    /// Scenario: An executable is temporarily unavailable although its process mappings do not change.
    /// Guarantees: Resolving the read failure permits a subsequent refresh to install the missing metadata.
    #[test]
    fn incomplete_metadata_recovers_after_transient_file_failure() {
        let temporary = tempfile::tempdir().expect("fixture directory");
        let root = temporary.path();
        let executable = root.join("process.elf");
        let hidden = root.join("unavailable.elf");
        executable_fixture(&executable, 3);
        process_fixture(root, 1, &executable);
        std::fs::rename(&executable, &hidden).expect("temporarily hide executable");
        let mut synchronizer = ProcessSynchronizer::new(
            root.to_owned(),
            ResourceLimits::default(),
            4096,
            KernelClock::collect().expect("kernel clock"),
        );
        let mut maps = MemoryMaps::default();
        synchronizer
            .synchronize_pid(1, &mut maps)
            .expect("partial process");
        assert!(!synchronizer.kernel_state[&1].complete);
        assert!(maps.uploads.is_empty());
        std::fs::rename(&hidden, &executable).expect("restore executable");
        synchronizer.begin_drain();
        synchronizer
            .synchronize_pid(1, &mut maps)
            .expect("retry file read");
        assert!(synchronizer.kernel_state[&1].complete);
        assert_eq!(maps.uploads.len(), 1);
    }

    /// Scenario: A kernel map_files handle has a virtual device number different from the maps backing device.
    /// Guarantees: Authoritative VMA handles remain usable while stale pathname fallbacks are still rejected.
    #[test]
    fn map_files_authority_allows_overlay_device_translation() {
        let temporary = tempfile::tempdir().expect("fixture directory");
        let root = temporary.path();
        let executable = root.join("overlay.elf");
        executable_fixture(&executable, 4);
        process_fixture(root, 1, &executable);
        let (state, _, _) = read_process_state(root, 1, 16, 8192).expect("process");
        let mut mapping = state.mappings[0].clone();
        mapping.device_major ^= 1;
        let synchronizer = ProcessSynchronizer::new(
            root.to_owned(),
            ResourceLimits::default(),
            4096,
            KernelClock::collect().expect("kernel clock"),
        );
        assert!(
            synchronizer.read_executable(1, &mapping).is_err(),
            "pathname fallback must verify identity"
        );
        let map_files = root.join("1/map_files");
        std::fs::create_dir(&map_files).expect("map_files directory");
        std::os::unix::fs::symlink(
            &executable,
            map_files.join(format!("{:x}-{:x}", mapping.start, mapping.end)),
        )
        .expect("authoritative VMA handle");
        let (id, _) = synchronizer
            .read_executable(1, &mapping)
            .expect("overlay VMA");
        assert_eq!(id, ExecutableId::from_path(&executable).expect("file ID"));
    }

    /// Scenario: An old queued trace is processed after the same PID acquires a new start time.
    /// Guarantees: The old trace is counted and rejected, while a later trace can use the new identity.
    #[test]
    fn queued_trace_is_not_attributed_to_reused_pid() {
        let temporary = tempfile::tempdir().expect("fixture directory");
        let root = temporary.path();
        let executable = root.join("reused.elf");
        executable_fixture(&executable, 5);
        process_fixture(root, 1, &executable);
        let clock = KernelClock::collect().expect("clock");
        let mut synchronizer =
            ProcessSynchronizer::new(root.to_owned(), ResourceLimits::default(), 4096, clock);
        let mut maps = MemoryMaps::default();
        synchronizer
            .synchronize_pid(1, &mut maps)
            .expect("old generation");
        let old_trace_time = clock.now().expect("old trace time");
        std::fs::write(
            root.join("1/stat"),
            format!("1 (fixture) R {}200\n", "0 ".repeat(18)),
        )
        .expect("PID reuse");
        synchronizer.begin_drain();
        assert!(
            synchronizer
                .identity_for_trace(1, old_trace_time, &mut maps, true)
                .expect("revalidate")
                .is_none()
        );
        let new_trace_time = clock.now().expect("new trace time");
        synchronizer.begin_drain();
        let identity = synchronizer
            .identity_for_trace(1, new_trace_time, &mut maps, true)
            .expect("new generation")
            .expect("accepted new trace");
        assert_eq!(identity.start_time_ticks, 200);
        assert_eq!(
            synchronizer
                .take_losses()
                .get(DropReason::ProcessGenerationMismatch),
            1
        );
    }

    /// Scenario: Executable mappings change without changing the PID or its start-time tick.
    /// Guarantees: A fresh metadata epoch rejects traces queued before the mapping replacement.
    #[test]
    fn mapping_change_starts_a_new_admission_window() {
        let temporary = tempfile::tempdir().expect("fixture directory");
        let root = temporary.path();
        let first = root.join("before.elf");
        let second = root.join("after.elf");
        executable_fixture(&first, 6);
        executable_fixture(&second, 7);
        process_fixture(root, 1, &first);
        let clock = KernelClock::collect().expect("clock");
        let mut synchronizer =
            ProcessSynchronizer::new(root.to_owned(), ResourceLimits::default(), 4096, clock);
        let mut maps = MemoryMaps::default();
        synchronizer
            .synchronize_pid(1, &mut maps)
            .expect("initial mappings");
        let queued = clock.now().expect("queued trace");
        mapping_fixture(root, 1, &second);
        synchronizer.begin_drain();
        assert!(
            synchronizer
                .identity_for_trace(1, queued, &mut maps, true)
                .expect("mapping refresh")
                .is_none()
        );
        assert!(
            maps.uploads
                .contains(&ExecutableId::from_path(&second).expect("new executable"))
        );
        assert!(
            !maps
                .uploads
                .contains(&ExecutableId::from_path(&first).expect("old executable"))
        );
    }

    /// Scenario: Shutdown drains without allowing any further procfs access.
    /// Guarantees: Only previously verified time intervals are accepted, even after procfs data is unavailable.
    #[test]
    fn shutdown_attribution_uses_only_verified_intervals() {
        let temporary = tempfile::tempdir().expect("fixture directory");
        let root = temporary.path();
        let executable = root.join("shutdown.elf");
        executable_fixture(&executable, 8);
        process_fixture(root, 1, &executable);
        let clock = KernelClock::collect().expect("clock");
        let mut synchronizer =
            ProcessSynchronizer::new(root.to_owned(), ResourceLimits::default(), 4096, clock);
        let mut maps = MemoryMaps::default();
        synchronizer
            .synchronize_pid(1, &mut maps)
            .expect("metadata");
        let admitted = synchronizer.kernel_state[&1].admitted_at_ns;
        std::fs::remove_file(root.join("1/stat")).expect("remove procfs access");
        synchronizer.begin_drain();
        assert!(
            synchronizer
                .identity_for_trace(1, admitted, &mut maps, false)
                .expect("cached interval")
                .is_some()
        );
        assert!(
            synchronizer
                .identity_for_trace(1, admitted + 1, &mut maps, false)
                .expect("unverified interval")
                .is_none()
        );
        assert_eq!(
            synchronizer
                .take_losses()
                .get(DropReason::ProcessGenerationMismatch),
            1
        );
    }

    /// Scenario: One executable remains over capacity while another mapping of the same process is usable.
    /// Guarantees: Retrying incomplete metadata preserves the generation's admission window and valid queued samples.
    #[test]
    fn incomplete_retry_preserves_admitted_samples() {
        use std::io::Write;
        let temporary = tempfile::tempdir().expect("fixture directory");
        let root = temporary.path();
        let first = root.join("usable.elf");
        let second = root.join("over-capacity.elf");
        executable_fixture(&first, 9);
        executable_fixture(&second, 10);
        process_fixture(root, 1, &first);
        let metadata = std::fs::metadata(&second).expect("second metadata");
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(root.join("1/maps"))
            .expect("maps");
        writeln!(
            file,
            "20001000-20002000 r-xp 00000000 {:x}:{:x} {} {}",
            libc::major(metadata.dev()),
            libc::minor(metadata.dev()),
            metadata.ino(),
            second.display()
        )
        .expect("second mapping");
        drop(file);
        let clock = KernelClock::collect().expect("clock");
        let limits = ResourceLimits {
            max_executable_ids: 1,
            ..ResourceLimits::default()
        };
        let mut synchronizer = ProcessSynchronizer::new(root.to_owned(), limits, 4096, clock);
        let mut maps = MemoryMaps::default();
        synchronizer
            .synchronize_pid(1, &mut maps)
            .expect("partial metadata");
        assert!(!synchronizer.kernel_state[&1].complete);
        let admission = synchronizer.kernel_state[&1].admitted_at_ns;
        let queued = clock.now().expect("queued sample");
        synchronizer.begin_drain();
        synchronizer
            .synchronize_pid(1, &mut maps)
            .expect("retry same generation");
        assert_eq!(synchronizer.kernel_state[&1].admitted_at_ns, admission);
        assert!(
            synchronizer
                .identity_for_trace(1, queued, &mut maps, true)
                .expect("identity")
                .is_some()
        );
        assert_eq!(
            synchronizer
                .take_losses()
                .get(DropReason::ProcessGenerationMismatch),
            0
        );
    }
}
