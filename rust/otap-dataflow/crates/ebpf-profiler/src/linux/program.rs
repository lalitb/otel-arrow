// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Bounded object loading and deferred per-CPU attachment through Aya.

use aya::{
    Ebpf, EbpfError, EbpfLoader, VerifierLogLevel,
    maps::{MapData, PerCpuArray, PerfEventArray},
    programs::{
        ProgramError,
        perf_event::{
            PerfEvent, PerfEventConfig, PerfEventLinkId, PerfEventScope, SamplePolicy,
            SoftwareEvent,
        },
    },
};
use nix::unistd::{SysconfVar, sysconf};
use object::{Object, ObjectSection, ObjectSymbol};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::File,
    path::{Path, PathBuf},
};

use crate::{
    FailureMode, Platform, PlatformGuard, PlatformStart, PreparedPlatform, ProfilerError,
    ProfilerStatistics, Result, ShardPlan, SystemTopology, ValidatedConfig,
    linux::{
        capability,
        kernel::{EVENTS_MAP_ENTRIES, EVENTS_MAP_NAME, program_name},
        perf::AyaEventSource,
        procfs,
    },
};

const COUNTERS_MAP_NAME: &str = "COUNTERS";
const MAX_MANIFEST_BYTES: usize = 4096;

#[derive(Debug, Default)]
pub(crate) struct LinuxPlatform;

impl LinuxPlatform {
    pub(crate) fn shared() -> std::sync::Arc<dyn Platform> {
        std::sync::Arc::new(Self)
    }
}

impl Platform for LinuxPlatform {
    fn discover_topology(&self, config: &ValidatedConfig) -> Result<SystemTopology> {
        procfs::discover(&config.get().limits)
    }

    fn prepare(&self, config: &ValidatedConfig, shards: &[ShardPlan]) -> Result<PreparedPlatform> {
        capability::probe(config)?;
        // Per-CPU maps allocate for possible CPUs, not just selected/online CPUs.
        let topology = procfs::discover(&config.get().limits)?;
        let selected_program = program_name(config.get().include_kernel_stacks);
        let object_path = object_path(config)?;
        let bytes = validated_object(&object_path, config)?;
        let page_size = sysconf(SysconfVar::PAGE_SIZE)
            .map_err(|error| ProfilerError::ProgramLoad(error.to_string()))?
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| {
                ProfilerError::ProgramLoad("host page size is unavailable".to_owned())
            })?;
        let page_count = perf_page_count(config.get().limits.perf_buffer_bytes_per_cpu, page_size)?;
        let event_entries = config
            .get()
            .limits
            .max_cpu_id
            .checked_add(1)
            .ok_or_else(|| ProfilerError::invalid("limits.max_cpu_id", "map size overflow"))?;
        super::btf_preflight::kernel_btf(&config.get().program)?;
        let mut ebpf = EbpfLoader::new()
            .btf(None)
            .verifier_log_level(VerifierLogLevel::DISABLE)
            .map_max_entries(EVENTS_MAP_NAME, event_entries)
            .load(&bytes)
            .map_err(classify_load)?;
        let map = ebpf.take_map(EVENTS_MAP_NAME).ok_or_else(|| {
            ProfilerError::MapCreation(format!("object has no {EVENTS_MAP_NAME} map"))
        })?;
        let mut perf_array = PerfEventArray::<MapData>::try_from(map)
            .map_err(|error| ProfilerError::MapCreation(error.to_string()))?;
        let map = ebpf
            .take_map(COUNTERS_MAP_NAME)
            .ok_or_else(|| ProfilerError::MapCreation("object has no COUNTERS map".to_owned()))?;
        let counters = PerCpuArray::<MapData, u64>::try_from(map)
            .map_err(|error| ProfilerError::MapCreation(error.to_string()))?;
        let program: &mut PerfEvent = ebpf
            .program_mut(selected_program)
            .ok_or_else(|| {
                ProfilerError::ProgramLoad(format!("object has no {selected_program} program"))
            })?
            .try_into()
            .map_err(classify_program_load)?;
        program.load().map_err(classify_program_load)?;

