// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Checked native-probe map binding, equivalent to upstream userspace relocation.
//!
//! Native sampling changes six ELF64 REL symbol references only. The separate
//! analysis policy changes one checked stack-load offset to select a TID rather
//! than a TGID. External files, licenses, and unrelated sections stay unchanged.

use object::{Object, ObjectSection, ObjectSymbol, SymbolKind};
use serde::Serialize;

use crate::artifact::{ArtifactArchitecture, Sha256Digest, UpstreamArtifact};
use crate::error::BackendError;

const SECTIONS: [(&str, &str); 2] = [
    ("kprobe/unwind_native", ".relkprobe/unwind_native"),
    ("kprobe/unwind_stop", ".relkprobe/unwind_stop"),
];
const MAP_NAMES: [(&str, &str); 2] = [
    ("per_cpu_records", "per_cpu_records_kp"),
    ("perf_progs", "kprobe_progs"),
];
const EXPECTED_BINDINGS: [usize; 2] = [2, 1];
const REL_SIZE: usize = 16;
const AMD64_INPUT: &str = "0f186e7f99d2b544fc69ec02834ebfb902e09e4ab89eda51436a4ad8a2ec7282";
const ARM64_INPUT: &str = "200a60f6bc10f8b964fd2b8170750f90bb7514bf73761948467c275b11d0849b";

/// Typed failures in the pinned preparation policy.
#[derive(Debug, thiserror::Error)]
pub enum PreparationError {
    /// Preparation is restricted to the two audited artifact fingerprints.
    #[error("no preparation policy for artifact {digest}")]
    UnsupportedArtifact {
        /// Rejected original fingerprint.
        digest: Sha256Digest,
    },
    /// ELF metadata was malformed.
    #[error("ELF preparation failed: {0}")]
    Elf(#[from] object::Error),
    /// A required section or symbol was absent.
    #[error("native-probe preparation requires {name}")]
    Missing {
        /// Required pinned object name.
        name: &'static str,
    },
    /// Source and replacement map definitions differed.
    #[error("native-probe maps {source_map} and {target_map} have different ABIs")]
    MapAbi {
        /// Original map.
        source_map: &'static str,
        /// Replacement map.
        target_map: &'static str,
    },
    /// A relocation or its instruction range did not match the pinned policy.
    #[error("invalid native-probe relocation in {section} at byte {offset}")]
    Relocation {
        /// Executable section.
        section: &'static str,
        /// Offset within the relocation section.
        offset: usize,
    },
    /// The policy did not find exactly the reviewed number of bindings.
    #[error(
        "native-probe binding count for {map} in {section}: expected {expected}, found {actual}"
    )]
    BindingCount {
        /// Executable section.
        section: &'static str,
        /// Original map.
        map: &'static str,
        /// Expected binding count.
        expected: usize,
        /// Observed binding count.
        actual: usize,
    },
    /// Preparation produced bytes other than the independently established output.
    #[error("prepared artifact fingerprint mismatch: expected {expected}, found {actual}")]
    Fingerprint {
        /// Pinned prepared fingerprint.
        expected: Sha256Digest,
        /// Actual prepared fingerprint.
        actual: Sha256Digest,
    },
    /// The analysis instruction did not match the audited original encoding.
    #[error("thread-scoped analysis instruction does not match the pinned policy")]
    AnalysisInstruction,
}

/// One precisely scoped map association supplied by the Rust control plane.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProbeMapBinding {
    /// Executable section containing the map load.
    pub section: &'static str,
    /// Byte offset of the LDDW instruction within that section.
    pub instruction_offset: u64,
    /// File offset of the eight-byte ELF relocation-info field.
    pub relocation_info_offset: usize,
    /// Original map name.
    pub source_map: &'static str,
    /// Replacement map name.
    pub target_map: &'static str,
    /// Original ELF symbol index.
    pub source_symbol: u32,
    /// Replacement ELF symbol index.
    pub target_symbol: u32,
}

