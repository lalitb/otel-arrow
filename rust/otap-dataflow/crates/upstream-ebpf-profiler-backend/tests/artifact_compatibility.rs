// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Non-privileged compatibility tests for an externally supplied artifact.

use otel_arrow_dfe_upstream_ebpf_profiler_backend::{
    AMD64_COMPATIBILITY_MANIFEST, ARM64_COMPATIBILITY_MANIFEST, CompatibilityManifest,
    CompatibilityValidator, UpstreamArtifact,
};

/// Scenario: The checked-in amd64 manifest is parsed without accessing a kernel.
/// Guarantees: It pins the complete 38-program, 47-map, 23-global contract and
/// a valid 13-slot tail-call table.
#[test]
fn checked_in_manifest_is_complete() {
    let manifest = CompatibilityManifest::from_json(AMD64_COMPATIBILITY_MANIFEST)
        .expect("checked-in manifest");
    manifest.validate_topology().expect("tail-call topology");
    assert_eq!(manifest.artifact.programs.len(), 38);
    assert_eq!(manifest.artifact.maps.len(), 47);
    assert_eq!(manifest.artifact.globals.len(), 23);
    assert_eq!(manifest.tail_calls.len(), 13);
    assert_eq!(
        manifest
            .artifact
            .maps
            .iter()
            .filter(|map| map.inner.is_some())
            .count(),
        16
    );
}

/// Scenario: `UPSTREAM_EBPF_OBJECT` identifies the pinned external amd64 ELF.
/// Guarantees: Pure-Rust inspection reproduces every checked-in program, map,
/// global, inner template, architecture, license, and artifact byte hash.
#[test]
fn external_artifact_matches_manifest_when_configured() {
    let Ok(path) = std::env::var("UPSTREAM_EBPF_OBJECT") else {
        return;
    };
    let manifest = CompatibilityManifest::from_json(AMD64_COMPATIBILITY_MANIFEST)
        .expect("checked-in manifest");
    let version = manifest.compatibility_version.clone();
    let validator = CompatibilityValidator::new(manifest).expect("valid manifest");
    let artifact = UpstreamArtifact::open(path).expect("external artifact");
    validator
        .validate(&artifact, &version)
        .expect("pinned artifact compatibility");
}

/// Scenario: Both pinned architecture manifests are inspected without their external ELF files.
/// Guarantees: Architecture and hash differ while the fixed event/map ABI and tail-call slots agree.
#[test]
fn architecture_manifests_pin_distinct_artifacts() {
    let amd64 = CompatibilityManifest::from_json(AMD64_COMPATIBILITY_MANIFEST).expect("amd64");
    let arm64 = CompatibilityManifest::from_json(ARM64_COMPATIBILITY_MANIFEST).expect("arm64");
    arm64.validate_topology().expect("arm64 topology");
    assert_ne!(amd64.artifact.sha256, arm64.artifact.sha256);
    assert_ne!(amd64.artifact.architecture, arm64.artifact.architecture);
    assert_eq!(amd64.tail_calls, arm64.tail_calls);
    for name in [
        "pid_page_to_mapping_info",
        "stack_delta_page_to_info",
        "unwind_info_array",
    ] {
        let left = amd64
            .artifact
            .maps
            .iter()
            .find(|map| map.name == name)
            .expect("amd64 map");
        let right = arm64
            .artifact
            .maps
            .iter()
            .find(|map| map.name == name)
            .expect("arm64 map");
        assert_eq!(left, right);
    }
}
