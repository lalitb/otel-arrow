// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Resource limits and conservative memory estimates.

use serde::{Deserialize, Serialize};

use crate::config::BackendConfig;
use crate::error::ConfigError;

/// Explicit limits for every host-influenced allocation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResourceLimits {
    /// Maximum selected CPUs.
    pub max_monitored_cpus: usize,
    /// Maximum possible-CPU slots accepted from sysfs.
    pub max_cpu_slots: usize,
    /// Maximum sampling, notification, and lifecycle-hook perf events.
    pub max_perf_events: usize,
    /// Perf notification-buffer bytes allocated per monitored CPU.
    pub perf_buffer_bytes_per_cpu: usize,
    /// Kernel ring-buffer bytes.
    pub ring_buffer_bytes: usize,
    /// Raw events decoded in one drain.
    pub max_raw_events_per_drain: usize,
    /// Process metadata refreshes permitted in one drain iteration.
    pub max_metadata_updates_per_drain: usize,
    /// Owned raw events waiting for aggregation.
    pub max_queued_raw_events: usize,
    /// Tracked process identities.
    pub max_processes: usize,
    /// Tracked threads.
    pub max_threads: usize,
    /// Executable mappings across all processes.
    pub max_executable_mappings: usize,
    /// Distinct executable IDs with uploaded unwind state.
    pub max_executable_ids: usize,
    /// Maximum bytes read from one executable file.
    pub max_executable_file_bytes: usize,
    /// Maximum bytes read from one procfs metadata file.
    pub max_procfs_file_bytes: usize,
    /// Encoded stack-delta records.
    pub max_stack_delta_records: usize,
    /// PID/page and executable/page map entries.
    pub max_unwind_map_entries: usize,
    /// Bytes held by native unwind metadata in userspace.
    pub max_native_unwind_bytes: usize,
    /// Distinct user stacks in a reporting window.
    pub max_unique_user_stacks: usize,
    /// Distinct kernel stacks in a reporting window.
    pub max_unique_kernel_stacks: usize,
    /// Distinct aggregation keys in a reporting window.
    pub max_aggregation_keys: usize,
    /// Logical bytes retained by keys, values, and unique-stack indexes.
    pub max_aggregation_logical_bytes: usize,
    /// Frames accepted from one trace.
    pub max_frames_per_trace: usize,
    /// Interned symbol strings.
    pub max_symbols: usize,
    /// Samples emitted in one snapshot.
    pub max_samples_per_snapshot: usize,
    /// Estimated logical bytes emitted in one snapshot.
    pub max_snapshot_logical_bytes: usize,
    /// Completed snapshots waiting for a consumer.
    pub max_pending_snapshots: usize,
    /// Stored diagnostic strings.
    pub max_error_details: usize,
    /// Worker count. The current backend supports one local worker.
    pub max_worker_count: usize,
    /// Retry attempts for retryable metadata reads.
    pub max_retry_attempts: usize,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_monitored_cpus: 16,
            max_cpu_slots: 4096,
            max_perf_events: 33,
            perf_buffer_bytes_per_cpu: 16 * 1024,
            ring_buffer_bytes: 8 * 1024 * 1024,
            max_raw_events_per_drain: 4096,
            max_metadata_updates_per_drain: 8,
            max_queued_raw_events: 8192,
            max_processes: 4096,
            max_threads: 16_384,
            max_executable_mappings: 131_072,
            max_executable_ids: 4096,
            max_executable_file_bytes: 128 * 1024 * 1024,
            max_procfs_file_bytes: 8 * 1024 * 1024,
            max_stack_delta_records: 1_048_576,
            max_unwind_map_entries: 1_048_576,
            max_native_unwind_bytes: 64 * 1024 * 1024,
            max_unique_user_stacks: 65_536,
            max_unique_kernel_stacks: 16_384,
            max_aggregation_keys: 65_536,
            max_aggregation_logical_bytes: 64 * 1024 * 1024,
            max_frames_per_trace: 512,
            max_symbols: 262_144,
            max_samples_per_snapshot: 65_536,
            max_snapshot_logical_bytes: 64 * 1024 * 1024,
            max_pending_snapshots: 2,
            max_error_details: 64,
            max_worker_count: 1,
            max_retry_attempts: 2,
        }
    }
}

