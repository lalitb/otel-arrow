// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Typed profiler configuration and static validation.

use std::{
    num::{NonZeroU32, NonZeroUsize},
    path::PathBuf,
    time::Duration,
};

use crate::{ProfilerError, Result, event::MAX_ABI_STACK_DEPTH};

/// Selects logical CPUs to monitor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CpuSelection {
    /// Monitor every online CPU discovered at startup.
    AllOnline,
    /// Monitor the listed logical CPU identifiers.
    Include(Vec<u32>),
}

/// Selects how monitored CPUs are assigned to workers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShardingStrategy {
    /// Use one worker for all selected CPUs.
    Single,
    /// Use one worker for each represented NUMA node.
    PerNuma,
    /// Use at most the requested number of workers.
    Fixed {
        /// Requested worker count.
        workers: NonZeroUsize,
    },
    /// Reserve the per-core strategy for future measured use.
    PerCore,
}

/// Controls selected-CPU startup failures. A terminal event-source fault always
/// stops collection and is surfaced through runtime failure reporting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureMode {
    /// Fail startup when any selected CPU cannot be attached.
    Strict,
    /// Continue with CPUs that attach successfully.
    BestEffort,
}

/// Policy for pinning a collection worker to its assigned CPU set.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AffinityMode {
    /// Do not alter worker affinity.
    Disabled,
    /// Try to pin; retain collection and count failures in restricted cpusets.
    #[default]
    BestEffort,
    /// Abort startup and roll back all resources if a worker cannot be pinned.
    Required,
}

/// Controls process and thread metadata collection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessConfig {
    /// Procfs root, allowing namespace-specific or test roots.
    pub procfs_root: PathBuf,
    /// Collect thread names from procfs.
    pub collect_thread_names: bool,
    /// Collect executable mappings from procfs.
    pub collect_mappings: bool,
}

impl Default for ProcessConfig {
    fn default() -> Self {
        Self {
            procfs_root: PathBuf::from("/proc"),
            collect_thread_names: true,
            collect_mappings: true,
        }
    }
}

/// Controls optional native symbol resolution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SymbolizationConfig {
    /// Resolve symbols from mapped object files when accessible.
    pub enabled: bool,
}

impl Default for SymbolizationConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Identifies the separately built eBPF object.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgramConfig {
    /// Path to an object built from `ebpf/profiler/src/profiler.bpf.c`.
    pub object_path: Option<PathBuf>,
    /// Require `/sys/kernel/btf/vmlinux` even though the initial program does
    /// not need BTF. This supports controlled deployment policies.
    pub require_btf: bool,
    /// Require and validate the generated object manifest.
    pub require_manifest: bool,
    /// Maximum eBPF ELF object bytes accepted by the loader.
    pub max_object_bytes: usize,
    /// Maximum immutable kernel BTF bytes that Aya may read during construction.
    pub max_kernel_btf_bytes: usize,
    /// Maximum serialized kernel BTF type records.
    pub max_kernel_btf_types: usize,
    /// Maximum aggregate variable-length BTF members, parameters, and enum values.
    pub max_kernel_btf_members: usize,
}

impl Default for ProgramConfig {
    fn default() -> Self {
        Self {
            object_path: None,
            require_btf: false,
            require_manifest: true,
            max_object_bytes: 256 * 1024,
            max_kernel_btf_bytes: 8 * 1024 * 1024,
            max_kernel_btf_types: 262_144,
            max_kernel_btf_members: 524_288,
        }
    }
}

impl ProgramConfig {
    /// Checks loader input limits without opening an object or kernel file.
    pub fn validate(&self) -> Result<()> {
        if self.max_object_bytes == 0
            || self.max_kernel_btf_bytes == 0
            || self.max_kernel_btf_types == 0
            || self.max_kernel_btf_members == 0
        {
            return Err(ProfilerError::invalid(
                "program",
                "loader limits must be positive",
            ));
        }
        Ok(())
    }
}