        let mut sources: Vec<Box<dyn crate::EventSource>> = Vec::with_capacity(shards.len());
        let mut pending_cpus = Vec::new();
        let mut unavailable_cpus = 0;
        for shard in shards {
            let mut buffers = Vec::with_capacity(shard.cpus.len());
            for &cpu in &shard.cpus {
                if cpu >= event_entries || !topology.cpu(cpu).is_some_and(|cpu| cpu.online) {
                    if config.get().failure_mode == FailureMode::Strict {
                        return Err(ProfilerError::CpuSelection(format!(
                            "CPU {cpu} is not online or exceeds the event map"
                        )));
                    }
                    unavailable_cpus += 1;
                    continue;
                }
                match perf_array.open(cpu, Some(page_count)) {
                    Ok(buffer) => {
                        buffers.push(buffer);
                        pending_cpus.push(cpu);
                    }
                    Err(_) if config.get().failure_mode == FailureMode::BestEffort => {
                        unavailable_cpus += 1
                    }
                    Err(error) => {
                        return Err(ProfilerError::PerfEventOpen {
                            cpu,
                            reason: error.to_string(),
                        });
                    }
                }
            }
            sources.push(Box::new(AyaEventSource::new(buffers)));
        }
        if pending_cpus.is_empty() {
            return Err(ProfilerError::PartialStartup(
                "no selected CPU perf buffer could be opened".to_owned(),
            ));
        }
        Ok(PreparedPlatform {
            sources,
            guard: Box::new(AyaGuard {
                state: Some(AyaState {
                    ebpf,
                    counters,
                    links: Vec::new(),
                    pending_cpus,
                }),
                program_name: selected_program,
                frequency: u64::from(config.get().samples_per_second.get()),
                failure_mode: config.get().failure_mode,
                coverage: PlatformStart {
                    attached_cpus: 0,
                    unavailable_cpus,
                },
                started: false,
                max_possible_cpus: config.get().limits.max_possible_cpus,
            }),
        })
    }
}

fn perf_page_count(bytes: usize, page_size: usize) -> Result<usize> {
    if page_size == 0 || !bytes.is_multiple_of(page_size) {
        return Err(ProfilerError::invalid(
            "limits.perf_buffer_bytes_per_cpu",
            "must be a multiple of host page size",
        ));
    }
    let pages = bytes / page_size;
    if !pages.is_power_of_two() || bytes.checked_add(page_size).is_none() {
        return Err(ProfilerError::invalid(
            "limits.perf_buffer_bytes_per_cpu",
            "must be a non-zero power-of-two page count with space for the perf metadata page",
        ));
    }
    Ok(pages)
}

fn classify_program_load(error: ProgramError) -> ProfilerError {
    match error {
        // The verifier commonly returns EACCES for invalid programs too. The
        // capability gate is separate; never turn load failures into smoke skips.
        ProgramError::LoadError { .. } => {
            ProfilerError::VerifierRejected(crate::error::bounded_message(error, 4096))
        }
        ProgramError::MapError(error) => {
            ProfilerError::MapCreation(crate::error::bounded_message(error, 4096))
        }
        error => ProfilerError::ProgramLoad(crate::error::bounded_message(error, 4096)),
    }
}

fn classify_load(error: EbpfError) -> ProfilerError {
    match error {
        EbpfError::MapError(error) => {
            ProfilerError::MapCreation(crate::error::bounded_message(error, 4096))
        }
        EbpfError::ProgramError(error) => classify_program_load(error),
        error => ProfilerError::ProgramLoad(crate::error::bounded_message(error, 4096)),
    }
}

fn classify_attach(cpu: u32, error: ProgramError) -> ProfilerError {
    // Aya exposes perf_event_open as a typed syscall error; no display-string matching.
    match error {
        ProgramError::SyscallError(error) if error.call == "perf_event_open" => {
            ProfilerError::PerfEventOpen {
                cpu,
                reason: error.to_string(),
            }
        }
        error => ProfilerError::ProgramAttachment {
            cpu,
            reason: error.to_string(),
        },
    }
}

fn object_path(config: &ValidatedConfig) -> Result<PathBuf> {
    config.get().program.object_path.clone()
        .or_else(|| std::env::var_os("OTEL_EBPF_PROFILER_OBJECT").map(PathBuf::from))
        .ok_or_else(|| ProfilerError::ProgramLoad(
            "no eBPF object path configured; set ProgramConfig::object_path or OTEL_EBPF_PROFILER_OBJECT".to_owned()
        ))
}

fn validated_object(path: &Path, config: &ValidatedConfig) -> Result<Vec<u8>> {
    let bytes = read_regular(path, config.get().program.max_object_bytes)?;
    validate_layout(&bytes)?;
    if config.get().program.require_manifest {
        let manifest = read_regular(&manifest_path(path), MAX_MANIFEST_BYTES)?;
        validate_manifest(&manifest, &bytes, std::env::consts::ARCH)?;
    }
    Ok(bytes)
}

