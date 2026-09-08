// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Deterministic artifact inventory and compatibility manifest types.

use serde::{Deserialize, Serialize};

use crate::artifact::{ArtifactArchitecture, Sha256Digest};
use crate::error::{BackendError, CompatibilityError, CompatibilityErrorKind};

/// Program metadata discovered without loading anything into the kernel.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct ProgramInventory {
    /// ELF/BTF program name.
    pub name: String,
    /// ELF section name.
    pub section: String,
    /// Kernel program type inferred from the section.
    pub program_type: String,
    /// Instruction count before relocations.
    pub instruction_count: usize,
}

/// Map metadata, including a map-in-map template when present.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct MapInventory {
    /// ELF/BTF map name.
    pub name: String,
    /// Numeric Linux BPF map type.
    pub map_type: u32,
    /// Stable readable map type.
    pub map_type_name: String,
    /// Key size in bytes.
    pub key_size: u32,
    /// Value size in bytes.
    pub value_size: u32,
    /// Default maximum entries.
    pub max_entries: u32,
    /// Kernel map flags.
    pub flags: u32,
    /// Optional inner-map template.
    pub inner: Option<InnerMapInventory>,
}

/// Inner-map template metadata.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct InnerMapInventory {
    /// Numeric Linux BPF map type.
    pub map_type: u32,
    /// Stable readable map type.
    pub map_type_name: String,
    /// Key size in bytes.
    pub key_size: u32,
    /// Value size in bytes.
    pub value_size: u32,
    /// Default maximum entries.
    pub max_entries: u32,
    /// Kernel map flags.
    pub flags: u32,
}

/// One mutable load-time global in `.rodata.var`.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct GlobalInventory {
    /// ELF symbol name.
    pub name: String,
    /// Byte offset in `.rodata.var`.
    pub offset: u64,
    /// ABI size in bytes.
    pub size: u64,
    /// Initial bytes represented as lowercase hexadecimal.
    pub default_hex: String,
}

/// Full deterministic inventory of one upstream artifact.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ArtifactInventory {
    /// SHA-256 of the complete ELF.
    pub sha256: Sha256Digest,
    /// Architecture encoded by the architecture-specific BTF types.
    pub architecture: ArtifactArchitecture,
    /// ELF e_machine label.
    pub elf_machine: String,
    /// License string from the object.
    pub license: String,
    /// Sorted programs.
    pub programs: Vec<ProgramInventory>,
    /// Sorted maps.
    pub maps: Vec<MapInventory>,
    /// Sorted mutable globals.
    pub globals: Vec<GlobalInventory>,
}

/// One upstream tail-call table entry.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct TailCallInventory {
    /// Numeric `TracePrograms` index.
    pub index: u32,
    /// Logical unwinder name.
    pub name: String,
    /// Perf-event program name.
    pub perf_program: String,
    /// Probe program name.
    pub probe_program: String,
    /// Whether the native-only implementation enables this entry.
    pub native_only: bool,
}

/// Checked-in compatibility contract for one upstream artifact.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CompatibilityManifest {
    /// Manifest schema version.
    pub schema_version: u32,
    /// Human-readable compatibility label used by configuration.
    pub compatibility_version: String,
    /// Exact upstream Git commit.
    pub upstream_commit: String,
    /// Discovered artifact inventory.
    pub artifact: ArtifactInventory,
    /// Expected tail-call topology.
    pub tail_calls: Vec<TailCallInventory>,
}

impl CompatibilityManifest {
    /// Parses a checked-in manifest.
    pub fn from_json(json: &str) -> Result<Self, BackendError> {
        if json.len() > 1024 * 1024 {
            return Err(BackendError::Capacity("compatibility manifest bytes"));
        }
        serde_json::from_str(json)
            .map_err(|error| BackendError::ArtifactParse(format!("manifest JSON: {error}")))
    }

    /// Serializes a manifest in deterministic pretty JSON.
    pub fn to_pretty_json(&self) -> Result<String, BackendError> {
        let mut json = serde_json::to_string_pretty(self)
            .map_err(|error| BackendError::ArtifactParse(format!("manifest JSON: {error}")))?;
        json.push('\n');
        Ok(json)
    }

    /// Creates a manifest from discovered inventory and the pinned topology.
    pub fn from_inventory(
        compatibility_version: impl Into<String>,
        upstream_commit: impl Into<String>,
        artifact: ArtifactInventory,
    ) -> Self {
        Self {
            schema_version: 1,
            compatibility_version: compatibility_version.into(),
            upstream_commit: upstream_commit.into(),
            artifact,
            tail_calls: pinned_tail_calls(),
        }
    }