/// Explicit bounds for all host-influenced in-memory state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Limits {
    /// Maximum possible CPUs, including offline CPUs used by per-CPU BPF maps.
    pub max_possible_cpus: usize,
    /// Highest logical CPU identifier accepted from topology.
    pub max_cpu_id: u32,
    /// Maximum selected logical CPUs.
    pub max_monitored_cpus: usize,
    /// Maximum collection shards.
    pub max_shards: usize,
    /// Perf-buffer bytes requested for each selected CPU.
    pub perf_buffer_bytes_per_cpu: usize,
    /// Maximum decoded events retained in one worker batch.
    pub max_raw_events_queued: usize,
    /// Maximum processes represented in one snapshot.
    pub max_processes: usize,
    /// Maximum threads represented in one snapshot.
    pub max_threads: usize,
    /// Maximum executable mappings represented in one snapshot.
    pub max_mappings: usize,
    /// Maximum unique user stacks in one reporting window.
    pub max_unique_user_stacks: usize,
    /// Maximum unique kernel stacks in one reporting window.
    pub max_unique_kernel_stacks: usize,
    /// Maximum frames retained from either stack.
    pub max_stack_depth: usize,
    /// Maximum resolved functions in one snapshot.
    pub max_functions: usize,
    /// Maximum normalized locations in one snapshot.
    pub max_locations: usize,
    /// Maximum symbol-cache entries.
    pub max_symbols: usize,
    /// Maximum distinct aggregation keys in one window.
    pub max_aggregation_keys: usize,
    /// Maximum samples emitted in one snapshot.
    pub max_samples_per_snapshot: usize,
    /// Maximum estimated logical bytes in one snapshot.
    pub max_snapshot_bytes: usize,
    /// Maximum completed snapshots waiting for the consumer.
    pub max_pending_snapshots: usize,
    /// Maximum pending shard windows waiting for finalization.
    pub max_pending_shard_windows: usize,
    /// Maximum stored error-detail strings in a snapshot.
    pub max_error_details: usize,
    /// Maximum bytes read from one procfs metadata file.
    pub max_procfs_file_bytes: usize,
    /// Maximum bytes read from one executable during symbolization.
    pub max_symbol_file_bytes: usize,
    /// Maximum UTF-8 bytes retained for one metadata string.
    pub max_metadata_string_bytes: usize,
    /// Maximum calculated userspace memory for configured worst-case state.
    pub max_total_memory_bytes: usize,
    /// Requested stack reservation for each worker and finalizer thread.
    pub worker_stack_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_possible_cpus: 1024,
            max_cpu_id: 4095,
            max_monitored_cpus: 128,
            max_shards: 8,
            perf_buffer_bytes_per_cpu: 64 * 1024,
            max_raw_events_queued: 128,
            max_processes: 256,
            max_threads: 1_024,
            max_mappings: 2_048,
            max_unique_user_stacks: 2_048,
            max_unique_kernel_stacks: 512,
            max_stack_depth: 64,
            max_functions: 2_048,
            max_locations: 8_192,
            max_symbols: 4_096,
            max_aggregation_keys: 4_096,
            max_samples_per_snapshot: 4_096,
            max_snapshot_bytes: 4 * 1024 * 1024,
            max_pending_snapshots: 2,
            max_pending_shard_windows: 2,
            max_error_details: 32,
            max_procfs_file_bytes: 1024 * 1024,
            max_symbol_file_bytes: 8 * 1024 * 1024,
            max_metadata_string_bytes: 256,
            max_total_memory_bytes: 256 * 1024 * 1024,
            worker_stack_bytes: 1024 * 1024,
        }
    }
}

/// Complete runtime-neutral profiler configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfilerConfig {
    /// Target on-CPU samples per second per monitored CPU.
    pub samples_per_second: NonZeroU32,
    /// Duration of each reporting window.
    pub reporting_interval: Duration,
    /// Include kernel stacks in addition to user stacks.
    pub include_kernel_stacks: bool,
    /// Logical CPU selection.
    pub cpu_selection: CpuSelection,
    /// Worker and aggregation sharding.
    pub sharding: ShardingStrategy,
    /// Process metadata behavior.
    pub process: ProcessConfig,
    /// Native symbolization behavior.
    pub symbolization: SymbolizationConfig,
    /// eBPF object selection.
    pub program: ProgramConfig,
    /// Resource bounds.
    pub limits: Limits,
    /// Runtime failure policy.
    pub failure_mode: FailureMode,
    /// Worker CPU-affinity policy.
    pub affinity: AffinityMode,
}

