// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Bounded reuse of the upstream task-structure analysis program.

use aya::EbpfLoader;
use aya::maps::{Array, MapData};
use aya::programs::RawTracePoint;
use std::os::unix::fs::MetadataExt;

use crate::artifact::UpstreamArtifact;
use crate::error::BackendError;
use crate::linux::system_config::KernelStackLayout;
use crate::preparation::PreparedObject;

const RECORD_SIZE: usize = 144;
const CODE_SIZE: usize = 128;
const MAX_TASK_BYTES: usize = 8 * 1024;
const MAX_STACK_BYTES: u64 = 64 * 1024;

#[repr(C)]
struct AnalysisLayout {
    address: u64,
    pid: u32,
    error: i32,
    code: [u8; CODE_SIZE],
}

const _: () = {
    assert!(size_of::<AnalysisLayout>() == RECORD_SIZE);
    assert!(std::mem::offset_of!(AnalysisLayout, pid) == 8);
    assert!(std::mem::offset_of!(AnalysisLayout, error) == 12);
    assert!(std::mem::offset_of!(AnalysisLayout, code) == 16);
};

struct AnalysisRead {
    registers: u64,
    code: [u8; CODE_SIZE],
}

pub(crate) fn discover(
    artifact: &UpstreamArtifact,
    btf: &aya::Btf,
    page_size: usize,
    after_resources: impl FnOnce() -> Result<(), BackendError>,
) -> Result<KernelStackLayout, BackendError> {
    let namespace = std::fs::metadata("/proc/self/ns/pid").map_err(|source| BackendError::Io {
        path: "/proc/self/ns/pid".into(),
        source,
    })?;
    let namespace_enabled = 1_u8;
    let namespace_device = namespace.dev();
    let namespace_inode = namespace.ino();
    let prepared = PreparedObject::thread_scoped_analysis(artifact)?;
    let mut loader = EbpfLoader::new();
    let _loader = loader
        .btf(Some(btf))
        .override_global("pid_ns_translation_enabled", &namespace_enabled, true)
        .override_global("target_pid_ns_dev", &namespace_device, true)
        .override_global("target_pid_ns_inode", &namespace_inode, true);
    // Aya creates the monolithic map set. Analysis uses only system_analysis
    // and globals; minimize the rest and drop the entire object before loading
    // the sampling runtime. Inner templates retain their pinned ABI sizes.
    for map in &artifact.inventory().maps {
        let entries = if map.map_type == 27 {
            u32::try_from(page_size)
                .map_err(|_| BackendError::Capacity("analysis ring page size"))?
        } else {
            1
        };
        let _loader = loader.map_max_entries(&map.name, entries);
    }
    let mut object = loader
        .load(prepared.bytes())
        .map_err(|error| BackendError::kernel("load analysis maps", error.to_string()))?;
    let mut analysis: Array<MapData, [u8; RECORD_SIZE]> = Array::try_from(
        object
            .take_map("system_analysis")
            .ok_or_else(|| BackendError::kernel("analysis map", "system_analysis is absent"))?,
    )
    .map_err(|error| BackendError::kernel("analysis map ABI", error.to_string()))?;
    let program: &mut RawTracePoint = object
        .program_mut("read_task_struct")
        .ok_or_else(|| BackendError::kernel("analysis program", "read_task_struct is absent"))?
        .try_into()
        .map_err(|error: aya::programs::ProgramError| {
            BackendError::kernel("analysis program type", error.to_string())
        })?;
    program
        .load()
        .map_err(|error| BackendError::kernel("load task analysis", error.to_string()))?;
    after_resources()?;
    let tid = super::syscall::thread_id()
        .map_err(|error| BackendError::kernel("analysis requester TID", error.to_string()))?;
    discover_with_reader(page_size as u64, |offset| {
        let mut request = [0_u8; RECORD_SIZE];
        request[..8].copy_from_slice(&(offset as u64).to_le_bytes());
        request[8..12].copy_from_slice(&tid.to_le_bytes());
        analysis
            .set(0, request, 0)
            .map_err(|error| BackendError::kernel("request task analysis", error.to_string()))?;
        // Publish the request before attaching. The separately pinned TID
        // filter also excludes concurrent syscalls from other threads.
        let link = program
            .attach("sys_enter")
            .map_err(|error| BackendError::kernel("attach task analysis", error.to_string()))?;
        let response = analysis.get(&0, 0);
        let detached = program.detach(link);
        detached
            .map_err(|error| BackendError::kernel("detach task analysis", error.to_string()))?;
        decode_response(
            &response
                .map_err(|error| BackendError::kernel("read task analysis", error.to_string()))?,
        )
    })
}