impl ResourceLimits {
    /// Validates non-zero limits and relationships required by kernel APIs.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let fields = [
            ("max_monitored_cpus", self.max_monitored_cpus),
            ("max_cpu_slots", self.max_cpu_slots),
            ("max_perf_events", self.max_perf_events),
            ("perf_buffer_bytes_per_cpu", self.perf_buffer_bytes_per_cpu),
            ("ring_buffer_bytes", self.ring_buffer_bytes),
            ("max_raw_events_per_drain", self.max_raw_events_per_drain),
            (
                "max_metadata_updates_per_drain",
                self.max_metadata_updates_per_drain,
            ),
            ("max_queued_raw_events", self.max_queued_raw_events),
            ("max_processes", self.max_processes),
            ("max_threads", self.max_threads),
            ("max_executable_mappings", self.max_executable_mappings),
            ("max_executable_ids", self.max_executable_ids),
            ("max_executable_file_bytes", self.max_executable_file_bytes),
            ("max_procfs_file_bytes", self.max_procfs_file_bytes),
            ("max_stack_delta_records", self.max_stack_delta_records),
            ("max_unwind_map_entries", self.max_unwind_map_entries),
            ("max_native_unwind_bytes", self.max_native_unwind_bytes),
            ("max_unique_user_stacks", self.max_unique_user_stacks),
            ("max_unique_kernel_stacks", self.max_unique_kernel_stacks),
            ("max_aggregation_keys", self.max_aggregation_keys),
            (
                "max_aggregation_logical_bytes",
                self.max_aggregation_logical_bytes,
            ),
            ("max_frames_per_trace", self.max_frames_per_trace),
            ("max_symbols", self.max_symbols),
            ("max_samples_per_snapshot", self.max_samples_per_snapshot),
            (
                "max_snapshot_logical_bytes",
                self.max_snapshot_logical_bytes,
            ),
            ("max_pending_snapshots", self.max_pending_snapshots),
            ("max_error_details", self.max_error_details),
            ("max_worker_count", self.max_worker_count),
            ("max_retry_attempts", self.max_retry_attempts),
        ];
        for (field, value) in fields {
            if value == 0 {
                return Err(ConfigError::InvalidLimit {
                    field,
                    detail: "must be non-zero",
                });
            }
            if u32::try_from(value).is_err() {
                return Err(ConfigError::InvalidLimit {
                    field,
                    detail: "must fit the 32-bit kernel capacity ABI",
                });
            }
        }
        if self.max_raw_events_per_drain < 2 {
            return Err(ConfigError::InvalidLimit {
                field: "max_raw_events_per_drain",
                detail: "must allow progress on both perf notifications and trace records",
            });
        }
        if !self.ring_buffer_bytes.is_power_of_two() || self.ring_buffer_bytes < 4096 {
            return Err(ConfigError::InvalidLimit {
                field: "ring_buffer_bytes",
                detail: "must be a power of two and at least one page",
            });
        }
        if !self.perf_buffer_bytes_per_cpu.is_power_of_two()
            || self.perf_buffer_bytes_per_cpu < 4096
        {
            return Err(ConfigError::InvalidLimit {
                field: "perf_buffer_bytes_per_cpu",
                detail: "must be a power of two and at least one page",
            });
        }
        if self.max_cpu_slots < self.max_monitored_cpus {
            return Err(ConfigError::InvalidLimit {
                field: "max_cpu_slots",
                detail: "must cover max_monitored_cpus",
            });
        }
        if self.max_frames_per_trace > crate::abi::MAX_FRAME_WORDS {
            return Err(ConfigError::InvalidLimit {
                field: "max_frames_per_trace",
                detail: "must not exceed the upstream wire-format maximum",
            });
        }
        if self.max_perf_events < self.max_monitored_cpus.saturating_mul(2).saturating_add(1) {
            return Err(ConfigError::InvalidLimit {
                field: "max_perf_events",
                detail: "must cover sampling, notification buffers, and one lifecycle hook",
            });
        }
        if self.max_worker_count != 1 {
            return Err(ConfigError::InvalidLimit {
                field: "max_worker_count",
                detail: "the native-only spike is intentionally single-worker",
            });
        }
        Ok(())
    }
}

