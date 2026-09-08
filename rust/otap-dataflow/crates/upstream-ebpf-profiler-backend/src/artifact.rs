// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! External artifact loading, fingerprinting, and compatibility validation.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use aya_obj::Object as AyaObject;
use object::{Object, ObjectSection, ObjectSymbol, SectionIndex, SymbolKind};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

use crate::error::{BackendError, CompatibilityError, CompatibilityErrorKind};
use crate::inventory::{
    ArtifactInventory, CompatibilityManifest, GlobalInventory, InnerMapInventory, MapInventory,
    ProgramInventory, map_type_name,
};

const MAX_ARTIFACT_BYTES: usize = 16 * 1024 * 1024;

/// A 32-byte SHA-256 digest serialized as lowercase hexadecimal.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Sha256Digest([u8; 32]);

impl Sha256Digest {
    /// All-zero digest useful in configuration construction tests.
    pub const ZERO: Self = Self([0; 32]);

    /// Hashes the supplied bytes.
    #[must_use]
    pub fn of_bytes(bytes: &[u8]) -> Self {
        let digest: [u8; 32] = Sha256::digest(bytes).into();
        Self(digest)
    }

    /// Returns the raw digest bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl FromStr for Sha256Digest {
    type Err = BackendError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 64 {
            return Err(BackendError::ArtifactParse(
                "SHA-256 must contain exactly 64 hexadecimal characters".to_owned(),
            ));
        }
        let bytes = hex::decode(value)
            .map_err(|error| BackendError::ArtifactParse(format!("invalid SHA-256: {error}")))?;
        let digest: [u8; 32] = bytes.try_into().map_err(|bytes: Vec<u8>| {
            BackendError::ArtifactParse(format!(
                "invalid SHA-256 length {}, expected 32 bytes",
                bytes.len()
            ))
        })?;
        Ok(Self(digest))
    }
}

impl fmt::Debug for Sha256Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&hex::encode(self.0))
    }
}

impl Serialize for Sha256Digest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Sha256Digest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

/// Architecture encoded by the architecture-specific upstream BTF.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactArchitecture {
    /// x86-64/amd64 kernel register ABI.
    Amd64,
    /// AArch64/arm64 kernel register ABI.
    Arm64,
    /// The BTF did not contain a recognized register signature.
    Unknown,
}

/// External upstream eBPF artifact plus its non-privileged inventory.
#[derive(Clone, Debug)]
pub struct UpstreamArtifact {
    path: PathBuf,
    bytes: Vec<u8>,
    inventory: ArtifactInventory,
}

impl UpstreamArtifact {
    /// Reads and inventories an external object without making a BPF syscall.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, BackendError> {
        let path = path.as_ref().to_path_buf();
        let bytes = crate::input::read_file(&path, MAX_ARTIFACT_BYTES)?;
        let inventory = inspect_bytes(&bytes)?;
        Ok(Self {
            path,
            bytes,
            inventory,
        })
    }

    /// Returns the external path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the deterministic non-privileged inventory.
    #[must_use]
    pub fn inventory(&self) -> &ArtifactInventory {
        &self.inventory
    }

    /// Returns the original bytes for checked object preparation and loading.
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Validates discovered artifacts against one checked-in manifest.
#[derive(Clone, Debug)]
pub struct CompatibilityValidator {
    manifest: CompatibilityManifest,
}

impl CompatibilityValidator {
    /// Creates a validator after checking manifest topology.
    pub fn new(manifest: CompatibilityManifest) -> Result<Self, CompatibilityError> {
        manifest.validate_topology()?;
        Ok(Self { manifest })
    }

    /// Returns the pinned manifest.
    #[must_use]
    pub fn manifest(&self) -> &CompatibilityManifest {
        &self.manifest
    }

    /// Validates the configured version and every discovered object.
    pub fn validate(
        &self,
        artifact: &UpstreamArtifact,
        expected_version: &str,
    ) -> Result<(), CompatibilityError> {
        if self.manifest.compatibility_version != expected_version {
            return Err(CompatibilityError::new(
                CompatibilityErrorKind::CompatibilityVersion,
                format!(
                    "configured {expected_version:?}, manifest {:?}",
                    self.manifest.compatibility_version
                ),
            ));
        }
        compare_inventory(&self.manifest.artifact, artifact.inventory())
    }
}

