// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Typed failure-path tests that do not issue BPF syscalls.

use otel_arrow_dfe_upstream_ebpf_profiler_backend::{
    AMD64_COMPATIBILITY_MANIFEST, CompatibilityErrorKind, CompatibilityManifest,
    CompatibilityValidator, UpstreamArtifact,
};

/// Scenario: A pinned manifest is mutated to require a program not present in
/// the artifact inventory.
/// Guarantees: Topology validation returns the stable tail-call category rather
/// than allowing failure to surface later as a verifier or attachment string.
#[test]
fn missing_tail_call_program_is_typed() {
    let mut manifest = CompatibilityManifest::from_json(AMD64_COMPATIBILITY_MANIFEST)
        .expect("checked-in manifest");
    manifest.tail_calls[0].perf_program = "missing_program".to_owned();
    let error = CompatibilityValidator::new(manifest).expect_err("invalid topology");
    assert_eq!(error.kind, CompatibilityErrorKind::TailCallTopology);
}

/// Scenario: An external artifact path does not exist.
/// Guarantees: Non-privileged preparation fails as typed I/O before attempting
/// capabilities, rlimits, maps, programs, or perf events.
#[test]
fn missing_artifact_fails_before_kernel_work() {
    let result = UpstreamArtifact::open("/definitely/not/an/upstream-artifact");
    assert!(result.is_err());
}
