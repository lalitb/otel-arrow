// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

use aya::Ebpf;
use aya::maps::lpm_trie::{Key as LpmKey, LpmTrie};
use aya::maps::{Array, HashMap as AyaHashMap, HashOfMaps, Map, MapData, MapError};
use std::collections::BTreeMap;

use crate::error::BackendError;
use crate::mappings::LpmPrefix;
use crate::unwind::{ExecutableId, FramePointerUnwindPlan};

pub(crate) type PidPageKey = LpmKey<[u8; 12]>;
type StackDeltaArray = Array<MapData, [u8; 4]>;

pub(crate) trait ProcessMaps {
    fn drain_pid_events(&mut self, maximum: usize) -> Result<Vec<u64>, BackendError>;
    fn remove_reported_pid(&mut self, pid: u32) -> Result<(), BackendError>;
    fn insert_pid_mapping(
        &mut self,
        pid: u32,
        prefix: LpmPrefix,
        executable_id: u64,
        bias: u64,
    ) -> Result<PidPageKey, BackendError>;
    fn remove_pid_mapping(&mut self, key: &PidPageKey) -> Result<(), BackendError>;
    fn upload_unwind_plan(
        &mut self,
        executable_id: ExecutableId,
        plan: &FramePointerUnwindPlan,
    ) -> Result<(), BackendError>;
    fn remove_unwind_plan(
        &mut self,
        executable_id: ExecutableId,
        plan: &FramePointerUnwindPlan,
    ) -> Result<(), BackendError>;
}

#[derive(Debug)]
pub(crate) struct KernelMaps {
    pid_page_to_mapping_info: LpmTrie<MapData, [u8; 12], [u8; 16]>,
    pid_events: AyaHashMap<MapData, [u8; 8], [u8; 1]>,
    reported_pids: AyaHashMap<MapData, [u8; 4], [u8; 8]>,
    inhibit_events: AyaHashMap<MapData, [u8; 4], [u8; 1]>,
    stack_delta_page_to_info: AyaHashMap<MapData, [u8; 16], [u8; 8]>,
    stack_delta_outer: BTreeMap<u16, HashOfMaps<MapData, [u8; 8], StackDeltaArray>>,
}

impl KernelMaps {
    pub(crate) fn take(bpf: &mut Ebpf) -> Result<Self, BackendError> {
        let pid_page_to_mapping_info =
            LpmTrie::try_from(take_map(bpf, "pid_page_to_mapping_info")?)
                .map_err(|error| map_error("convert pid_page_to_mapping_info", error))?;
        let pid_events = AyaHashMap::try_from(take_map(bpf, "pid_events")?)
            .map_err(|error| map_error("convert pid_events", error))?;
        let reported_pids = AyaHashMap::try_from(take_map(bpf, "reported_pids")?)
            .map_err(|error| map_error("convert reported_pids", error))?;
        let inhibit_events = AyaHashMap::try_from(take_map(bpf, "inhibit_events")?)
            .map_err(|error| map_error("convert inhibit_events", error))?;
        let stack_delta_page_to_info =
            AyaHashMap::try_from(take_map(bpf, "stack_delta_page_to_info")?)
                .map_err(|error| map_error("convert stack_delta_page_to_info", error))?;
        let mut stack_delta_outer = BTreeMap::new();
        for map_id in 8_u16..=23 {
            let name = format!("exe_id_to_{map_id}_stack_deltas");
            let outer = HashOfMaps::try_from(take_map(bpf, &name)?)
                .map_err(|error| map_error("convert stack-delta outer map", error))?;
            let _previous = stack_delta_outer.insert(map_id, outer);
        }
        Ok(Self {
            pid_page_to_mapping_info,
            pid_events,
            reported_pids,
            inhibit_events,
            stack_delta_page_to_info,
            stack_delta_outer,
        })
    }
}

impl ProcessMaps for KernelMaps {
    fn drain_pid_events(&mut self, maximum: usize) -> Result<Vec<u64>, BackendError> {
        let mut keys = Vec::with_capacity(maximum);
        for key in self.pid_events.keys().take(maximum) {
            let key = key.map_err(|error| map_error("iterate pid_events", error))?;
            keys.push(u64::from_le_bytes(key));
        }
        keys.sort_unstable();
        keys.dedup();
        for key in &keys {
            removed_or_absent(
                self.pid_events.remove(&key.to_le_bytes()),
                "remove pid_events",
            )?;
        }
        let event_type = 1_u32.to_le_bytes();
        removed_or_absent(
            self.inhibit_events.remove(&event_type),
            "release PID event inhibition",
        )?;
        Ok(keys)
    }

    fn remove_reported_pid(&mut self, pid: u32) -> Result<(), BackendError> {
        removed_or_absent(
            self.reported_pids.remove(&pid.to_le_bytes()),
            "remove reported PID",
        )
    }