/// Inventories already-owned ELF bytes.
pub fn inspect_bytes(bytes: &[u8]) -> Result<ArtifactInventory, BackendError> {
    if bytes.len() > MAX_ARTIFACT_BYTES {
        return Err(BackendError::InputTooLarge {
            path: PathBuf::from("<artifact bytes>"),
            maximum: MAX_ARTIFACT_BYTES,
        });
    }
    let elf = object::File::parse(bytes)
        .map_err(|error| BackendError::ArtifactParse(format!("ELF parser: {error}")))?;
    if elf.format() != object::BinaryFormat::Elf {
        return Err(BackendError::ArtifactParse(
            "artifact is not ELF".to_owned(),
        ));
    }
    if elf.architecture() != object::Architecture::Bpf {
        return Err(BackendError::ArtifactParse(format!(
            "ELF machine is {:?}, expected BPF",
            elf.architecture()
        )));
    }
    if !elf.is_little_endian() || !elf.is_64() {
        return Err(BackendError::ArtifactParse(
            "the pinned ABI requires ELF64 little-endian BPF".to_owned(),
        ));
    }
    let aya = AyaObject::parse(bytes)
        .map_err(|error| BackendError::ArtifactParse(format!("Aya ELF parser: {error}")))?;

    let mut programs = Vec::with_capacity(aya.programs.len());
    for (name, program) in &aya.programs {
        let section = elf
            .section_by_index(SectionIndex(program.section_index))
            .map_err(|error| {
                BackendError::ArtifactParse(format!(
                    "program {name} section {}: {error}",
                    program.section_index
                ))
            })?;
        let section_name = section
            .name()
            .map_err(|error| {
                BackendError::ArtifactParse(format!("program {name} section name: {error}"))
            })?
            .to_owned();
        let instruction_count = aya
            .functions
            .get(&program.function_key())
            .ok_or_else(|| {
                BackendError::ArtifactParse(format!("program {name} function is absent"))
            })?
            .instructions
            .len();
        programs.push(ProgramInventory {
            name: name.clone(),
            section: section_name.clone(),
            program_type: program_type_name(&section_name).to_owned(),
            instruction_count,
        });
    }
    programs.sort();

    let mut maps = Vec::with_capacity(aya.maps.len());
    for (name, map) in &aya.maps {
        let inner = map.inner().map(|inner| InnerMapInventory {
            map_type: inner.map_type(),
            map_type_name: map_type_name(inner.map_type()).to_owned(),
            key_size: inner.key_size(),
            value_size: inner.value_size(),
            max_entries: inner.max_entries(),
            flags: inner.map_flags(),
        });
        maps.push(MapInventory {
            name: name.clone(),
            map_type: map.map_type(),
            map_type_name: map_type_name(map.map_type()).to_owned(),
            key_size: map.key_size(),
            value_size: map.value_size(),
            max_entries: map.max_entries(),
            flags: map.map_flags(),
            inner,
        });
    }
    maps.sort();

    let globals = inspect_globals(&elf)?;
    let architecture = detect_architecture(&elf)?;
    let license = aya
        .license
        .to_str()
        .map_err(|error| BackendError::ArtifactParse(format!("license is not UTF-8: {error}")))?
        .to_owned();

    Ok(ArtifactInventory {
        sha256: Sha256Digest::of_bytes(bytes),
        architecture,
        elf_machine: "bpf".to_owned(),
        license,
        programs,
        maps,
        globals,
    })
}

fn inspect_globals(elf: &object::File<'_>) -> Result<Vec<GlobalInventory>, BackendError> {
    let section = elf
        .section_by_name(".rodata.var")
        .ok_or_else(|| BackendError::ArtifactParse(".rodata.var is absent".to_owned()))?;
    let section_index = section.index();
    let data = section
        .data()
        .map_err(|error| BackendError::ArtifactParse(format!(".rodata.var data: {error}")))?;
    let mut globals = Vec::new();
    for symbol in elf.symbols() {
        if symbol.section_index() != Some(section_index)
            || symbol.kind() != SymbolKind::Data
            || !symbol.is_definition()
            || symbol.size() == 0
        {
            continue;
        }
        let name = symbol
            .name()
            .map_err(|error| BackendError::ArtifactParse(format!("global name: {error}")))?;
        let offset = symbol.address() as usize;
        let size = symbol.size() as usize;
        let end = offset
            .checked_add(size)
            .ok_or_else(|| BackendError::ArtifactParse(format!("global {name} range overflow")))?;
        let default = data.get(offset..end).ok_or_else(|| {
            BackendError::ArtifactParse(format!(
                "global {name} range {offset}..{end} exceeds .rodata.var"
            ))
        })?;
        globals.push(GlobalInventory {
            name: name.to_owned(),
            offset: symbol.address(),
            size: symbol.size(),
            default_hex: hex::encode(default),
        });
    }
    globals.sort();
    Ok(globals)
}

