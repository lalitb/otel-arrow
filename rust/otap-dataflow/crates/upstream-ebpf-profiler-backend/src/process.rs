// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! PID identity and bounded process metadata state.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::BackendError;
use crate::mappings::{ProcessMapping, parse_executable_mappings};
use crate::statistics::{DropReason, LossCounters};

const MAX_PROC_STAT_BYTES: usize = 4096;
/// Hard bound on a retained executable pathname.
pub const MAX_EXECUTABLE_PATH_BYTES: usize = 8192;

/// PID plus Linux start time, which remains stable across PID reuse.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct ProcessIdentity {
    /// Linux process ID.
    pub pid: u32,
    /// `/proc/<pid>/stat` field 22 in clock ticks since boot.
    pub start_time_ticks: u64,
    /// Best-effort executable symlink target.
    pub executable: Option<String>,
}

/// Owned process metadata retained by the synchronizer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessState {
    /// Stable process identity.
    pub identity: ProcessIdentity,
    /// Bounded executable mappings.
    pub mappings: Vec<ProcessMapping>,
}

/// Result of inserting a process identity into bounded state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessUpdate {
    /// A previously unseen PID was inserted.
    Inserted,
    /// Existing metadata with the same identity was replaced.
    Refreshed,
    /// The PID was reused and old metadata was replaced.
    Reused,
}

/// Bounded process table with explicit PID reuse handling.
#[derive(Debug)]
pub struct ProcessTable {
    capacity: usize,
    processes: BTreeMap<u32, ProcessState>,
    losses: LossCounters,
}

impl ProcessTable {
    /// Creates a process table with a fixed PID count.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            processes: BTreeMap::new(),
            losses: LossCounters::default(),
        }
    }

    /// Inserts or refreshes process state.
    pub fn upsert(&mut self, state: ProcessState) -> Result<ProcessUpdate, DropReason> {
        let pid = state.identity.pid;
        let update = match self.processes.get(&pid) {
            Some(previous)
                if previous.identity.start_time_ticks != state.identity.start_time_ticks =>
            {
                ProcessUpdate::Reused
            }
            Some(_) => ProcessUpdate::Refreshed,
            None if self.processes.len() == self.capacity => {
                self.losses.add(DropReason::ProcessCapacity, 1);
                return Err(DropReason::ProcessCapacity);
            }
            None => ProcessUpdate::Inserted,
        };
        let _previous = self.processes.insert(pid, state);
        Ok(update)
    }

    /// Removes and returns one process.
    pub fn remove(&mut self, pid: u32) -> Option<ProcessState> {
        self.processes.remove(&pid)
    }

    /// Removes a process only when its start time still matches an exit event.
    pub fn remove_if_start_time(
        &mut self,
        pid: u32,
        expected_start_time_ticks: u64,
    ) -> Option<ProcessState> {
        if self
            .identity(pid)
            .is_some_and(|identity| identity.start_time_ticks == expected_start_time_ticks)
        {
            self.remove(pid)
        } else {
            None
        }
    }

    /// Looks up a stable identity by PID.
    #[must_use]
    pub fn identity(&self, pid: u32) -> Option<&ProcessIdentity> {
        self.processes.get(&pid).map(|state| &state.identity)
    }

    /// Looks up complete state by PID.
    #[must_use]
    pub fn state(&self, pid: u32) -> Option<&ProcessState> {
        self.processes.get(&pid)
    }

    /// Returns tracked process count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.processes.len()
    }

    /// Returns whether no process is tracked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.processes.is_empty()
    }

    /// Returns process-table capacity losses.
    #[must_use]
    pub fn losses(&self) -> &LossCounters {
        &self.losses
    }
}