    /// Checks internal manifest invariants before comparing an artifact.
    pub fn validate_topology(&self) -> Result<(), CompatibilityError> {
        if self.schema_version != 1 || self.upstream_commit != crate::PINNED_UPSTREAM_COMMIT {
            return Err(CompatibilityError::new(
                CompatibilityErrorKind::ManifestVersion,
                "this backend supports schema 1 and the pinned upstream commit only",
            ));
        }
        if self.tail_calls != pinned_tail_calls() {
            return Err(CompatibilityError::new(
                CompatibilityErrorKind::TailCallTopology,
                "tail-call destinations and enabled slots must match the pinned ABI",
            ));
        }
        let mut indexes = std::collections::BTreeSet::new();
        for tail_call in &self.tail_calls {
            if tail_call.index >= 13 || !indexes.insert(tail_call.index) {
                return Err(CompatibilityError::new(
                    CompatibilityErrorKind::TailCallTopology,
                    format!("invalid or duplicate tail-call index {}", tail_call.index),
                ));
            }
            for name in ["perf_progs", "kprobe_progs"] {
                if !self.artifact.maps.iter().any(|map| {
                    map.name == name
                        && map.map_type == 3
                        && map.max_entries == 13
                        && map.key_size == 4
                        && map.value_size == 4
                }) {
                    return Err(CompatibilityError::new(
                        CompatibilityErrorKind::TailCallTopology,
                        format!(
                            "{name} must be a 13-slot program array with four-byte keys and values"
                        ),
                    ));
                }
            }
            for program in [&tail_call.perf_program, &tail_call.probe_program] {
                if !self
                    .artifact
                    .programs
                    .iter()
                    .any(|candidate| &candidate.name == program)
                {
                    return Err(CompatibilityError::new(
                        CompatibilityErrorKind::TailCallTopology,
                        format!("tail-call program {program} is absent"),
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Returns the pinned `TracePrograms` table from upstream `support/ebpf/types.h`.
#[must_use]
pub fn pinned_tail_calls() -> Vec<TailCallInventory> {
    [
        (0, "unwind_stop", true),
        (1, "unwind_native", true),
        (2, "unwind_hotspot", false),
        (3, "unwind_perl", false),
        (4, "unwind_python", false),
        (5, "unwind_php", false),
        (6, "unwind_ruby", false),
        (7, "unwind_v8", false),
        (8, "unwind_dotnet", false),
        (9, "unwind_dotnet10", false),
        (10, "go_labels", false),
        (11, "unwind_beam", false),
        (12, "unwind_luajit", false),
    ]
    .into_iter()
    .map(|(index, name, native_only)| TailCallInventory {
        index,
        name: name.to_owned(),
        perf_program: format!("perf_{name}"),
        probe_program: format!("kprobe_{name}"),
        native_only,
    })
    .collect()
}

/// Stable name for a Linux BPF map type.
#[must_use]
pub fn map_type_name(map_type: u32) -> &'static str {
    match map_type {
        1 => "hash",
        2 => "array",
        3 => "program_array",
        4 => "perf_event_array",
        6 => "per_cpu_array",
        9 => "lru_hash",
        10 => "lru_per_cpu_hash",
        11 => "lpm_trie",
        12 => "array_of_maps",
        13 => "hash_of_maps",
        27 => "ring_buffer",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: The pinned tail-call list is materialized without an artifact.
    /// Guarantees: Every numeric slot is unique, bounded by the upstream
    /// program-array size, and native-only mode enables only stop and native.
    #[test]
    fn pinned_tail_calls_are_complete_and_unique() {
        let calls = pinned_tail_calls();
        assert_eq!(calls.len(), 13);
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.native_only)
                .map(|call| call.index)
                .collect::<Vec<_>>(),
            [0, 1]
        );
        assert_eq!(
            calls
                .iter()
                .map(|call| call.index)
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            13
        );
    }

    /// Scenario: A manifest omits a tail-call slot, swaps destinations, or changes its revision.
    /// Guarantees: Incomplete or unsupported contracts cannot authorize the hard-coded loader ABI.
    #[test]
    fn manifest_rejects_incomplete_or_unsupported_contracts() {
        let baseline = CompatibilityManifest::from_json(crate::AMD64_COMPATIBILITY_MANIFEST)
            .expect("manifest");
        let mut missing = baseline.clone();
        let _removed = missing.tail_calls.pop();
        assert_eq!(
            missing.validate_topology().expect_err("missing slot").kind,
            CompatibilityErrorKind::TailCallTopology
        );
        let mut changed = baseline.clone();
        changed.tail_calls[0].perf_program = "perf_unwind_native".to_owned();
        assert_eq!(
            changed.validate_topology().expect_err("changed slot").kind,
            CompatibilityErrorKind::TailCallTopology
        );
        let mut future = baseline;
        future.schema_version = 2;
        assert_eq!(
            future.validate_topology().expect_err("future schema").kind,
            CompatibilityErrorKind::ManifestVersion
        );
    }
}