/// Provenance for an in-memory preparation; contains no executable bytes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PreparationReport {
    /// Explicit upstream revision governing the transformation.
    pub upstream_commit: &'static str,
    /// Version of this fixed native-probe preparation policy.
    pub policy: &'static str,
    /// Original external artifact fingerprint.
    pub original_sha256: Sha256Digest,
    /// Prepared in-memory fingerprint.
    pub prepared_sha256: Sha256Digest,
    /// The six audited native-probe map associations.
    pub bindings: Vec<ProbeMapBinding>,
    /// A separately audited analysis-only instruction change, if applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instruction_patch: Option<AnalysisInstructionPatch>,
}

/// Exact provenance for the single-threaded analysis request filter.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AnalysisInstructionPatch {
    /// ELF program section.
    pub section: &'static str,
    /// Byte offset within that section.
    pub section_offset: u64,
    /// Original eight-byte instruction.
    pub original_hex: &'static str,
    /// Replacement eight-byte instruction.
    pub prepared_hex: &'static str,
    /// Required namespace configuration for this policy.
    pub requirement: &'static str,
}

/// Owned loader input with audited native-perf and native-probe map bindings.
pub struct PreparedObject {
    bytes: Vec<u8>,
    report: PreparationReport,
}

impl PreparedObject {
    /// Prepares the pinned native probe chain without modifying the external artifact.
    pub fn native_probe_maps(artifact: &UpstreamArtifact) -> Result<Self, BackendError> {
        let original = artifact.inventory().sha256;
        let (input, output) = match artifact.inventory().architecture {
            ArtifactArchitecture::Amd64 => (
                AMD64_INPUT,
                "0e1da58748c2e6df2a933bd3c7d61bc8b4a8c2edcc3db1ff2ad45641b72e4b2e",
            ),
            ArtifactArchitecture::Arm64 => (
                ARM64_INPUT,
                "23105184886581a1dbd8309f8f7c4cfaecb5d359995bb1f501aadbdf3057529a",
            ),
            ArtifactArchitecture::Unknown => {
                return Err(PreparationError::UnsupportedArtifact { digest: original }.into());
            }
        };
        if original != input.parse::<Sha256Digest>()? {
            return Err(PreparationError::UnsupportedArtifact { digest: original }.into());
        }
        let file = object::File::parse(artifact.bytes()).map_err(PreparationError::from)?;
        let mut symbols = [(0_u32, 0_u32); 2];
        for (index, (source_map, target_map)) in MAP_NAMES.into_iter().enumerate() {
            let source = artifact
                .inventory()
                .maps
                .iter()
                .find(|map| map.name == source_map)
                .ok_or(PreparationError::Missing { name: source_map })?;
            let target = artifact
                .inventory()
                .maps
                .iter()
                .find(|map| map.name == target_map)
                .ok_or(PreparationError::Missing { name: target_map })?;
            if (
                source.map_type,
                source.key_size,
                source.value_size,
                source.max_entries,
                source.flags,
            ) != (
                target.map_type,
                target.key_size,
                target.value_size,
                target.max_entries,
                target.flags,
            ) {
                return Err(PreparationError::MapAbi {
                    source_map,
                    target_map,
                }
                .into());
            }
            symbols[index] = (
                map_symbol(&file, source_map)?,
                map_symbol(&file, target_map)?,
            );
        }
        let mut bindings = Vec::with_capacity(6);
        for (section_name, relocation_name) in SECTIONS {
            let code = file
                .section_by_name(section_name)
                .ok_or(PreparationError::Missing { name: section_name })?
                .data()
                .map_err(PreparationError::from)?;
            let relocations =
                file.section_by_name(relocation_name)
                    .ok_or(PreparationError::Missing {
                        name: relocation_name,
                    })?;
            let (offset, _) = relocations.file_range().ok_or(PreparationError::Missing {
                name: relocation_name,
            })?;
            let offset = usize::try_from(offset).map_err(|_| PreparationError::Relocation {
                section: section_name,
                offset: 0,
            })?;
            bindings.extend(collect_bindings(
                section_name,
                code,
                relocations.data().map_err(PreparationError::from)?,
                offset,
                &symbols,
            )?);
        }
        let mut bytes = artifact.bytes().to_vec();
        for binding in &bindings {
            let start = binding.relocation_info_offset;
            let end = start.checked_add(8).ok_or(PreparationError::Relocation {
                section: binding.section,
                offset: start,
            })?;
            let info = bytes
                .get_mut(start..end)
                .ok_or(PreparationError::Relocation {
                    section: binding.section,
                    offset: start,
                })?;
            info.copy_from_slice(&((u64::from(binding.target_symbol) << 32) | 1).to_le_bytes());
        }
        let prepared = Sha256Digest::of_bytes(&bytes);
        let expected = output.parse::<Sha256Digest>()?;
        if prepared != expected {
            return Err(PreparationError::Fingerprint {
                expected,
                actual: prepared,
            }
            .into());
        }
        Ok(Self {
            bytes,
            report: PreparationReport {
                upstream_commit: crate::PINNED_UPSTREAM_COMMIT,
                policy: "native-probe-map-bindings-v1",
                original_sha256: original,
                prepared_sha256: prepared,
                bindings,
                instruction_patch: None,
            },
        })
    }