/// Reads one process identity from a configurable procfs root.
pub fn read_process_identity(
    procfs_root: &Path,
    pid: u32,
) -> Result<ProcessIdentity, BackendError> {
    let stat_path = procfs_root.join(pid.to_string()).join("stat");
    let stat = crate::input::read_file(&stat_path, MAX_PROC_STAT_BYTES)?;
    let stat = std::str::from_utf8(&stat)
        .map_err(|error| BackendError::process(pid, format!("stat is not UTF-8: {error}")))?;
    let close = stat
        .rfind(')')
        .ok_or_else(|| BackendError::process(pid, "stat command terminator is absent"))?;
    let remainder = stat
        .get(close + 1..)
        .ok_or_else(|| BackendError::process(pid, "stat field range is invalid"))?;
    let start_time = remainder
        .split_whitespace()
        .nth(19)
        .ok_or_else(|| BackendError::process(pid, "stat start-time field is absent"))?
        .parse::<u64>()
        .map_err(|error| BackendError::process(pid, format!("invalid start time: {error}")))?;
    let exe_path = procfs_root.join(pid.to_string()).join("exe");
    let executable = match std::fs::read_link(&exe_path) {
        Ok(path) => {
            let path = path.to_string_lossy();
            if path.len() > MAX_EXECUTABLE_PATH_BYTES {
                return Err(BackendError::InputTooLarge {
                    path: exe_path,
                    maximum: MAX_EXECUTABLE_PATH_BYTES,
                });
            }
            Some(path.trim_end_matches(" (deleted)").to_owned())
        }
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
            ) =>
        {
            None
        }
        Err(source) => {
            return Err(BackendError::Io {
                path: exe_path,
                source,
            });
        }
    };
    Ok(ProcessIdentity {
        pid,
        start_time_ticks: start_time,
        executable,
    })
}

/// Reads bounded executable mappings for one process.
pub fn read_process_state(
    procfs_root: &Path,
    pid: u32,
    mapping_capacity: usize,
    maximum_bytes: usize,
) -> Result<(ProcessState, u64, u64), BackendError> {
    let identity = read_process_identity(procfs_root, pid)?;
    let maps_path = procfs_root.join(pid.to_string()).join("maps");
    let maps = crate::input::read_file(&maps_path, maximum_bytes)?;
    let maps = std::str::from_utf8(&maps)
        .map_err(|error| BackendError::process(pid, format!("maps is not UTF-8: {error}")))?;
    let (mappings, parse_errors, capacity_drops) =
        parse_executable_mappings(maps, mapping_capacity);
    if identity != read_process_identity(procfs_root, pid)? {
        return Err(BackendError::ProcessChanged { pid });
    }
    Ok((
        ProcessState { identity, mappings },
        parse_errors,
        capacity_drops,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(pid: u32, start_time_ticks: u64) -> ProcessState {
        ProcessState {
            identity: ProcessIdentity {
                pid,
                start_time_ticks,
                executable: None,
            },
            mappings: Vec::new(),
        }
    }

    /// Scenario: Linux reuses a PID for a process with a different start time.
    /// Guarantees: New state replaces the stale identity and reports the reuse
    /// transition instead of merging samples from two processes.
    #[test]
    fn pid_reuse_replaces_state() {
        let mut table = ProcessTable::new(1);
        assert_eq!(table.upsert(state(7, 10)), Ok(ProcessUpdate::Inserted));
        assert_eq!(table.upsert(state(7, 11)), Ok(ProcessUpdate::Reused));
        assert_eq!(table.identity(7).expect("identity").start_time_ticks, 11);
    }

    /// Scenario: A second distinct PID arrives after process capacity is full.
    /// Guarantees: Existing metadata remains intact and the rejection is
    /// represented by a fixed process-capacity reason.
    #[test]
    fn process_capacity_is_enforced() {
        let mut table = ProcessTable::new(1);
        let _update = table.upsert(state(7, 10)).expect("first");
        assert_eq!(table.upsert(state(8, 10)), Err(DropReason::ProcessCapacity));
        assert!(table.identity(7).is_some());
        assert!(table.identity(8).is_none());
    }

    /// Scenario: A delayed exit observation for an old PID generation arrives
    /// after the PID has been reused.
    /// Guarantees: Start-time guarded cleanup does not remove metadata for the
    /// newer process generation.
    #[test]
    fn stale_exit_does_not_remove_reused_pid() {
        let mut table = ProcessTable::new(1);
        let _inserted = table.upsert(state(7, 10)).expect("old process");
        let _reused = table.upsert(state(7, 11)).expect("new process");
        assert!(table.remove_if_start_time(7, 10).is_none());
        assert_eq!(table.identity(7).expect("new process").start_time_ticks, 11);
    }

    /// Scenario: The current test process is read through real procfs.
    /// Guarantees: Start-time identity and at least one executable mapping can
    /// be obtained without root or BPF privileges.
    #[test]
    #[cfg(target_os = "linux")]
    fn current_process_metadata_is_readable() {
        let pid = std::process::id();
        let (state, _, _) = read_process_state(Path::new("/proc"), pid, 4096, 8 * 1024 * 1024)
            .expect("current process");
        assert_eq!(state.identity.pid, pid);
        assert!(state.identity.start_time_ticks > 0);
        assert!(!state.mappings.is_empty());
    }
}