fn read_regular(path: &Path, max_bytes: usize) -> Result<Vec<u8>> {
    use std::os::unix::fs::OpenOptionsExt;
    // O_NONBLOCK prevents FIFO opens from hanging before the regular-file check.
    let file = File::options()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK)
        .open(path)
        .map_err(|source| ProfilerError::Io {
            operation: "open eBPF artifact",
            path: path.into(),
            source,
        })?;
    let metadata = file.metadata().map_err(|source| ProfilerError::Io {
        operation: "inspect eBPF artifact handle",
        path: path.into(),
        source,
    })?;
    if !metadata.is_file() || metadata.len() > max_bytes as u64 {
        return Err(ProfilerError::ProgramLoad(
            "artifact is not a bounded regular file".to_owned(),
        ));
    }
    // Hashing, parsing and Aya consume this same bounded read from the same handle.
    procfs::read_handle(file, path, max_bytes)
}

fn manifest_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(".manifest");
    PathBuf::from(value)
}

fn validate_manifest(manifest: &[u8], object: &[u8], architecture: &str) -> Result<()> {
    if manifest.len() > MAX_MANIFEST_BYTES {
        return Err(ProfilerError::ProgramLoad(
            "manifest exceeds 4096 bytes".to_owned(),
        ));
    }
    let text = std::str::from_utf8(manifest)
        .map_err(|error| ProfilerError::ProgramLoad(format!("manifest is not UTF-8: {error}")))?;
    let mut values = HashMap::new();
    for line in text.lines() {
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| ProfilerError::ProgramLoad("malformed manifest entry".to_owned()))?;
        if key.is_empty() || value.is_empty() || values.insert(key, value).is_some() {
            return Err(ProfilerError::ProgramLoad(
                "empty or duplicate manifest key/value".to_owned(),
            ));
        }
    }
    for (key, expected) in [
        ("format", "otel-ebpf-profiler-object-v1"),
        ("abi_version", "1"),
        ("architecture", architecture),
        ("endianness", "little"),
        ("timestamp_clock", "BOOTTIME"),
        ("program_user", "profile_cpu"),
        ("program_user_kernel", "profile_cpu_kernel"),
        ("events_map", EVENTS_MAP_NAME),
        ("counters_map", COUNTERS_MAP_NAME),
        ("license", "GPL-2.0-only"),
    ] {
        if values.get(key).copied() != Some(expected) {
            return Err(ProfilerError::ProgramLoad(format!(
                "manifest {key} must be {expected}"
            )));
        }
    }
    let actual = hex::encode(Sha256::digest(object));
    if values.get("object_sha256").copied() != Some(actual.as_str()) {
        return Err(ProfilerError::ProgramLoad(
            "object digest mismatch".to_owned(),
        ));
    }
    Ok(())
}

fn layout_error(error: impl std::fmt::Display) -> ProfilerError {
    ProfilerError::ProgramLoad(format!("invalid eBPF object layout: {error}"))
}