impl Default for ProfilerConfig {
    fn default() -> Self {
        Self {
            samples_per_second: NonZeroU32::new(19).expect("19 is non-zero"),
            reporting_interval: Duration::from_secs(10),
            include_kernel_stacks: false,
            cpu_selection: CpuSelection::AllOnline,
            sharding: ShardingStrategy::PerNuma,
            process: ProcessConfig::default(),
            symbolization: SymbolizationConfig::default(),
            program: ProgramConfig::default(),
            limits: Limits::default(),
            failure_mode: FailureMode::Strict,
            affinity: AffinityMode::BestEffort,
        }
    }
}

/// Statically validated profiler configuration.
#[derive(Clone, Debug)]
pub struct ValidatedConfig(ProfilerConfig);

impl ValidatedConfig {
    /// Returns the validated configuration.
    #[must_use]
    pub fn get(&self) -> &ProfilerConfig {
        &self.0
    }

    /// Consumes the wrapper and returns the validated configuration.
    #[must_use]
    pub fn into_inner(self) -> ProfilerConfig {
        self.0
    }
}

impl ProfilerConfig {
    /// Validates relationships and bounds without probing the runtime kernel.
    pub fn validate(self) -> Result<ValidatedConfig> {
        if self.process.procfs_root.as_os_str().len() > 4096
            || self
                .program
                .object_path
                .as_ref()
                .is_some_and(|path| path.as_os_str().len() > 4096)
        {
            return Err(ProfilerError::invalid(
                "paths",
                "configured paths must not exceed 4096 bytes",
            ));
        }
        if self.reporting_interval < Duration::from_millis(10) {
            return Err(ProfilerError::invalid(
                "reporting_interval",
                "must be at least 10ms",
            ));
        }
        if self.reporting_interval > Duration::from_secs(24 * 60 * 60) {
            return Err(ProfilerError::invalid(
                "reporting_interval",
                "must not exceed 24 hours",
            ));
        }
        if self.samples_per_second.get() > 10_000 {
            return Err(ProfilerError::invalid(
                "samples_per_second",
                "must not exceed 10000",
            ));
        }
        self.program.validate()?;
        self.limits.validate()?;
        if self.limits.max_stack_depth > MAX_ABI_STACK_DEPTH {
            return Err(ProfilerError::invalid(
                "limits.max_stack_depth",
                format!("must not exceed ABI maximum {MAX_ABI_STACK_DEPTH}"),
            ));
        }
        if !self.limits.perf_buffer_bytes_per_cpu.is_power_of_two()
            || self.limits.perf_buffer_bytes_per_cpu < 8 * 1024
        {
            return Err(ProfilerError::invalid(
                "limits.perf_buffer_bytes_per_cpu",
                "must be a power of two and at least 8192",
            ));
        }
        if self.limits.max_samples_per_snapshot > self.limits.max_aggregation_keys {
            return Err(ProfilerError::invalid(
                "limits.max_samples_per_snapshot",
                "must not exceed max_aggregation_keys",
            ));
        }
        if let CpuSelection::Include(cpus) = &self.cpu_selection {
            if cpus.is_empty() {
                return Err(ProfilerError::invalid(
                    "cpu_selection",
                    "explicit CPU list must not be empty",
                ));
            }
            if cpus.len() > self.limits.max_monitored_cpus {
                return Err(ProfilerError::invalid(
                    "cpu_selection",
                    "explicit CPU list exceeds max_monitored_cpus",
                ));
            }
            if cpus.iter().any(|cpu| *cpu > self.limits.max_cpu_id) {
                return Err(ProfilerError::invalid(
                    "cpu_selection",
                    "CPU identifier exceeds max_cpu_id",
                ));
            }
            let mut sorted = cpus.clone();
            sorted.sort_unstable();
            sorted.dedup();
            if sorted.len() != cpus.len() {
                return Err(ProfilerError::invalid(
                    "cpu_selection",
                    "explicit CPU list contains duplicates",
                ));
            }
        }
        if matches!(self.sharding, ShardingStrategy::PerCore) {
            return Err(ProfilerError::invalid(
                "sharding",
                "per-core mode is reserved until benchmarks justify it",
            ));
        }
        if let ShardingStrategy::Fixed { workers } = self.sharding
            && workers.get() > self.limits.max_shards
        {
            return Err(ProfilerError::invalid(
                "sharding",
                "fixed worker count exceeds max_shards",
            ));
        }
        let estimated = self.estimated_max_memory_bytes()?;
        if estimated > self.limits.max_total_memory_bytes {
            return Err(ProfilerError::invalid(
                "limits.max_total_memory_bytes",
                format!(
                    "calculated worst-case {estimated} bytes exceeds configured maximum {}",
                    self.limits.max_total_memory_bytes
                ),
            ));
        }
        Ok(ValidatedConfig(self))
    }

