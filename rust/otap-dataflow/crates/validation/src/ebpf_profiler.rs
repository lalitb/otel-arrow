// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Pinned OpenTelemetry eBPF profiler container configuration.

use crate::ContainerConfig;

/// Official eBPF-profiler Collector image repository.
pub const PROFILER_IMAGE: &str = "otel/opentelemetry-collector-ebpf-profiler";

/// Pinned Collector release and manifest digest used by the smoke harness.
pub const PROFILER_IMAGE_TAG: &str =
    "0.159.0@sha256:90d6b6536ce0283d706f7e7b6c45f534c65b140ff6ec456c19385e50a7d12b8e";

/// Log marker emitted after the profiler attaches its scheduler monitor.
pub const PROFILER_READY_LOG: &str = "Attached sched monitor";

/// Linux capabilities required by the upstream profiler deployment.
pub const PROFILER_CAPABILITIES: &[&str] = &[
    "BPF",
    "PERFMON",
    "SYS_PTRACE",
    "SYS_RESOURCE",
    "DAC_READ_SEARCH",
    "SYSLOG",
    "CHECKPOINT_RESTORE",
    "IPC_LOCK",
];

/// Build the least-privilege container configuration used for a manual eBPF smoke test.
///
/// The container shares the host PID and network namespaces, mounts tracefs and
/// the supplied Collector configuration read-only, and exports OTLP Profiles to
/// `otlp_endpoint`.
#[must_use]
pub fn profiler_container(
    host_config_path: impl Into<String>,
    otlp_endpoint: impl Into<String>,
) -> ContainerConfig {
    let mut config = ContainerConfig::new(PROFILER_IMAGE, PROFILER_IMAGE_TAG)
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", otlp_endpoint)
        .command(["--config=/etc/otelcol-ebpf-profiler/config.yaml"])
        .bind_mount(
            host_config_path,
            "/etc/otelcol-ebpf-profiler/config.yaml",
            true,
        )
        .bind_mount("/sys/kernel/tracing", "/sys/kernel/tracing", true)
        .host_pid(true)
        .host_network(true)
        .security_opt("seccomp=unconfined")
        .security_opt("apparmor=unconfined")
        .wait_for_log(PROFILER_READY_LOG);
    for capability in PROFILER_CAPABILITIES {
        config = config.cap_add(*capability);
    }
    config
}

#[cfg(test)]
mod tests {
    use super::*;
    use testcontainers::core::AccessMode;

    /// Scenario: The pinned profiler container is prepared for host-wide eBPF collection.
    /// Guarantees: Digest pinning, namespaces, capabilities, mounts, and startup gating stay fixed.
    #[test]
    fn profiler_container_has_required_host_access() {
        let config = profiler_container("/tmp/profiler.yaml", "127.0.0.1:14317");

        assert_eq!(config.image, PROFILER_IMAGE);
        assert_eq!(config.tag, PROFILER_IMAGE_TAG);
        assert!(config.host_pid);
        assert!(config.host_network);
        assert!(!config.privileged);
        assert_eq!(config.cap_add, PROFILER_CAPABILITIES);
        assert_eq!(config.mounts.len(), 2);
        assert!(
            config
                .mounts
                .iter()
                .all(|mount| mount.access_mode() == AccessMode::ReadOnly)
        );
        assert_eq!(
            config.security_opts,
            vec!["seccomp=unconfined", "apparmor=unconfined"]
        );
        assert_eq!(
            config.command,
            vec!["--config=/etc/otelcol-ebpf-profiler/config.yaml"]
        );
    }

    /// Scenario: Rust and shell eBPF launchers reference the same external contract.
    /// Guarantees: Image pinning, capabilities, readiness, and Collector limits cannot drift silently.
    #[test]
    fn shell_harness_matches_pinned_container_contract() {
        let script = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tools/profiles/run-ebpf-smoke.sh"
        ));
        let collector_config = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tools/profiles/ebpf-profiler-config.yaml"
        ));
        let image_reference = format!("{PROFILER_IMAGE}:{PROFILER_IMAGE_TAG}");

        assert!(script.contains(&image_reference));
        assert!(script.contains(PROFILER_READY_LOG));
        for capability in PROFILER_CAPABILITIES {
            assert!(script.contains(&format!("--cap-add {capability}")));
        }
        assert!(collector_config.contains("reporter_interval: 1s"));
        assert!(collector_config.contains("reporter_jitter: 0"));
        assert!(collector_config.contains("filter_min_process_age: 0s"));
        assert!(collector_config.contains("max_rpc_msg_size: 33554432"));
    }
}