fn validate_layout(bytes: &[u8]) -> Result<()> {
    let file = object::File::parse(bytes).map_err(layout_error)?;
    if file.format() != object::BinaryFormat::Elf
        || file.architecture() != object::Architecture::Bpf
        || file.kind() != object::ObjectKind::Relocatable
        || !file.is_64()
        || !file.is_little_endian()
    {
        return Err(layout_error(
            "expected 64-bit little-endian relocatable BPF ELF",
        ));
    }
    let mut sections = 0;
    let mut program_sections = [false; 2];
    let mut maps = None;
    let mut license = false;
    let mut btf = None;
    let mut btf_ext = None;
    for section in file.sections() {
        sections += 1;
        if sections > 128 {
            return Err(layout_error("too many sections"));
        }
        let name = section.name().map_err(layout_error)?;
        let data = section.data().map_err(layout_error)?;
        if matches!(name, ".BTF" | ".BTF.ext")
            && matches!(section.flags(), object::SectionFlags::Elf { sh_flags }
                if sh_flags & u64::from(object::elf::SHF_ALLOC) != 0)
        {
            return Err(layout_error(
                "BTF metadata must not be an allocated section",
            ));
        }
        match name {
            "maps" => {
                if maps.replace(section.index()).is_some() || data.len() != 60 {
                    return Err(layout_error(
                        "expected exactly three legacy map definitions",
                    ));
                }
            }
            "license" => {
                if license || data != b"GPL\0" {
                    return Err(layout_error("invalid license section"));
                }
                license = true;
            }
            "perf_event/profile_cpu" | "perf_event/profile_cpu_kernel" => {
                let index = usize::from(name.ends_with("_kernel"));
                if program_sections[index] || data.is_empty() || !data.len().is_multiple_of(8) {
                    return Err(layout_error("invalid perf program section"));
                }
                program_sections[index] = true;
            }
            ".text" if data.is_empty() => {}
            ".BTF" => {
                if btf
                    .replace((data, super::btf_preflight::inspect(data, 8192, 16384)?))
                    .is_some()
                {
                    return Err(layout_error("duplicate BTF"));
                }
            }
            ".BTF.ext" => {
                if btf_ext.replace(data).is_some() {
                    return Err(layout_error("duplicate BTF.ext"));
                }
            }
            _ => {
                let metadata = name.starts_with(".debug_")
                    || name.starts_with(".rel.debug_")
                    || matches!(
                        name,
                        ".strtab"
                            | ".symtab"
                            | ".BTF"
                            | ".BTF.ext"
                            | ".rel.BTF"
                            | ".rel.BTF.ext"
                            | ".llvm_addrsig"
                            | ".relperf_event/profile_cpu"
                            | ".relperf_event/profile_cpu_kernel"
                    );
                if !metadata
                    || name.len() > 256
                    || matches!(section.flags(),
                    object::SectionFlags::Elf { sh_flags } if sh_flags & u64::from(object::elf::SHF_ALLOC) != 0)
                    || name == ".maps"
                    || name.starts_with(".data")
                    || name.starts_with(".rodata")
                    || name.starts_with(".bss")
                {
                    return Err(layout_error("unexpected allocated section or implicit map"));
                }
            }
        }
    }
    if !license || program_sections != [true; 2] {
        return Err(layout_error("missing programs/license"));
    }
    if let Some(ext) = btf_ext {
        let (data, shape) = btf
            .as_ref()
            .ok_or_else(|| layout_error("BTF.ext has no BTF"))?;
        super::btf_preflight::inspect_ext(ext, data, shape)?;
    }
    let map_section = file
        .section_by_index(maps.ok_or_else(|| layout_error("missing maps"))?)
        .map_err(layout_error)?;
    let data = map_section.data().map_err(layout_error)?;
    let mut found = [false; 3];
    let mut offsets = [u64::MAX; 3];
    for (count, symbol) in file.symbols().enumerate() {
        if count >= 8192 {
            return Err(layout_error("too many symbols"));
        }
        if symbol.name().map_err(layout_error)?.len() > 256 {
            return Err(layout_error("symbol name exceeds 256 bytes"));
        }
        if symbol.section_index() != maps {
            continue;
        }
        if symbol.kind() != object::SymbolKind::Data {
            return Err(layout_error("unexpected map-section symbol kind"));
        }
        let name = symbol.name().map_err(layout_error)?;
        let (index, expected): (usize, [u32; 5]) = match name {
            EVENTS_MAP_NAME => (0, [4, 4, 4, EVENTS_MAP_ENTRIES, 0]),
            "SCRATCH" => (1, [6, 4, crate::ABI_EVENT_SIZE as u32, 1, 0]),
            COUNTERS_MAP_NAME => (2, [6, 4, 8, 3, 0]),
            _ => return Err(layout_error("unexpected map symbol")),
        };
        let offset = usize::try_from(symbol.address()).map_err(layout_error)?;
        if found[index]
            || symbol.size() != 20
            || offset % 20 != 0
            || offsets.contains(&symbol.address())
        {
            return Err(layout_error("duplicate/overlapping map symbol"));
        }
        let definition = data
            .get(offset..offset.saturating_add(20))
            .ok_or_else(|| layout_error("map definition out of bounds"))?;
        for (word, expected) in definition.chunks_exact(4).zip(expected) {
            if word != expected.to_le_bytes() {
                return Err(layout_error("unexpected map shape/capacity"));
            }
        }
        found[index] = true;
        offsets[index] = symbol.address();
    }
    if found != [true; 3] {
        return Err(layout_error("missing map definition"));
    }
    Ok(())
}

struct AyaState {
    ebpf: Ebpf,
    counters: PerCpuArray<MapData, u64>,
    links: Vec<(u32, PerfEventLinkId)>,
    pending_cpus: Vec<u32>,
}