/// Planning estimates for configured logical storage and kernel map capacities.
///
/// Allocator, verifier, and kernel-version overhead still need privileged
/// measurement; this is not an enforced process RSS or physical-memory limit.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct MemoryEstimate {
    /// Owned userspace queues, aggregation, metadata, and snapshots.
    pub userspace_bytes: u64,
    /// BPF map memory, including a fixed hash-entry overhead estimate.
    pub kernel_map_bytes: u64,
    /// Ring-buffer and perf-buffer memory.
    pub event_buffer_bytes: u64,
    /// Sum of all three estimates.
    pub combined_bytes: u64,
}

impl MemoryEstimate {
    /// Estimates configured storage, including duplicated state and transient templates.
    #[must_use]
    pub fn for_config(config: &BackendConfig, possible_cpus: usize) -> Self {
        let limits = &config.limits;
        let cpu_count = possible_cpus as u64;
        let monitored = limits.max_monitored_cpus as u64;
        let trace_bytes = (crate::abi::TRACE_MAX_SIZE + 7 + size_of::<Vec<u8>>()) as u64;
        let raw_queue = (limits.max_queued_raw_events as u64).saturating_mul(trace_bytes);
        let processes = (limits.max_processes as u64)
            .saturating_mul(3 * (crate::process::MAX_EXECUTABLE_PATH_BYTES as u64 + 256));
        let mappings = (limits.max_executable_mappings as u64)
            .saturating_mul(3 * (crate::mappings::MAX_MAPS_LINE_BYTES as u64 + 192));
        let decoded_batch = (limits.max_raw_events_per_drain as u64).saturating_mul(
            trace_bytes.saturating_add((limits.max_frames_per_trace as u64).saturating_mul(64)),
        );
        let snapshots = (limits.max_pending_snapshots as u64)
            .saturating_add(1)
            .saturating_mul((limits.max_snapshot_logical_bytes as u64).saturating_add(1024));
        let userspace_bytes = [
            raw_queue,
            processes,
            mappings,
            decoded_batch,
            snapshots,
            (limits.max_aggregation_logical_bytes as u64).saturating_mul(2),
            limits.max_native_unwind_bytes as u64,
            limits.max_executable_file_bytes as u64,
            limits.max_procfs_file_bytes as u64,
            (limits.max_unwind_map_entries as u64).saturating_mul(32),
            (limits.max_error_details as u64).saturating_mul(512 + 32),
            // Headroom for the pinned artifact, BTF, and loader/verifier data.
            64 * 1024 * 1024,
        ]
        .into_iter()
        .fold(0_u64, u64::saturating_add);

        const HASH_OVERHEAD: u64 = 64;
        let pid_pages =
            (limits.max_unwind_map_entries as u64).saturating_mul(16 + 16 + HASH_OVERHEAD);
        let stack_pages =
            (limits.max_unwind_map_entries as u64).saturating_mul(16 + 8 + HASH_OVERHEAD);
        // Arrays round each four-byte value to eight bytes. The prototype uses
        // bucket 8; every live executable holds a complete 256-entry inner map.
        let stack_deltas = (limits.max_executable_ids as u64).saturating_mul(256 * 8);
        let outer_maps =
            (limits.max_executable_ids as u64).saturating_mul(16 * (8 + 4 + HASH_OVERHEAD));
        let unwind_infos = 16_384_u64 * 16;
        let per_cpu_records = cpu_count.saturating_mul(2 * 26_640 + 118 * 8 + 16);
        let pid_events = (limits.max_processes as u64)
            .saturating_add(limits.max_threads as u64)
            .saturating_mul(8 + 8 + HASH_OVERHEAD);
        let reported_pids = (limits.max_processes as u64).saturating_mul(4 + 8 + HASH_OVERHEAD);
        // Aya creates one transient template at a time, including bucket 23.
        let fixed_maps = (1_u64 << 23) * 8 + 1024 * 1024;
        let kernel_map_bytes = pid_pages
            .saturating_add(stack_pages)
            .saturating_add(stack_deltas)
            .saturating_add(outer_maps)
            .saturating_add(pid_events)
            .saturating_add(reported_pids)
            .saturating_add(unwind_infos)
            .saturating_add(per_cpu_records)
            .saturating_add(fixed_maps);

        // Include perf metadata pages and ring metadata. Use the largest page
        // size of the two analyzed architectures as planning headroom.
        let perf = monitored
            .saturating_mul((limits.perf_buffer_bytes_per_cpu as u64).saturating_add(65_536));
        let event_buffer_bytes = perf
            .saturating_add(limits.ring_buffer_bytes as u64)
            .saturating_add(2 * 65_536);
        Self {
            userspace_bytes,
            kernel_map_bytes,
            event_buffer_bytes,
            combined_bytes: userspace_bytes
                .saturating_add(kernel_map_bytes)
                .saturating_add(event_buffer_bytes),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::Sha256Digest;
    use std::path::PathBuf;

    /// Scenario: Default resource limits are used on a host with 64 possible CPUs.
    /// Guarantees: Every memory class has a finite non-zero maximum and the
    /// combined estimate is the saturating sum of those classes.
    #[test]
    fn default_memory_estimate_is_finite() {
        let config = BackendConfig::new(PathBuf::from("/artifact.o"), Sha256Digest::ZERO, "test");
        let estimate = MemoryEstimate::for_config(&config, 64);
        assert!(estimate.userspace_bytes > 0);
        assert!(estimate.kernel_map_bytes > 0);
        assert!(estimate.event_buffer_bytes > 0);
        assert_eq!(
            estimate.combined_bytes,
            estimate
                .userspace_bytes
                .saturating_add(estimate.kernel_map_bytes)
                .saturating_add(estimate.event_buffer_bytes)
        );
    }

    /// Scenario: A ring-buffer limit is not a kernel-compatible power of two.
    /// Guarantees: Validation rejects it before Aya attempts map creation.
    #[test]
    fn invalid_ring_buffer_size_is_rejected() {
        let limits = ResourceLimits {
            ring_buffer_bytes: 6000,
            ..ResourceLimits::default()
        };
        assert!(matches!(
            limits.validate(),
            Err(ConfigError::InvalidLimit {
                field: "ring_buffer_bytes",
                ..
            })
        ));
    }

    /// Scenario: Unvalidated configuration fields approach the machine integer maximum.
    /// Guarantees: Memory estimation saturates instead of panicking or wrapping to a small value.
    #[test]
    fn extreme_memory_estimates_do_not_overflow() {
        let mut config =
            BackendConfig::new(PathBuf::from("/artifact.o"), Sha256Digest::ZERO, "test");
        config.limits.max_executable_mappings = usize::MAX;
        config.limits.max_raw_events_per_drain = usize::MAX;
        let estimate = MemoryEstimate::for_config(&config, usize::MAX);
        assert_eq!(estimate.combined_bytes, u64::MAX);
        assert!(config.validate().is_err());
    }

    /// Scenario: Each configured resource limit is independently set to zero.
    /// Guarantees: Every declared limit participates in non-privileged validation.
    #[test]
    fn every_resource_limit_is_validated() {
        let defaults = serde_json::to_value(ResourceLimits::default()).expect("limits");
        for field in defaults.as_object().expect("limit object").keys() {
            let mut candidate = defaults.clone();
            candidate[field] = serde_json::json!(0);
            let limits: ResourceLimits = serde_json::from_value(candidate).expect("limits");
            assert!(limits.validate().is_err(), "unvalidated limit: {field}");
        }
    }
}
