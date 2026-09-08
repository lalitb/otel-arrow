// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

use std::os::unix::fs::MetadataExt;

use aya::maps::perf::PerfEventArrayBuffer;
use aya::maps::{MapData, PerCpuArray, PerfEventArray, ProgramArray, RingBuf};
use aya::programs::perf_event::{
    PerfEvent, PerfEventConfig, PerfEventScope, SamplePolicy, SoftwareEvent,
};
use aya::programs::{KProbe, ProgramError, TracePoint};
use aya::{Ebpf, EbpfLoader};

use crate::artifact::{ArtifactArchitecture, UpstreamArtifact};
use crate::config::{BackendConfig, CpuSelection, PidNamespaceMode};
use crate::error::{BackendError, ConfigError};
use crate::linux::SystemProbe;
use crate::linux::system_config::{KernelStackLayout, NativeGlobals};
use crate::preparation::PreparedObject;

use super::maps::KernelMaps;
use super::{syscall, system_analysis};

const OUTER_STACK_MAPS: [&str; 16] = [
    "exe_id_to_8_stack_deltas",
    "exe_id_to_9_stack_deltas",
    "exe_id_to_10_stack_deltas",
    "exe_id_to_11_stack_deltas",
    "exe_id_to_12_stack_deltas",
    "exe_id_to_13_stack_deltas",
    "exe_id_to_14_stack_deltas",
    "exe_id_to_15_stack_deltas",
    "exe_id_to_16_stack_deltas",
    "exe_id_to_17_stack_deltas",
    "exe_id_to_18_stack_deltas",
    "exe_id_to_19_stack_deltas",
    "exe_id_to_20_stack_deltas",
    "exe_id_to_21_stack_deltas",
    "exe_id_to_22_stack_deltas",
    "exe_id_to_23_stack_deltas",
];

const OPTIONAL_MAPS: [&str; 16] = [
    "apm_int_procs",
    "beam_procs",
    "dotnet_procs",
    "go_procs",
    "hotspot_procs",
    "interpreter_offsets",
    "luajit_procs",
    "perl_procs",
    "php_procs",
    "py_procs",
    "ruby_procs",
    "sched_times",
    "traces_ctx_v1",
    "v8_procs",
    "ext_probe_value",
    "per_cpu_records_kp",
];