struct AyaGuard {
    state: Option<AyaState>,
    program_name: &'static str,
    frequency: u64,
    failure_mode: FailureMode,
    coverage: PlatformStart,
    started: bool,
    max_possible_cpus: usize,
}

fn loaded_program<'a>(ebpf: &'a mut Ebpf, name: &str) -> Result<&'a mut PerfEvent> {
    ebpf.program_mut(name)
        .ok_or_else(|| {
            ProfilerError::InternalInvariant("loaded Aya program disappeared".to_owned())
        })?
        .try_into()
        .map_err(classify_program_load)
}

impl PlatformGuard for AyaGuard {
    fn start_sampling(&mut self) -> Result<PlatformStart> {
        if self.started {
            return if self
                .state
                .as_ref()
                .is_some_and(|state| !state.links.is_empty())
            {
                Ok(self.coverage)
            } else {
                Err(ProfilerError::PartialStartup(
                    "Aya sampling was already stopped".to_owned(),
                ))
            };
        }
        let state = self
            .state
            .as_mut()
            .ok_or_else(|| ProfilerError::PartialStartup("Aya guard was cleaned up".to_owned()))?;
        let program = loaded_program(&mut state.ebpf, self.program_name)?;
        let mut failure = None;
        for cpu in state.pending_cpus.drain(..) {
            match program.attach(
                PerfEventConfig::Software(SoftwareEvent::CpuClock),
                PerfEventScope::AllProcessesOneCpu { cpu },
                SamplePolicy::Frequency(self.frequency),
                false,
            ) {
                Ok(link) => state.links.push((cpu, link)),
                Err(_) if self.failure_mode == FailureMode::BestEffort => {
                    self.coverage.unavailable_cpus += 1
                }
                Err(error) => {
                    // Dropping the whole state also releases links if detachment fails.
                    failure = Some(classify_attach(cpu, error));
                    break;
                }
            }
        }
        if let Some(error) = failure {
            self.state = None;
            return Err(error);
        }
        self.coverage.attached_cpus = state.links.len();
        if state.links.is_empty() {
            self.state = None;
            return Err(ProfilerError::PartialStartup(
                "no selected CPU could attach".to_owned(),
            ));
        }
        self.started = true;
        Ok(self.coverage)
    }