fn decode_response(bytes: &[u8; RECORD_SIZE]) -> Result<AnalysisRead, BackendError> {
    let mut word = [0_u8; 4];
    word.copy_from_slice(&bytes[8..12]);
    if u32::from_le_bytes(word) != 0 {
        return Err(BackendError::kernel(
            "task analysis response",
            "request was not handled",
        ));
    }
    word.copy_from_slice(&bytes[12..16]);
    let error = i32::from_le_bytes(word);
    if error != 0 {
        return Err(BackendError::kernel(
            "task analysis response",
            format!("kernel helper error {error}"),
        ));
    }
    let mut address = [0_u8; 8];
    address.copy_from_slice(&bytes[..8]);
    let mut code = [0_u8; CODE_SIZE];
    code.copy_from_slice(&bytes[16..]);
    Ok(AnalysisRead {
        registers: u64::from_le_bytes(address),
        code,
    })
}

fn candidate(base: u64, registers: u64, page_size: u64) -> Option<u32> {
    if base == 0 || base & (page_size - 1) != 0 {
        return None;
    }
    let offset = registers.checked_sub(base)?;
    if offset == 0 || offset >= MAX_STACK_BYTES || !offset.is_multiple_of(8) {
        return None;
    }
    u32::try_from(offset).ok()
}