    /// Calculates a conservative checked upper bound for userspace memory.
    ///
    /// The estimate includes kernel perf buffers, worker raw-event scratch,
    /// all retained indexes and metadata, frozen windows in a shared permit
    /// pool, merge scratch, completed-snapshot slots, thread stacks, and bounded
    /// concurrent reads. The estimate is not an RSS promise; see `MemoryEstimate`.
    pub fn estimated_max_memory_bytes(&self) -> Result<usize> {
        self.memory_estimate()?.total_bytes()
    }

    /// Returns the components of the conservative requested-memory envelope.
    /// Allocator fragmentation and snapshots retained by consumers after
    /// ownership transfer are outside the profiler's allocation contract.
    pub fn memory_estimate(&self) -> Result<crate::MemoryEstimate> {
        crate::limits::estimate(self)
    }
}

fn validate_positive_limits(limits: &Limits) -> Result<()> {
    let values = [
        ("max_monitored_cpus", limits.max_monitored_cpus),
        ("max_possible_cpus", limits.max_possible_cpus),
        ("max_shards", limits.max_shards),
        (
            "perf_buffer_bytes_per_cpu",
            limits.perf_buffer_bytes_per_cpu,
        ),
        ("max_raw_events_queued", limits.max_raw_events_queued),
        ("max_processes", limits.max_processes),
        ("max_threads", limits.max_threads),
        ("max_mappings", limits.max_mappings),
        ("max_unique_user_stacks", limits.max_unique_user_stacks),
        ("max_unique_kernel_stacks", limits.max_unique_kernel_stacks),
        ("max_stack_depth", limits.max_stack_depth),
        ("max_functions", limits.max_functions),
        ("max_locations", limits.max_locations),
        ("max_symbols", limits.max_symbols),
        ("max_aggregation_keys", limits.max_aggregation_keys),
        ("max_samples_per_snapshot", limits.max_samples_per_snapshot),
        ("max_snapshot_bytes", limits.max_snapshot_bytes),
        ("max_pending_snapshots", limits.max_pending_snapshots),
        (
            "max_pending_shard_windows",
            limits.max_pending_shard_windows,
        ),
        ("max_error_details", limits.max_error_details),
        ("max_procfs_file_bytes", limits.max_procfs_file_bytes),
        ("max_symbol_file_bytes", limits.max_symbol_file_bytes),
        (
            "max_metadata_string_bytes",
            limits.max_metadata_string_bytes,
        ),
        ("max_total_memory_bytes", limits.max_total_memory_bytes),
        ("worker_stack_bytes", limits.worker_stack_bytes),
    ];
    for (name, value) in values {
        if value == 0 {
            return Err(ProfilerError::invalid(
                "limits",
                format!("{name} must be non-zero"),
            ));
        }
    }
    Ok(())
}