pub(crate) struct LoadedRuntime {
    pub(crate) bpf: Option<Ebpf>,
    // The kernel clears program-array slots when their last userspace FD closes,
    // even if loaded programs still reference the map.
    _perf_progs: ProgramArray<MapData>,
    _kprobe_progs: ProgramArray<MapData>,
    pub(crate) ring: RingBuf<MapData>,
    pub(crate) report_buffers: Vec<PerfEventArrayBuffer<MapData>>,
    pub(crate) maps: KernelMaps,
    pub(crate) metrics: PerCpuArray<MapData, [u8; 8]>,
    pub(crate) selected_cpus: Vec<u32>,
    pub(crate) cpu_attachment_failures: u64,
    pub(crate) page_size: usize,
    pub(crate) stack_layout: KernelStackLayout,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StartupStage {
    AnalysisResourcesCreated,
    SystemAnalysisCompleted,
    MapsCreated,
    NativeProgramsLoaded,
    TailCallsInitialized,
    ReadersCreated,
    LifecycleAttached,
    CpuAttached,
}

pub(crate) fn load(
    config: &BackendConfig,
    artifact: &UpstreamArtifact,
    probe: &SystemProbe,
    mut after_stage: impl FnMut(StartupStage) -> Result<(), BackendError>,
) -> Result<LoadedRuntime, BackendError> {
    let (selected_cpus, offline_cpus) = select_cpus(config, probe)?;
    let cpu_slots = probe.topology.cpu_slots();
    if cpu_slots == 0 || cpu_slots > config.limits.max_cpu_slots {
        return Err(ConfigError::InvalidLimit {
            field: "max_cpu_slots",
            detail: "is smaller than the kernel possible-CPU span",
        }
        .into());
    }
    let page_size = syscall::page_size()
        .map_err(|error| BackendError::kernel("read page size", error.to_string()))?;
    if !config
        .limits
        .perf_buffer_bytes_per_cpu
        .is_multiple_of(page_size)
    {
        return Err(ConfigError::InvalidLimit {
            field: "perf_buffer_bytes_per_cpu",
            detail: "must be an exact multiple of the host page size",
        }
        .into());
    }
    if !config.limits.ring_buffer_bytes.is_multiple_of(page_size) {
        return Err(ConfigError::InvalidLimit {
            field: "ring_buffer_bytes",
            detail: "must contain a whole number of host pages",
        }
        .into());
    }
    let perf_pages = config.limits.perf_buffer_bytes_per_cpu / page_size;
    if !perf_pages.is_power_of_two() {
        return Err(ConfigError::InvalidLimit {
            field: "perf_buffer_bytes_per_cpu",
            detail: "must contain a power-of-two page count",
        }
        .into());
    }

    let architecture = artifact.inventory().architecture;
    let mut globals = globals(config, architecture)?;
    let zero_u16 = 0_u16;
    let zero_u32 = 0_u32;
    let zero_u64 = 0_u64;
    let python_frames = 10_u32;
    let zero_u8 = 0_u8;
    let btf_bytes = crate::input::read_file(&config.kernel_btf_path, 64 * 1024 * 1024)?;
    let default_btf = std::path::Path::new("/sys/kernel/btf/vmlinux");
    if config.kernel_btf_path != default_btf {
        // EbpfLoader::new also probes the default kernel BTF before our explicit
        // override. Bound that boot-immutable sysfs input as well.
        match crate::input::read_file(default_btf, 64 * 1024 * 1024) {
            Ok(bytes) => drop(bytes),
            Err(BackendError::Io { source, .. })
                if matches!(
                    source.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
                ) => {}
            Err(error) => return Err(error),
        }
    }
    let btf = aya::Btf::parse(&btf_bytes, object::Endianness::Little)
        .map_err(|error| BackendError::kernel("parse configured kernel BTF", error.to_string()))?;
    let stack_layout = system_analysis::discover(artifact, &btf, page_size, || {
        after_stage(StartupStage::AnalysisResourcesCreated)
    })?;
    globals.task_stack_offset = stack_layout.task_stack_offset;
    globals.stack_ptregs_offset = stack_layout.stack_ptregs_offset;
    after_stage(StartupStage::SystemAnalysisCompleted)?;
    let mut loader = EbpfLoader::new();
    let _loader = loader
        .btf(Some(&btf))
        .override_global("origin_id_probe", &zero_u16, true)
        .override_global("filter_error_frames", &globals.filter_error_frames, true)
        .override_global("go_labels_disabled", &globals.go_labels_disabled, true)
        .override_global("filter_idle_frames", &globals.filter_idle_frames, true)
        .override_global("inverse_pac_mask", &globals.inverse_pac_mask, true)
        .override_global("vma_lookup_enabled", &globals.vma_lookup_enabled, true)
        .override_global("vma_vm_file_offset", &zero_u32, true)
        .override_global("vma_vm_flags_offset", &zero_u32, true)
        .override_global("origin_id_sampling", &globals.origin_id_sampling, true)
        .override_global("off_cpu_threshold", &zero_u32, true)
        .override_global("origin_id_off_cpu", &zero_u16, true)
        .override_global(
            "filter_min_process_age_ns",
            &globals.filter_min_process_age_ns,
            true,
        )
        .override_global("task_group_leader_offset", &zero_u32, true)
        .override_global("task_start_time_offset", &zero_u32, true)
        .override_global("task_stack_offset", &globals.task_stack_offset, true)
        .override_global("stack_ptregs_offset", &globals.stack_ptregs_offset, true)
        .override_global("python_frames_per_program", &python_frames, true)
        .override_global("ruby_skip_native_resume", &zero_u8, true)
        .override_global("tpbase_offset", &zero_u64, true)
        .override_global(
            "pid_ns_translation_enabled",
            &globals.pid_ns_translation_enabled,
            true,
        )
        .override_global("target_pid_ns_dev", &globals.target_pid_ns_dev, true)
        .override_global("target_pid_ns_inode", &globals.target_pid_ns_inode, true)
        .override_global("with_debug_output", &globals.with_debug_output, true);

    set_map_size(&mut loader, "trace_events", config.limits.ring_buffer_bytes)?;
    set_map_size(&mut loader, "report_events", cpu_slots)?;
    set_map_size(
        &mut loader,
        "pid_page_to_mapping_info",
        config.limits.max_unwind_map_entries,
    )?;
    set_map_size(
        &mut loader,
        "stack_delta_page_to_info",
        config.limits.max_unwind_map_entries,
    )?;
    set_map_size(&mut loader, "reported_pids", config.limits.max_processes)?;
    set_map_size(
        &mut loader,
        "pid_events",
        config
            .limits
            .max_processes
            .saturating_add(config.limits.max_threads),
    )?;
    for name in OUTER_STACK_MAPS {
        set_map_size(&mut loader, name, config.limits.max_executable_ids)?;
    }
    for name in OPTIONAL_MAPS {
        set_map_size(&mut loader, name, 1)?;
    }

    let prepared = PreparedObject::native_probe_maps(artifact)?;
    let mut bpf = loader
        .load(prepared.bytes())
        .map_err(|error| BackendError::kernel("Aya object load", error.to_string()))?;
    after_stage(StartupStage::MapsCreated)?;
    load_perf_program(&mut bpf, "perf_unwind_stop")?;
    load_perf_program(&mut bpf, "perf_unwind_native")?;
    load_perf_program(&mut bpf, "native_tracer_entry")?;
    load_probe_program(&mut bpf, "kprobe_unwind_stop")?;
    load_probe_program(&mut bpf, "kprobe_unwind_native")?;
    after_stage(StartupStage::NativeProgramsLoaded)?;
    let perf_progs = populate_tail_calls(
        &mut bpf,
        "perf_progs",
        [(0, "perf_unwind_stop"), (1, "perf_unwind_native")],
    )?;
    let kprobe_progs = populate_tail_calls(
        &mut bpf,
        "kprobe_progs",
        [(0, "kprobe_unwind_stop"), (1, "kprobe_unwind_native")],
    )?;
    after_stage(StartupStage::TailCallsInitialized)?;

    let sched_program = if probe.kernel.version.uses_sched_process_free_v2() {
        "tracepoint__sched_process_free"
    } else {
        "tracepoint__sched_process_free_pre616"
    };
    let tracepoint: &mut TracePoint = bpf
        .program_mut(sched_program)
        .ok_or_else(|| BackendError::kernel("find program", format!("{sched_program} is absent")))?
        .try_into()
        .map_err(program_error("convert process-free tracepoint"))?;
    tracepoint
        .load()
        .map_err(|error| BackendError::kernel("load process-free tracepoint", error.to_string()))?;
    let mut report_events = PerfEventArray::try_from(
        bpf.take_map("report_events")
            .ok_or_else(|| BackendError::kernel("take map", "report_events is absent"))?,
    )
    .map_err(|error| BackendError::kernel("convert report_events", error.to_string()))?;
    let mut report_buffers = Vec::with_capacity(selected_cpus.len());
    for cpu in &selected_cpus {
        report_buffers.push(
            report_events
                .open(*cpu, Some(perf_pages))
                .map_err(|error| {
                    BackendError::kernel("open report perf buffer", error.to_string())
                })?,
        );
    }
    let ring = RingBuf::try_from(
        bpf.take_map("trace_events")
            .ok_or_else(|| BackendError::kernel("take map", "trace_events is absent"))?,
    )
    .map_err(|error| BackendError::kernel("open trace ring buffer", error.to_string()))?;
    let metrics = PerCpuArray::try_from(
        bpf.take_map("metrics")
            .ok_or_else(|| BackendError::kernel("take map", "metrics is absent"))?,
    )
    .map_err(|error| BackendError::kernel("convert metrics", error.to_string()))?;
    let maps = KernelMaps::take(&mut bpf)?;
    after_stage(StartupStage::ReadersCreated)?;

    let tracepoint: &mut TracePoint = bpf
        .program_mut(sched_program)
        .ok_or_else(|| BackendError::kernel("find program", format!("{sched_program} is absent")))?
        .try_into()
        .map_err(program_error("convert process-free tracepoint"))?;
    let _link = tracepoint
        .attach("sched", "sched_process_free")
        .map_err(|error| {
            BackendError::kernel("attach process-free tracepoint", error.to_string())
        })?;
    after_stage(StartupStage::LifecycleAttached)?;

    let mut cpu_attachment_failures = offline_cpus;
    let mut attached_cpus = Vec::with_capacity(selected_cpus.len());
    let entry: &mut PerfEvent = bpf
        .program_mut("native_tracer_entry")
        .ok_or_else(|| BackendError::kernel("find program", "native_tracer_entry is absent"))?
        .try_into()
        .map_err(program_error("convert native_tracer_entry"))?;
    for cpu in &selected_cpus {
        let result = entry.attach(
            PerfEventConfig::Software(SoftwareEvent::CpuClock),
            PerfEventScope::AllProcessesOneCpu { cpu: *cpu },
            SamplePolicy::Frequency(u64::from(config.sampling_frequency_hz)),
            false,
        );
        if let Err(error) = result {
            if config.strict_cpu_attachment {
                return Err(BackendError::kernel(
                    "attach sampling perf event",
                    format!("CPU {cpu}: {error}"),
                ));
            }
            cpu_attachment_failures = cpu_attachment_failures.saturating_add(1);
        } else {
            attached_cpus.push(*cpu);
            after_stage(StartupStage::CpuAttached)?;
        }
    }
    if attached_cpus.is_empty() {
        return Err(BackendError::kernel(
            "attach sampling perf event",
            "no selected CPU attached",
        ));
    }

    Ok(LoadedRuntime {
        bpf: Some(bpf),
        _perf_progs: perf_progs,
        _kprobe_progs: kprobe_progs,
        ring,
        report_buffers,
        maps,
        metrics,
        selected_cpus: attached_cpus,
        cpu_attachment_failures,
        page_size,
        stack_layout,
    })
}

fn globals(
    config: &BackendConfig,
    architecture: ArtifactArchitecture,
) -> Result<NativeGlobals, BackendError> {
    if architecture == ArtifactArchitecture::Arm64 && config.inverse_pac_mask.is_none() {
        return Err(BackendError::Unsupported(
            "arm64 native loading requires an explicit inverse PAC mask".to_owned(),
        ));
    }
    let mut globals = NativeGlobals::minimal(architecture, config.inverse_pac_mask);
    if config.pid_namespace == PidNamespaceMode::Current {
        let metadata =
            std::fs::metadata(config.procfs_root.join("self/ns/pid")).map_err(|source| {
                BackendError::Io {
                    path: config.procfs_root.join("self/ns/pid"),
                    source,
                }
            })?;
        globals.pid_ns_translation_enabled = 1;
        globals.target_pid_ns_dev = metadata.dev();
        globals.target_pid_ns_inode = metadata.ino();
    }
    Ok(globals)
}

fn select_cpus(
    config: &BackendConfig,
    probe: &SystemProbe,
) -> Result<(Vec<u32>, u64), BackendError> {
    let mut offline = 0_u64;
    let selected = match &config.cpus {
        CpuSelection::AllOnline => probe.topology.online.clone(),
        CpuSelection::List(cpus) => {
            let mut selected = Vec::with_capacity(cpus.len());
            for cpu in cpus {
                if probe.topology.online.binary_search(cpu).is_ok() {
                    selected.push(*cpu);
                } else if config.strict_cpu_attachment {
                    return Err(BackendError::Unsupported(format!(
                        "selected CPU {cpu} is offline"
                    )));
                } else {
                    offline = offline.saturating_add(1);
                }
            }
            selected
        }
    };
    if selected.is_empty() {
        return Err(BackendError::Unsupported(
            "CPU selection contains no online CPU".to_owned(),
        ));
    }
    if selected.len() > config.limits.max_monitored_cpus
        || selected.len().saturating_mul(2).saturating_add(1) > config.limits.max_perf_events
    {
        return Err(BackendError::Capacity("selected CPUs or perf events"));
    }
    Ok((selected, offline))
}

fn set_map_size(
    loader: &mut EbpfLoader<'_>,
    name: &'static str,
    size: usize,
) -> Result<(), BackendError> {
    let size = u32::try_from(size).map_err(|_| ConfigError::OutOfRange {
        field: name,
        minimum: 1,
        maximum: u64::from(u32::MAX),
        actual: u64::try_from(size).unwrap_or(u64::MAX),
    })?;
    let _loader = loader.map_max_entries(name, size);
    Ok(())
}

fn load_perf_program(bpf: &mut Ebpf, name: &'static str) -> Result<(), BackendError> {
    let program: &mut PerfEvent = bpf
        .program_mut(name)
        .ok_or_else(|| BackendError::kernel("find program", format!("{name} is absent")))?
        .try_into()
        .map_err(program_error("convert perf program"))?;
    program
        .load()
        .map_err(|error| BackendError::kernel("load perf program", format!("{name}: {error}")))
}

fn load_probe_program(bpf: &mut Ebpf, name: &'static str) -> Result<(), BackendError> {
    let program: &mut KProbe = bpf
        .program_mut(name)
        .ok_or_else(|| BackendError::kernel("find program", format!("{name} is absent")))?
        .try_into()
        .map_err(program_error("convert probe program"))?;
    program
        .load()
        .map_err(|error| BackendError::kernel("load probe program", format!("{name}: {error}")))
}

fn populate_tail_calls(
    bpf: &mut Ebpf,
    map_name: &'static str,
    targets: [(u32, &'static str); 2],
) -> Result<ProgramArray<MapData>, BackendError> {
    let mut array = ProgramArray::try_from(bpf.take_map(map_name).ok_or_else(|| {
        BackendError::kernel("take program array", format!("{map_name} is absent"))
    })?)
    .map_err(|error| BackendError::kernel("convert program array", error.to_string()))?;
    for (index, name) in targets {
        let program = bpf.program(name).ok_or_else(|| {
            BackendError::kernel("find tail-call target", format!("{name} is absent"))
        })?;
        array
            .set(
                index,
                program
                    .fd()
                    .map_err(program_error("get tail-call program FD"))?,
                0,
            )
            .map_err(|error| {
                BackendError::kernel(
                    "populate program array",
                    format!("{map_name}[{index}]: {error}"),
                )
            })?;
    }
    Ok(array)
}

fn program_error(operation: &'static str) -> impl FnOnce(ProgramError) -> BackendError {
    move |error| BackendError::kernel(operation, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::Sha256Digest;
    use crate::linux::{
        capabilities::CapabilityProbe,
        kernel::{KernelInfo, KernelVersion},
        topology::CpuTopology,
    };

    fn probe() -> SystemProbe {
        SystemProbe {
            kernel: KernelInfo {
                release: "6.6.0".to_owned(),
                version: KernelVersion {
                    major: 6,
                    minor: 6,
                    patch: 0,
                },
                architecture: "x86_64".to_owned(),
            },
            capabilities: CapabilityProbe {
                effective: 0,
                sys_admin: false,
                perfmon: false,
                bpf: false,
            },
            topology: CpuTopology {
                possible: vec![0, 1],
                online: vec![0],
            },
            kernel_btf_readable: false,
            unprivileged_bpf_disabled: Some(2),
        }
    }

    /// Scenario: Best-effort mode selects one online and one offline CPU.
    /// Guarantees: The omitted CPU contributes to attachment-failure accounting without any BPF operation.
    #[test]
    fn best_effort_cpu_selection_accounts_for_offline_cpus() {
        let mut config = BackendConfig::new("/artifact.o".into(), Sha256Digest::ZERO, "test");
        config.cpus = CpuSelection::List(vec![0, 1]);
        config.strict_cpu_attachment = false;
        assert_eq!(
            select_cpus(&config, &probe()).expect("online subset"),
            (vec![0], 1)
        );
    }

    /// Scenario: Strict mode includes an offline CPU, or best-effort mode has no online CPU.
    /// Guarantees: Startup cannot report readiness when its CPU requirements cannot be met.
    #[test]
    fn unavailable_cpu_selection_is_rejected() {
        let mut config = BackendConfig::new("/artifact.o".into(), Sha256Digest::ZERO, "test");
        config.cpus = CpuSelection::List(vec![0, 1]);
        assert!(select_cpus(&config, &probe()).is_err());
        config.strict_cpu_attachment = false;
        config.cpus = CpuSelection::List(vec![1]);
        assert!(select_cpus(&config, &probe()).is_err());
    }
}