    fn stop_sampling(&mut self) -> Result<()> {
        let Some(state) = self.state.as_mut() else {
            return Ok(());
        };
        let program = loaded_program(&mut state.ebpf, self.program_name)?;
        let mut failure = None;
        for (cpu, link) in state.links.drain(..) {
            if let Err(error) = program.detach(link) {
                if failure.is_none() {
                    failure = Some(classify_attach(cpu, error));
                }
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn statistics(&mut self) -> Result<ProfilerStatistics> {
        let Some(state) = &self.state else {
            return Ok(ProfilerStatistics::default());
        };
        let mut totals = [0_u64; 3];
        for (index, total) in totals.iter_mut().enumerate() {
            let values = state
                .counters
                .get(&(index as u32), 0)
                .map_err(|error| ProfilerError::MapCreation(error.to_string()))?;
            if values.len() > self.max_possible_cpus {
                return Err(ProfilerError::TopologyDiscovery(
                    "possible CPU count grew beyond configured bound".to_owned(),
                ));
            }
            for value in values.iter() {
                *total = total.saturating_add(*value);
            }
        }
        Ok(ProfilerStatistics {
            sampling_periods_attempted: totals[0],
            kernel_output_failures: totals[1],
            idle_samples_skipped: totals[2],
            ..ProfilerStatistics::default()
        })
    }

    fn cleanup(&mut self) -> Result<()> {
        let result = self.stop_sampling();
        self.state = None;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(bytes: &[u8]) -> String {
        format!(
            "format=otel-ebpf-profiler-object-v1\nabi_version=1\narchitecture={}\nendianness=little\ntimestamp_clock=BOOTTIME\nprogram_user=profile_cpu\nprogram_user_kernel=profile_cpu_kernel\nevents_map=EVENTS\ncounters_map=COUNTERS\nlicense=GPL-2.0-only\nobject_sha256={}\n",
            std::env::consts::ARCH,
            hex::encode(Sha256::digest(bytes))
        )
    }

    /// Scenario: Manifests contain duplicate keys, wrong digests or incompatible metadata.
    /// Guarantees: Integrity/compatibility checks reject ambiguity before kernel loading.
    #[test]
    fn bounded_manifest_integrity() {
        let bytes = b"fixture";
        let valid = manifest(bytes);
        validate_manifest(valid.as_bytes(), bytes, std::env::consts::ARCH).expect("manifest");
        for invalid in [
            format!("{valid}abi_version=1\n"),
            valid.replace("abi_version=1", "abi_version=2"),
            valid.replace("endianness=little", "endianness=big"),
            format!("{valid}bad line\n"),
            "x".repeat(MAX_MANIFEST_BYTES + 1),
        ] {
            assert!(validate_manifest(invalid.as_bytes(), bytes, std::env::consts::ARCH).is_err());
        }
        assert!(validate_manifest(valid.as_bytes(), b"modified", std::env::consts::ARCH).is_err());
        assert!(validate_manifest(valid.as_bytes(), bytes, "different-arch").is_err());
    }

    /// Scenario: Buffer byte budgets are checked on 4 KiB and 64 KiB page hosts.
    /// Guarantees: Partial-page truncation and zero/non-power-of-two page counts fail.
    #[test]
    fn perf_bytes_respect_native_page_size() {
        assert_eq!(perf_page_count(262144, 4096).expect("4K"), 64);
        assert_eq!(perf_page_count(262144, 65536).expect("64K"), 4);
        for (bytes, pages) in [
            (0, 4096),
            (8193, 4096),
            (12288, 4096),
            (4096, 65536),
            (1, 0),
        ] {
            assert!(perf_page_count(bytes, pages).is_err());
        }
    }

    /// Scenario: An independently compiled original Clang object is supplied.
    /// Guarantees: Its ELF/maps/manifest pass preflight without BPF capabilities.
    #[test]
    fn generated_object_contract() {
        let Some(path) = std::env::var_os("OTEL_EBPF_PROFILER_CONTRACT_OBJECT") else {
            return;
        };
        let path = PathBuf::from(path);
        let config = crate::ProfilerConfig::default().validate().expect("config");
        let bytes = validated_object(&path, &config).expect("compiled object must validate");
        assert!(!bytes.is_empty());
        assert!(validate_layout(b"not ELF").is_err());
        let parsed = object::File::parse(bytes.as_slice()).expect("ELF");
        let maps = parsed.section_by_name("maps").expect("maps");
        let (offset, _) = maps.file_range().expect("map bytes");
        let mut malformed = bytes.clone();
        malformed[offset as usize + 12..offset as usize + 16]
            .copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(validate_layout(&malformed).is_err());
        for name in ["perf_event/profile_cpu", "perf_event/profile_cpu_kernel"] {
            let instructions = parsed
                .section_by_name(name)
                .expect("program")
                .data()
                .expect("code");
            let helpers: Vec<_> = instructions
                .chunks_exact(8)
                .filter(|instruction| instruction[0] == 0x85)
                .map(|instruction| {
                    i32::from_le_bytes(instruction[4..8].try_into().expect("immediate"))
                })
                .collect();
            for required in [1, 8, 14, 25, 67, 125] {
                assert!(
                    helpers.contains(&required),
                    "missing helper {required} in {name}"
                );
            }
            assert!(
                !helpers.contains(&5),
                "MONOTONIC helper must not replace BOOTTIME"
            );
        }
        if let Some(cross) = std::env::var_os("OTEL_EBPF_PROFILER_CROSS_OBJECT") {
            let cross = PathBuf::from(cross);
            let bytes =
                read_regular(&cross, config.get().program.max_object_bytes).expect("cross object");
            validate_layout(&bytes).expect("cross ELF layout");
            let manifest =
                read_regular(&manifest_path(&cross), MAX_MANIFEST_BYTES).expect("cross manifest");
            let architecture = if std::env::consts::ARCH == "aarch64" {
                "x86_64"
            } else {
                "aarch64"
            };
            validate_manifest(&manifest, &bytes, architecture).expect("cross manifest");
        }
    }

    /// Scenario: Artifact paths are directories or exceed the configured byte limit.
    /// Guarantees: Only bounded regular-file handles can reach hashing or Aya.
    #[test]
    fn artifact_handle_is_bounded_and_regular() {
        let directory = tempfile::tempdir_in(".").expect("fixture directory");
        assert!(read_regular(directory.path(), 100).is_err());
        let path = directory.path().join("file");
        std::fs::write(&path, b"12345").expect("fixture file");
        assert!(read_regular(&path, 4).is_err());
        assert_eq!(read_regular(&path, 5).expect("bounded file"), b"12345");
    }
}