    /// Restricts temporary task analysis to the requesting TID in its own PID namespace.
    ///
    /// The caller must enable namespace translation to its current namespace
    /// and put gettid(), not getpid(), in the request's `pid` field.
    pub fn thread_scoped_analysis(artifact: &UpstreamArtifact) -> Result<Self, BackendError> {
        const SECTION: &str = "raw_tracepoint/sys_enter";
        const OFFSET: usize = 0xd8;
        const ORIGINAL: [u8; 8] = [0x61, 0xa0, 0xfc, 0xff, 0, 0, 0, 0];
        const PREPARED: [u8; 8] = [0x61, 0xa0, 0xf8, 0xff, 0, 0, 0, 0];
        let original = artifact.inventory().sha256;
        let (input, output) = match artifact.inventory().architecture {
            ArtifactArchitecture::Amd64 => (
                AMD64_INPUT,
                "5f2ba2305429daa29c57f54f219928de743ff83af3629c805a35c6663b7ebaeb",
            ),
            ArtifactArchitecture::Arm64 => (
                ARM64_INPUT,
                "fc5bc74b29db811f0cbcffd1925d71417d363d4c1660b8368f42fa5c9131beb5",
            ),
            ArtifactArchitecture::Unknown => {
                return Err(PreparationError::UnsupportedArtifact { digest: original }.into());
            }
        };
        if original != input.parse::<Sha256Digest>()? {
            return Err(PreparationError::UnsupportedArtifact { digest: original }.into());
        }
        let file = object::File::parse(artifact.bytes()).map_err(PreparationError::from)?;
        let section = file
            .section_by_name(SECTION)
            .ok_or(PreparationError::Missing { name: SECTION })?;
        if section
            .data()
            .map_err(PreparationError::from)?
            .get(OFFSET..OFFSET + 8)
            != Some(ORIGINAL.as_slice())
        {
            return Err(PreparationError::AnalysisInstruction.into());
        }
        let (start, _) = section
            .file_range()
            .ok_or(PreparationError::AnalysisInstruction)?;
        let start = usize::try_from(start)
            .ok()
            .and_then(|start| start.checked_add(OFFSET))
            .ok_or(PreparationError::AnalysisInstruction)?;
        let end = start
            .checked_add(8)
            .ok_or(PreparationError::AnalysisInstruction)?;
        let mut bytes = artifact.bytes().to_vec();
        bytes
            .get_mut(start..end)
            .ok_or(PreparationError::AnalysisInstruction)?
            .copy_from_slice(&PREPARED);
        let prepared = Sha256Digest::of_bytes(&bytes);
        let expected = output.parse::<Sha256Digest>()?;
        if prepared != expected {
            return Err(PreparationError::Fingerprint {
                expected,
                actual: prepared,
            }
            .into());
        }
        Ok(Self {
            bytes,
            report: PreparationReport {
                upstream_commit: crate::PINNED_UPSTREAM_COMMIT,
                policy: "thread-scoped-task-analysis-v1",
                original_sha256: original,
                prepared_sha256: prepared,
                bindings: Vec::new(),
                instruction_patch: Some(AnalysisInstructionPatch {
                    section: SECTION,
                    section_offset: OFFSET as u64,
                    original_hex: "61a0fcff00000000",
                    prepared_hex: "61a0f8ff00000000",
                    requirement: "current PID namespace translation enabled; request field contains local TID",
                }),
            },
        })
    }