fn detect_architecture(elf: &object::File<'_>) -> Result<ArtifactArchitecture, BackendError> {
    let btf = elf
        .section_by_name(".BTF")
        .ok_or_else(|| BackendError::ArtifactParse(".BTF is absent".to_owned()))?
        .data()
        .map_err(|error| BackendError::ArtifactParse(format!(".BTF data: {error}")))?;
    let amd64 = contains_bytes(btf, b"orig_ax\0") && contains_bytes(btf, b"r15\0");
    let arm64 = contains_bytes(btf, b"orig_x0\0") && contains_bytes(btf, b"pstate\0");
    Ok(match (amd64, arm64) {
        (true, false) => ArtifactArchitecture::Amd64,
        (false, true) => ArtifactArchitecture::Arm64,
        _ => ArtifactArchitecture::Unknown,
    })
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn program_type_name(section: &str) -> &'static str {
    match section.split('/').next().unwrap_or(section) {
        "perf_event" => "perf_event",
        "kprobe" => "kprobe",
        "tracepoint" => "tracepoint",
        "raw_tracepoint" | "raw_tp" => "raw_tracepoint",
        _ => "unknown",
    }
}

fn compare_inventory(
    expected: &ArtifactInventory,
    actual: &ArtifactInventory,
) -> Result<(), CompatibilityError> {
    if expected.sha256 != actual.sha256 {
        return Err(CompatibilityError::new(
            CompatibilityErrorKind::ArtifactHash,
            format!("expected {}, discovered {}", expected.sha256, actual.sha256),
        ));
    }
    if expected.architecture != actual.architecture {
        return Err(CompatibilityError::new(
            CompatibilityErrorKind::Architecture,
            format!(
                "expected {:?}, discovered {:?}",
                expected.architecture, actual.architecture
            ),
        ));
    }
    compare_named(
        &expected.programs,
        &actual.programs,
        |program| &program.name,
        CompatibilityErrorKind::MissingProgram,
        CompatibilityErrorKind::ChangedProgramAbi,
        "program",
    )?;
    compare_named(
        &expected.maps,
        &actual.maps,
        |map| &map.name,
        CompatibilityErrorKind::MissingMap,
        CompatibilityErrorKind::ChangedMapAbi,
        "map",
    )?;
    compare_named(
        &expected.globals,
        &actual.globals,
        |global| &global.name,
        CompatibilityErrorKind::MissingGlobal,
        CompatibilityErrorKind::ChangedGlobalAbi,
        "global",
    )?;
    if expected.license != actual.license {
        return Err(CompatibilityError::new(
            CompatibilityErrorKind::ArtifactHash,
            format!(
                "expected license {:?}, discovered {:?}",
                expected.license, actual.license
            ),
        ));
    }
    Ok(())
}