fn discover_with_reader(
    page_size: u64,
    mut read: impl FnMut(usize) -> Result<AnalysisRead, BackendError>,
) -> Result<KernelStackLayout, BackendError> {
    if !page_size.is_power_of_two() {
        return Err(BackendError::kernel(
            "task analysis layout",
            "invalid kernel page size",
        ));
    }
    for offset in (0..MAX_TASK_BYTES).step_by(CODE_SIZE) {
        let response = read(offset)?;
        for (index, word) in response.code.chunks_exact(8).enumerate() {
            let mut bytes = [0_u8; 8];
            bytes.copy_from_slice(word);
            if let Some(stack_ptregs_offset) =
                candidate(u64::from_le_bytes(bytes), response.registers, page_size)
            {
                let task_stack_offset = offset + index * 8;
                // Confirm the same offsets on a fresh request. Kernel addresses
                // may differ between threads, but their layout must agree.
                let confirmation = read(offset)?;
                bytes.copy_from_slice(&confirmation.code[index * 8..index * 8 + 8]);
                if candidate(u64::from_le_bytes(bytes), confirmation.registers, page_size)
                    != Some(stack_ptregs_offset)
                {
                    return Err(BackendError::kernel(
                        "task analysis layout",
                        "stack layout changed during confirmation",
                    ));
                }
                return Ok(KernelStackLayout {
                    task_stack_offset: task_stack_offset as u32,
                    stack_ptregs_offset,
                });
            }
        }
    }
    Err(BackendError::kernel(
        "task analysis layout",
        "no stack field found within the bounded task scan",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: Temporary task analysis runs in one thread while other threads continuously issue syscalls.
    /// Guarantees: Only the requested local TID handles each request, independently of process-wide activity or host PID mode.
    #[test]
    #[ignore = "requires authorized BPF capabilities, two CPUs, and UPSTREAM_EBPF_OBJECT"]
    fn privileged_analysis_excludes_other_threads() {
        use std::sync::atomic::{AtomicBool, Ordering};
        assert_eq!(
            std::env::var("OTEL_ARROW_EBPF_PRIVILEGED_TEST").as_deref(),
            Ok("1")
        );
        let artifact = UpstreamArtifact::open(
            std::env::var_os("UPSTREAM_EBPF_OBJECT").expect("artifact path"),
        )
        .expect("artifact");
        let bytes = crate::input::read_file(
            std::path::Path::new("/sys/kernel/btf/vmlinux"),
            64 * 1024 * 1024,
        )
        .expect("BTF");
        let btf = aya::Btf::parse(&bytes, object::Endianness::Little).expect("BTF parse");
        let page_size = super::super::syscall::page_size().expect("page size");
        // The atomic coordinates only this test's bounded syscall competitors.
        let stop = AtomicBool::new(false);
        struct Stop<'a>(&'a AtomicBool);
        impl Drop for Stop<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let _thread = scope.spawn(|| {
                    while !stop.load(Ordering::Acquire) {
                        let _tid = super::super::syscall::thread_id().expect("competitor syscall");
                    }
                });
            }
            let _stop = Stop(&stop);
            let analyser = scope.spawn(|| {
                assert_ne!(
                    super::super::syscall::thread_id().expect("analysis TID"),
                    std::process::id()
                );
                let mut expected = None;
                for _ in 0..3 {
                    let layout = discover(&artifact, &btf, page_size, || Ok(()))
                        .expect("exclusive analysis");
                    if let Some(previous) = expected {
                        assert_eq!(layout, previous);
                    }
                    expected = Some(layout);
                }
            });
            analyser.join().expect("analysis thread");
        });
    }

    fn response(base: u64, offset: u64) -> AnalysisRead {
        let mut code = [0_u8; CODE_SIZE];
        code[..8].copy_from_slice(&base.to_le_bytes());
        AnalysisRead {
            registers: base + offset,
            code,
        }
    }

    /// Scenario: The stack field is found in a later task block and confirmed on another thread.
    /// Guarantees: Only layout offsets escape analysis; differing kernel addresses do not change the result.
    #[test]
    fn stack_layout_is_discovered_and_confirmed() {
        let mut calls = 0;
        let layout = discover_with_reader(4096, |offset| {
            calls += 1;
            Ok(match calls {
                1 => AnalysisRead {
                    registers: 0x5000,
                    code: [0; CODE_SIZE],
                },
                2 => {
                    assert_eq!(offset, 128);
                    response(0x10000, 0x3f58)
                }
                _ => {
                    assert_eq!(offset, 128);
                    response(0x20000, 0x3f58)
                }
            })
        })
        .expect("confirmed layout");
        assert_eq!(
            layout,
            KernelStackLayout {
                task_stack_offset: 128,
                stack_ptregs_offset: 0x3f58
            }
        );
        assert_eq!(calls, 3);
    }

    /// Scenario: No page-aligned stack pointer exists within the allowed task range.
    /// Guarantees: The scan stops after 64 reads rather than growing or retrying indefinitely.
    #[test]
    fn scan_has_a_fixed_request_bound() {
        let mut calls = 0;
        assert!(
            discover_with_reader(4096, |_| {
                calls += 1;
                Ok(AnalysisRead {
                    registers: 0x5000,
                    code: [0; CODE_SIZE],
                })
            })
            .is_err()
        );
        assert_eq!(calls, MAX_TASK_BYTES / CODE_SIZE);
    }

    /// Scenario: The probe leaves its PID set or reports a kernel read error.
    /// Guarantees: Incomplete or failed analysis cannot become success-shaped zero offsets.
    #[test]
    fn failed_probe_responses_are_rejected() {
        let mut bytes = [0_u8; RECORD_SIZE];
        bytes[8..12].copy_from_slice(&1_u32.to_le_bytes());
        assert!(decode_response(&bytes).is_err());
        bytes[8..12].fill(0);
        bytes[12..16].copy_from_slice(&(-14_i32).to_le_bytes());
        assert!(decode_response(&bytes).is_err());
    }
}