    /// Returns provenance without exposing or distributing the prepared artifact.
    #[must_use]
    pub fn report(&self) -> &PreparationReport {
        &self.report
    }

    /// Returns the bounded prepared ELF size without exposing its bytes.
    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.bytes.len()
    }

    #[cfg(any(test, all(feature = "native", target_os = "linux")))]
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

fn map_symbol(file: &object::File<'_>, name: &'static str) -> Result<u32, PreparationError> {
    for symbol in file.symbols() {
        if symbol.kind() == SymbolKind::Data && symbol.is_definition() && symbol.name()? == name {
            return u32::try_from(symbol.index().0).map_err(|_| PreparationError::Missing { name });
        }
    }
    Err(PreparationError::Missing { name })
}

fn collect_bindings(
    section: &'static str,
    code: &[u8],
    relocations: &[u8],
    file_offset: usize,
    symbols: &[(u32, u32); 2],
) -> Result<Vec<ProbeMapBinding>, PreparationError> {
    if !relocations.len().is_multiple_of(REL_SIZE) {
        return Err(PreparationError::Relocation {
            section,
            offset: relocations.len(),
        });
    }
    let mut counts = [0_usize; 2];
    let mut bindings = Vec::with_capacity(3);
    for (index, record) in relocations.chunks_exact(REL_SIZE).enumerate() {
        let offset = index * REL_SIZE;
        let mut word = [0_u8; 8];
        word.copy_from_slice(&record[8..]);
        let info = u64::from_le_bytes(word);
        let Some(binding_index) = symbols
            .iter()
            .position(|(source, _)| *source == (info >> 32) as u32)
        else {
            continue;
        };
        word.copy_from_slice(&record[..8]);
        let instruction_offset = u64::from_le_bytes(word);
        let instruction_start = usize::try_from(instruction_offset).ok();
        let instruction = instruction_start
            .and_then(|start| start.checked_add(16).and_then(|end| code.get(start..end)));
        if info as u32 != 1
            || !instruction_offset.is_multiple_of(8)
            || !instruction.is_some_and(|bytes| bytes[0] == 0x18 && bytes[8] == 0)
        {
            return Err(PreparationError::Relocation { section, offset });
        }
        counts[binding_index] += 1;
        if counts[binding_index] > EXPECTED_BINDINGS[binding_index] {
            return Err(PreparationError::BindingCount {
                section,
                map: MAP_NAMES[binding_index].0,
                expected: EXPECTED_BINDINGS[binding_index],
                actual: counts[binding_index],
            });
        }
        let relocation_info_offset = file_offset
            .checked_add(offset)
            .and_then(|offset| offset.checked_add(8))
            .ok_or(PreparationError::Relocation { section, offset })?;
        bindings.push(ProbeMapBinding {
            section,
            instruction_offset,
            relocation_info_offset,
            source_map: MAP_NAMES[binding_index].0,
            target_map: MAP_NAMES[binding_index].1,
            source_symbol: symbols[binding_index].0,
            target_symbol: symbols[binding_index].1,
        });
    }
    for (index, count) in counts.into_iter().enumerate() {
        if count != EXPECTED_BINDINGS[index] {
            return Err(PreparationError::BindingCount {
                section,
                map: MAP_NAMES[index].0,
                expected: EXPECTED_BINDINGS[index],
                actual: count,
            });
        }
    }
    Ok(bindings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, HashSet};

    /// Scenario: Both pinned objects are prepared for a thread-scoped temporary analysis request.
    /// Guarantees: Exactly one byte changes in the audited instruction; all other sections and sampling code remain intact.
    #[test]
    #[ignore = "requires UPSTREAM_EBPF_OBJECT and UPSTREAM_EBPF_ARM64_OBJECT; no privileges needed"]
    fn external_analysis_filter_is_thread_scoped() {
        for variable in ["UPSTREAM_EBPF_OBJECT", "UPSTREAM_EBPF_ARM64_OBJECT"] {
            let artifact =
                UpstreamArtifact::open(std::env::var_os(variable).expect("external artifact"))
                    .expect("artifact");
            let prepared =
                PreparedObject::thread_scoped_analysis(&artifact).expect("analysis preparation");
            assert_eq!(
                artifact
                    .bytes()
                    .iter()
                    .zip(prepared.bytes())
                    .filter(|(left, right)| left != right)
                    .count(),
                1
            );
            let original = object::File::parse(artifact.bytes()).expect("original ELF");
            let modified = object::File::parse(prepared.bytes()).expect("prepared ELF");
            for section in original.sections() {
                let name = section.name().expect("section");
                let after = modified
                    .section_by_name(name)
                    .expect("prepared section")
                    .data()
                    .expect("data");
                if name == "raw_tracepoint/sys_enter" {
                    assert_eq!(&after[0xd8..0xe0], &[0x61, 0xa0, 0xf8, 0xff, 0, 0, 0, 0]);
                } else {
                    assert_eq!(section.data().expect("original data"), after, "{name}");
                }
            }
            assert!(prepared.report().bindings.is_empty());
            assert!(prepared.report().instruction_patch.is_some());
        }
    }

    fn relocation_inputs() -> (Vec<u8>, Vec<u8>) {
        let mut code = vec![0_u8; 48];
        let mut records = Vec::new();
        for (index, source) in [1_u64, 1, 3].into_iter().enumerate() {
            code[index * 16] = 0x18;
            records.extend_from_slice(&((index * 16) as u64).to_le_bytes());
            records.extend_from_slice(&((source << 32) | 1).to_le_bytes());
        }
        (code, records)
    }

    /// Scenario: A native probe has exactly two scratch-map loads and one tail-array load.
    /// Guarantees: Preparation records only the three expected metadata substitutions and never mutates instructions.
    #[test]
    fn native_bindings_are_precise_and_bounded() {
        let (code, records) = relocation_inputs();
        let bindings = collect_bindings("probe", &code, &records, 100, &[(1, 2), (3, 4)])
            .expect("three bindings");
        assert_eq!(bindings.len(), 3);
        assert_eq!(bindings[0].relocation_info_offset, 108);
        assert_eq!(bindings[1].relocation_info_offset, 124);
        assert_eq!(bindings[2].target_symbol, 4);
        assert_eq!(bindings[2].instruction_offset, 32);
    }

    /// Scenario: ELF relocation lengths, offsets, or opcodes are malformed.
    /// Guarantees: Bad metadata receives a typed error without slicing outside either input buffer.
    #[test]
    fn malformed_relocations_are_rejected() {
        let (mut code, mut records) = relocation_inputs();
        assert!(matches!(
            collect_bindings("probe", &code, &records[..47], 0, &[(1, 2), (3, 4)]),
            Err(PreparationError::Relocation { .. }),
        ));
        records[..8].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(matches!(
            collect_bindings("probe", &code, &records, 0, &[(1, 2), (3, 4)]),
            Err(PreparationError::Relocation { .. }),
        ));
        records[..8].copy_from_slice(&0_u64.to_le_bytes());
        code[0] = 0;
        assert!(matches!(
            collect_bindings("probe", &code, &records, 0, &[(1, 2), (3, 4)]),
            Err(PreparationError::Relocation { .. }),
        ));
    }

    /// Scenario: A probe's relocation set omits its tail-call binding or duplicates a scratch-map binding.
    /// Guarantees: A changed upstream contract cannot silently produce a partial map rewrite.
    #[test]
    fn changed_binding_counts_are_rejected() {
        let (code, mut records) = relocation_inputs();
        assert!(matches!(
            collect_bindings("probe", &code, &records[..32], 0, &[(1, 2), (3, 4)]),
            Err(PreparationError::BindingCount {
                map: "perf_progs",
                actual: 0,
                ..
            }),
        ));
        records.extend_from_slice(&0_u64.to_le_bytes());
        records.extend_from_slice(&((1_u64 << 32) | 1).to_le_bytes());
        assert!(matches!(
            collect_bindings("probe", &code, &records, 0, &[(1, 2), (3, 4)]),
            Err(PreparationError::BindingCount { actual: 3, .. }),
        ));
    }

    fn relocate(bytes: &[u8]) -> (aya_obj::Object, BTreeMap<String, i32>) {
        let elf = object::File::parse(bytes).expect("ELF");
        let text: HashSet<_> = elf
            .sections()
            .filter(|section| section.name().expect("section name") == ".text")
            .map(|section| section.index().0)
            .collect();
        let mut parsed = aya_obj::Object::parse(bytes).expect("Aya object");
        let mut maps: Vec<_> = parsed
            .maps
            .iter()
            .map(|(name, map)| (name.clone(), map.clone()))
            .collect();
        maps.sort_by(|left, right| left.0.cmp(&right.0));
        let fds: BTreeMap<_, _> = maps
            .iter()
            .enumerate()
            .map(|(index, (name, _))| (name.clone(), 100 + index as i32))
            .collect();
        parsed
            .relocate_maps(
                maps.iter()
                    .map(|(name, map)| (name.as_str(), fds[name], map)),
                &text,
            )
            .expect("synthetic map relocation");
        parsed.relocate_calls(&text).expect("function linking");
        (parsed, fds)
    }

    /// Scenario: Both external pinned artifacts are prepared and linked with synthetic map descriptors.
    /// Guarantees: All 38 programs match the source-derived upstream map-rebinding oracle, with no kernel operation.
    #[test]
    #[ignore = "requires UPSTREAM_EBPF_OBJECT and UPSTREAM_EBPF_ARM64_OBJECT; no privileges needed"]
    fn external_probe_bindings_match_upstream_relocation() {
        for variable in ["UPSTREAM_EBPF_OBJECT", "UPSTREAM_EBPF_ARM64_OBJECT"] {
            let artifact =
                UpstreamArtifact::open(std::env::var_os(variable).expect("external artifact path"))
                    .expect("pinned artifact");
            let prepared = PreparedObject::native_probe_maps(&artifact).expect("prepared artifact");
            assert_eq!(prepared.report().bindings.len(), 6);
            assert_eq!(prepared.byte_len(), artifact.bytes().len());
            let before = object::File::parse(artifact.bytes()).expect("original ELF");
            let after = object::File::parse(prepared.bytes()).expect("prepared ELF");
            for section in before.sections() {
                let name = section.name().expect("section name");
                if SECTIONS.iter().any(|(_, relocations)| name == *relocations) {
                    continue;
                }
                assert_eq!(
                    section.data().expect("original section"),
                    after
                        .section_by_name(name)
                        .expect("prepared section")
                        .data()
                        .expect("prepared data"),
                    "unexpected modification of {name}",
                );
            }
            let (original, fds) = relocate(artifact.bytes());
            let (relocated, prepared_fds) = relocate(prepared.bytes());
            assert_eq!(fds, prepared_fds);
            assert_eq!(original.programs.len(), 38);
            for (name, program) in &original.programs {
                let mut expected = original.functions[&program.function_key()]
                    .instructions
                    .clone();
                if ["kprobe_unwind_native", "kprobe_unwind_stop"].contains(&name.as_str()) {
                    for instruction in &mut expected {
                        if instruction.code == 0x18 && instruction.src_reg() == 1 {
                            for (source, target) in MAP_NAMES {
                                if instruction.imm == fds[source] {
                                    instruction.imm = fds[target];
                                    break;
                                }
                            }
                        }
                    }
                }
                let actual = &relocated.functions[&program.function_key()].instructions;
                assert_eq!(expected.len(), actual.len());
                for (index, (left, right)) in expected.iter().zip(actual).enumerate() {
                    assert_eq!(
                        (
                            left.code,
                            left.dst_reg(),
                            left.src_reg(),
                            left.off,
                            left.imm
                        ),
                        (
                            right.code,
                            right.dst_reg(),
                            right.src_reg(),
                            right.off,
                            right.imm
                        ),
                        "{variable}: program {name}, instruction {index}",
                    );
                }
            }
        }
    }
}