fn compare_named<T, F>(
    expected: &[T],
    actual: &[T],
    name: F,
    missing_kind: CompatibilityErrorKind,
    changed_kind: CompatibilityErrorKind,
    label: &str,
) -> Result<(), CompatibilityError>
where
    T: Eq + fmt::Debug,
    F: Fn(&T) -> &String,
{
    for item in expected {
        let item_name = name(item);
        let Some(discovered) = actual.iter().find(|candidate| name(candidate) == item_name) else {
            return Err(CompatibilityError::new(
                missing_kind,
                format!("{label} {item_name} is absent"),
            ));
        };
        if item != discovered {
            return Err(CompatibilityError::new(
                changed_kind,
                format!(
                    "{label} {item_name} changed: expected {item:?}, discovered {discovered:?}"
                ),
            ));
        }
    }
    if expected.len() != actual.len() {
        return Err(CompatibilityError::new(
            changed_kind,
            format!(
                "{label} count changed: expected {}, discovered {}",
                expected.len(),
                actual.len()
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::pinned_tail_calls;

    fn inventory() -> ArtifactInventory {
        ArtifactInventory {
            sha256: Sha256Digest::ZERO,
            architecture: ArtifactArchitecture::Amd64,
            elf_machine: "bpf".to_owned(),
            license: "GPL".to_owned(),
            programs: vec![ProgramInventory {
                name: "perf_unwind_stop".to_owned(),
                section: "perf_event/unwind_stop".to_owned(),
                program_type: "perf_event".to_owned(),
                instruction_count: 1,
            }],
            maps: Vec::new(),
            globals: Vec::new(),
        }
    }

    /// Scenario: Configuration presents an artifact digest other than the
    /// checked-in digest.
    /// Guarantees: Compatibility fails with the stable hash category before
    /// any kernel operation.
    #[test]
    fn unexpected_hash_has_typed_category() {
        let expected = inventory();
        let mut actual = expected.clone();
        actual.sha256 = Sha256Digest::of_bytes(b"different");
        let error = compare_inventory(&expected, &actual).expect_err("hash mismatch");
        assert_eq!(error.kind, CompatibilityErrorKind::ArtifactHash);
    }

    /// Scenario: An expected map remains named but changes its value size.
    /// Guarantees: The mismatch is classified as changed map ABI rather than a
    /// string-only loader failure.
    #[test]
    fn changed_map_abi_has_typed_category() {
        let mut expected = inventory();
        expected.maps.push(MapInventory {
            name: "events".to_owned(),
            map_type: 1,
            map_type_name: "hash".to_owned(),
            key_size: 4,
            value_size: 8,
            max_entries: 1,
            flags: 0,
            inner: None,
        });
        let mut actual = expected.clone();
        actual.maps[0].value_size = 16;
        let error = compare_inventory(&expected, &actual).expect_err("map mismatch");
        assert_eq!(error.kind, CompatibilityErrorKind::ChangedMapAbi);
    }

    /// Scenario: A manifest tail-call entry references a missing probe program.
    /// Guarantees: Manifest validation rejects incomplete topology before it is
    /// trusted for program-array setup.
    #[test]
    fn manifest_rejects_missing_tail_call_program() {
        let manifest = CompatibilityManifest {
            schema_version: 1,
            compatibility_version: "test".to_owned(),
            upstream_commit: crate::PINNED_UPSTREAM_COMMIT.to_owned(),
            artifact: inventory(),
            tail_calls: pinned_tail_calls(),
        };
        assert!(matches!(
            manifest.validate_topology(),
            Err(CompatibilityError {
                kind: CompatibilityErrorKind::TailCallTopology,
                ..
            })
        ));
    }

    /// Scenario: A pinned inventory loses a program or map, or changes an inner-map or global ABI.
    /// Guarantees: Each drift category is reported without loading anything into the kernel.
    #[test]
    fn inventory_drift_has_precise_categories() {
        let expected = CompatibilityManifest::from_json(crate::AMD64_COMPATIBILITY_MANIFEST)
            .expect("manifest")
            .artifact;
        let mut actual = expected.clone();
        let _removed = actual.programs.pop();
        assert_eq!(
            compare_inventory(&expected, &actual)
                .expect_err("missing program")
                .kind,
            CompatibilityErrorKind::MissingProgram
        );
        let mut actual = expected.clone();
        let _removed = actual.maps.pop();
        assert_eq!(
            compare_inventory(&expected, &actual)
                .expect_err("missing map")
                .kind,
            CompatibilityErrorKind::MissingMap
        );
        let mut actual = expected.clone();
        actual
            .maps
            .iter_mut()
            .find_map(|map| map.inner.as_mut())
            .expect("inner map")
            .value_size = 8;
        assert_eq!(
            compare_inventory(&expected, &actual)
                .expect_err("inner ABI")
                .kind,
            CompatibilityErrorKind::ChangedMapAbi
        );
        let mut actual = expected.clone();
        actual.globals[0].offset += 1;
        assert_eq!(
            compare_inventory(&expected, &actual)
                .expect_err("global offset")
                .kind,
            CompatibilityErrorKind::ChangedGlobalAbi
        );
        let mut actual = expected.clone();
        actual.programs[0].program_type = "unknown".to_owned();
        assert_eq!(
            compare_inventory(&expected, &actual)
                .expect_err("program type")
                .kind,
            CompatibilityErrorKind::ChangedProgramAbi
        );
    }
}