impl Limits {
    /// Checks standalone limits without probing the host or allocating tables.
    pub fn validate(&self) -> Result<()> {
        validate_positive_limits(self)?;
        let relationships = [
            (
                self.max_monitored_cpus <= self.max_possible_cpus,
                "monitored CPUs exceed possible CPUs",
            ),
            (
                self.max_shards <= self.max_monitored_cpus,
                "shards exceed monitored CPUs",
            ),
            (
                self.max_possible_cpus <= 65_536,
                "possible CPUs exceed 65536",
            ),
            (self.max_cpu_id <= 1_048_575, "CPU ID exceeds 1048575"),
            (self.max_shards <= 256, "shards exceed 256"),
            (
                self.max_raw_events_queued <= 8192,
                "raw batch exceeds 8192 events",
            ),
            (
                self.max_pending_shard_windows <= 1024,
                "pending shard windows exceed 1024",
            ),
            (
                self.max_pending_snapshots <= 64,
                "pending snapshots exceed 64",
            ),
            (self.max_error_details <= 128, "error details exceed 128"),
            (
                self.max_metadata_string_bytes <= 4096,
                "metadata strings exceed 4096 bytes",
            ),
            (
                (256 * 1024..=16 * 1024 * 1024).contains(&self.worker_stack_bytes),
                "thread stacks must be 256 KiB through 16 MiB",
            ),
            (
                self.max_procfs_file_bytes <= 16 * 1024 * 1024,
                "procfs reads exceed 16 MiB",
            ),
            (
                self.max_symbol_file_bytes <= 1024 * 1024 * 1024,
                "symbol reads exceed 1 GiB",
            ),
            (
                self.max_stack_depth <= MAX_ABI_STACK_DEPTH,
                "stack depth exceeds the ABI",
            ),
            (
                self.max_samples_per_snapshot <= self.max_aggregation_keys,
                "snapshot samples exceed aggregation keys",
            ),
            (
                self.max_snapshot_bytes >= size_of::<crate::ProfileSnapshot>(),
                "snapshot budget cannot hold its header",
            ),
        ];
        for (valid, reason) in relationships {
            if !valid {
                return Err(ProfilerError::invalid("limits", reason));
            }
        }
        for count in [
            self.max_processes,
            self.max_threads,
            self.max_mappings,
            self.max_unique_user_stacks,
            self.max_unique_kernel_stacks,
            self.max_locations,
            self.max_functions,
            self.max_symbols,
            self.max_aggregation_keys,
            self.max_samples_per_snapshot,
        ] {
            if count > 16_777_216 {
                return Err(ProfilerError::invalid(
                    "limits",
                    "table cardinality exceeds 2^24",
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use std::{num::NonZeroUsize, time::Duration};

    use super::*;

    /// Scenario: The conservative default configuration is validated without
    /// probing the host kernel.
    /// Guarantees: Configuration validation remains non-privileged and defaults
    /// stay internally consistent.
    #[test]
    fn defaults_validate() {
        assert!(ProfilerConfig::default().validate().is_ok());
    }

    /// Scenario: A zero host-influenced capacity is configured.
    /// Guarantees: No unbounded or unusable zero-capacity state reaches startup.
    #[test]
    fn zero_limit_is_rejected() {
        let mut config = ProfilerConfig::default();
        config.limits.max_processes = 0;
        assert!(config.validate().is_err());
    }

    /// Scenario: A reporting interval would cause effectively continuous
    /// snapshot rollover.
    /// Guarantees: Reporting windows have a conservative static lower bound.
    #[test]
    fn tiny_reporting_interval_is_rejected() {
        let mut config = ProfilerConfig::default();
        config.reporting_interval = Duration::from_millis(1);
        assert!(config.validate().is_err());
    }

    /// Scenario: Fixed worker planning exceeds the configured shard bound.
    /// Guarantees: Worker counts cannot bypass max_shards.
    #[test]
    fn excessive_fixed_workers_are_rejected() {
        let mut config = ProfilerConfig::default();
        config.limits.max_shards = 2;
        config.sharding = ShardingStrategy::Fixed {
            workers: NonZeroUsize::new(3).expect("3 is non-zero"),
        };
        assert!(config.validate().is_err());
    }

    /// Scenario: An explicit CPU selection repeats a logical CPU.
    /// Guarantees: A CPU cannot be attached and assigned more than once.
    #[test]
    fn duplicate_cpu_selection_is_rejected() {
        let mut config = ProfilerConfig::default();
        config.cpu_selection = CpuSelection::Include(vec![0, 0]);
        assert!(config.validate().is_err());
    }

    /// Scenario: Default limits are evaluated using checked worst-case memory
    /// arithmetic.
    /// Guarantees: The documented constrained-agent defaults fit their total
    /// memory ceiling.
    #[test]
    fn default_memory_estimate_fits_budget() {
        let config = ProfilerConfig::default();
        let estimate = config
            .estimated_max_memory_bytes()
            .expect("default estimate should not overflow");
        assert!(estimate <= config.limits.max_total_memory_bytes);
    }

    /// Scenario: Host cardinality settings imply more memory than permitted.
    /// Guarantees: Static validation rejects the configuration before kernel
    /// resources are acquired.
    #[test]
    fn memory_budget_violation_is_rejected() {
        let mut config = ProfilerConfig::default();
        config.limits.max_total_memory_bytes = 1024;
        assert!(config.validate().is_err());
    }

    /// Scenario: Memory-estimate multiplication overflows usize.
    /// Guarantees: Arithmetic overflow is a typed configuration error rather
    /// than a wrapped under-estimate.
    #[test]
    fn memory_estimate_overflow_is_rejected() {
        let mut config = ProfilerConfig::default();
        config.limits.max_monitored_cpus = usize::MAX;
        assert!(config.estimated_max_memory_bytes().is_err());
    }

    /// Scenario: The public estimator is called before validation with zero shards.
    /// Guarantees: It returns a typed error rather than dividing by zero.
    #[test]
    fn zero_shards_cannot_panic_in_estimator() {
        let mut config = ProfilerConfig::default();
        config.limits.max_shards = 0;
        assert!(config.estimated_max_memory_bytes().is_err());
    }

    /// Scenario: Single-shard mode is permitted while the configured maximum
    /// shard count is larger.
    /// Guarantees: Pending memory budgets cover full single-shard windows.
    #[test]
    fn pending_budget_uses_largest_window_not_max_shard_division() {
        let config = ProfilerConfig {
            sharding: ShardingStrategy::Single,
            ..ProfilerConfig::default()
        };
        let estimate = config.memory_estimate().expect("default estimate is valid");
        assert_eq!(
            estimate.pending_windows,
            estimate.active_aggregation * config.limits.max_pending_shard_windows
        );
    }

    /// Scenario: A fixed stack or snapshot-local identifier limit is excessive.
    /// Guarantees: Validation rejects oversized tables before allocation.
    #[test]
    fn table_and_stack_hard_limits_are_checked() {
        let mut config = ProfilerConfig::default();
        config.limits.max_stack_depth = MAX_ABI_STACK_DEPTH + 1;
        assert!(config.validate().is_err());
        let mut config = ProfilerConfig::default();
        config.limits.max_processes = usize::MAX;
        assert!(config.validate().is_err());
    }

    /// Scenario: A CPU selection contains an out-of-range sparse identifier.
    /// Guarantees: Static validation fails without probing the host.
    #[test]
    fn sparse_cpu_identifier_is_statically_bounded() {
        let config = ProfilerConfig {
            cpu_selection: CpuSelection::Include(vec![u32::MAX]),
            ..ProfilerConfig::default()
        };
        assert!(config.validate().is_err());
    }

    /// Scenario: A sampling frequency exceeds the static supported ceiling.
    /// Guarantees: The kernel is never loaded for an invalid sample rate.
    #[test]
    fn excessive_sampling_frequency_is_rejected() {
        let config = ProfilerConfig {
            samples_per_second: NonZeroU32::new(10_001).expect("nonzero fixture"),
            ..ProfilerConfig::default()
        };
        assert!(config.validate().is_err());
    }

    /// Scenario: Kernel BTF limits are zero or would overflow initialization memory.
    /// Guarantees: Static validation fails before Aya performs its eager read.
    #[test]
    fn loader_limits_are_checked_without_kernel_access() {
        let mut config = ProfilerConfig::default();
        config.program.max_kernel_btf_types = 0;
        assert!(config.memory_estimate().is_err());
        let mut config = ProfilerConfig::default();
        config.program.max_kernel_btf_members = usize::MAX;
        assert!(config.validate().is_err());
    }

    /// Scenario: Loader initialization requires more memory than running collection.
    /// Guarantees: The peak is validated even though the phases do not overlap.
    #[test]
    fn initialization_peak_is_part_of_memory_validation() {
        let mut config = ProfilerConfig::default();
        config.program.max_kernel_btf_types = 1_000_000;
        let estimate = config
            .memory_estimate()
            .expect("arithmetic is representable");
        assert!(estimate.initialization > estimate.running_bytes().expect("running bound"));
        assert_eq!(
            estimate.total_bytes().expect("total bound"),
            estimate.initialization
        );
        assert!(config.validate().is_err());
    }
}