    fn insert_pid_mapping(
        &mut self,
        pid: u32,
        prefix: LpmPrefix,
        executable_id: u64,
        bias: u64,
    ) -> Result<PidPageKey, BackendError> {
        if bias >> 56 != 0 {
            return Err(BackendError::process(
                pid,
                format!("mapping bias {bias:#x} exceeds 56-bit ABI"),
            ));
        }
        let mut data = [0_u8; 12];
        data[..4].copy_from_slice(&pid.to_be_bytes());
        data[4..].copy_from_slice(&prefix.key.to_be_bytes());
        let key = LpmKey::new(32 + prefix.length, data);
        let mut value = [0_u8; 16];
        value[..8].copy_from_slice(&executable_id.to_le_bytes());
        value[8..].copy_from_slice(&(bias | (1_u64 << 56)).to_le_bytes());
        self.pid_page_to_mapping_info
            .insert(&key, value, 0)
            .map_err(|error| map_error("insert pid_page_to_mapping_info", error))?;
        Ok(key)
    }

    fn remove_pid_mapping(&mut self, key: &PidPageKey) -> Result<(), BackendError> {
        removed_or_absent(
            self.pid_page_to_mapping_info.remove(key),
            "remove PID mapping",
        )
    }

    fn upload_unwind_plan(
        &mut self,
        executable_id: ExecutableId,
        plan: &FramePointerUnwindPlan,
    ) -> Result<(), BackendError> {
        let outer = self
            .stack_delta_outer
            .get_mut(&plan.map_id)
            .ok_or_else(|| {
                BackendError::kernel(
                    "select stack-delta outer map",
                    format!("unsupported bucket {}", plan.map_id),
                )
            })?;
        let max_entries = 1_u32
            .checked_shl(u32::from(plan.map_id))
            .ok_or_else(|| BackendError::kernel("create inner map", "bucket shift overflow"))?;
        let mut inner = StackDeltaArray::create(max_entries, 0)
            .map_err(|error| map_error("create stack-delta inner map", error))?;
        for (index, delta) in plan.deltas.iter().enumerate() {
            let index = u32::try_from(index)
                .map_err(|_| BackendError::kernel("populate inner map", "index overflow"))?;
            let mut value = [0_u8; 4];
            value[..2].copy_from_slice(&delta.address_low.to_le_bytes());
            value[2..].copy_from_slice(&delta.unwind_info.to_le_bytes());
            inner
                .set(index, value, 0)
                .map_err(|error| map_error("populate inner map", error))?;
        }
        let file_id = executable_id.kernel_id();
        outer
            .insert(file_id.to_le_bytes(), &inner, 0)
            .map_err(|error| map_error("insert stack-delta inner map", error))?;

        let mut inserted_pages = Vec::new();
        for page in &plan.pages {
            let mut key = [0_u8; 16];
            key[..8].copy_from_slice(&file_id.to_le_bytes());
            key[8..].copy_from_slice(&page.page.to_le_bytes());
            let mut value = [0_u8; 8];
            value[..4].copy_from_slice(&page.first_delta.to_le_bytes());
            value[4..6].copy_from_slice(&page.delta_count.to_le_bytes());
            value[6..].copy_from_slice(&page.map_id.to_le_bytes());
            if let Err(error) = self.stack_delta_page_to_info.insert(key, value, 0) {
                for inserted in &inserted_pages {
                    removed_or_absent(
                        self.stack_delta_page_to_info.remove(inserted),
                        "rollback stack-delta page",
                    )?;
                }
                removed_or_absent(outer.remove(&file_id.to_le_bytes()), "rollback inner map")?;
                return Err(map_error("insert stack_delta_page_to_info", error));
            }
            inserted_pages.push(key);
        }
        Ok(())
    }

    fn remove_unwind_plan(
        &mut self,
        executable_id: ExecutableId,
        plan: &FramePointerUnwindPlan,
    ) -> Result<(), BackendError> {
        let file_id = executable_id.kernel_id();
        for page in &plan.pages {
            let mut key = [0_u8; 16];
            key[..8].copy_from_slice(&file_id.to_le_bytes());
            key[8..].copy_from_slice(&page.page.to_le_bytes());
            removed_or_absent(
                self.stack_delta_page_to_info.remove(&key),
                "remove stack-delta page",
            )?;
        }
        if let Some(outer) = self.stack_delta_outer.get_mut(&plan.map_id) {
            removed_or_absent(outer.remove(&file_id.to_le_bytes()), "remove inner map")?;
        }
        Ok(())
    }
}

fn take_map(bpf: &mut Ebpf, name: &str) -> Result<Map, BackendError> {
    bpf.take_map(name)
        .ok_or_else(|| BackendError::kernel("take map", format!("{name} is absent")))
}

fn removed_or_absent(
    result: Result<(), MapError>,
    operation: &'static str,
) -> Result<(), BackendError> {
    match result {
        Ok(()) | Err(MapError::KeyNotFound) => Ok(()),
        Err(MapError::SyscallError(error))
            if error.io_error.raw_os_error() == Some(libc::ENOENT) =>
        {
            Ok(())
        }
        Err(error) => Err(map_error(operation, error)),
    }
}

fn map_error(operation: &'static str, error: impl std::fmt::Display) -> BackendError {
    BackendError::kernel(operation, error.to_string())
}
