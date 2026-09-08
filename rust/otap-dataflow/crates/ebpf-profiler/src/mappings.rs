// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Executable mapping parsing, normalization, and bounded lookup.

use std::path::Path;

use crate::{ProfilerError, Result};

/// Classification of a procfs virtual-memory mapping.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MappingKind {
    /// File-backed mapping.
    File,
    /// File-backed mapping whose pathname is marked deleted.
    DeletedFile,
    /// Anonymous mapping without a pathname.
    Anonymous,
    /// Kernel or runtime special mapping such as `[vdso]`.
    Special,
}

/// Device/inode identity for a file-backed executable mapping.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct MappingFileIdentity {
    /// Procfs device major number.
    pub device_major: u32,
    /// Procfs device minor number.
    pub device_minor: u32,
    /// Procfs inode number.
    pub inode: u64,
}

impl MappingFileIdentity {
    /// Checks metadata from the actual file handle used for symbol lookup.
    #[must_use]
    pub fn matches(&self, metadata: &std::fs::Metadata) -> bool {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::MetadataExt;
            metadata.is_file()
                && self.inode != 0
                && metadata.ino() == self.inode
                && nix::libc::major(metadata.dev()) == self.device_major
                && nix::libc::minor(metadata.dev()) == self.device_minor
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _metadata = metadata;
            false
        }
    }
}

/// One executable mapping from `/proc/<pid>/maps`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutableMapping {
    /// Inclusive virtual-address start.
    pub start: u64,
    /// Exclusive virtual-address end.
    pub end: u64,
    /// File offset corresponding to `start`.
    pub file_offset: u64,
    /// Path or special mapping label.
    pub path: Option<String>,
    /// Mapping classification.
    pub kind: MappingKind,
    /// Major device number reported by procfs.
    pub device_major: u32,
    /// Minor device number reported by procfs.
    pub device_minor: u32,
    /// File inode reported by procfs, or zero for non-file mappings.
    pub inode: u64,
    /// Whether the display pathname was shortened and must not be opened.
    pub path_truncated: bool,
}

impl ExecutableMapping {
    /// Returns whether the address falls inside this mapping.
    #[must_use]
    pub fn contains(&self, address: u64) -> bool {
        self.start <= address && address < self.end
    }

    /// Normalizes a virtual address to a file-relative address.
    #[must_use]
    pub fn normalize(&self, address: u64) -> Option<u64> {
        if !self.contains(address) {
            return None;
        }
        address
            .checked_sub(self.start)
            .and_then(|relative| relative.checked_add(self.file_offset))
    }

    /// Returns a filesystem path suitable for object-file symbolization.
    #[must_use]
    pub fn symbol_path(&self) -> Option<&Path> {
        if self.kind != MappingKind::File || self.path_truncated || self.inode == 0 {
            return None;
        }
        let path = self.path.as_deref()?;
        // Procfs escapes newlines ambiguously. Do not open a potentially
        // different object when the kernel pathname cannot be recovered.
        if path.contains(['\\', '\0']) || !Path::new(path).is_absolute() {
            return None;
        }
        Some(Path::new(path))
    }

    /// Checks the backing file against the device and inode recorded by procfs.
    /// Deleted, anonymous, and special mappings cannot pass this check.
    #[must_use]
    pub fn matches_file_identity(&self, metadata: &std::fs::Metadata) -> bool {
        self.symbol_path().is_some() && self.file_identity().matches(metadata)
    }

    /// Returns the recorded numeric backing-file identity.
    #[must_use]
    pub fn file_identity(&self) -> MappingFileIdentity {
        MappingFileIdentity {
            device_major: self.device_major,
            device_minor: self.device_minor,
            inode: self.inode,
        }
    }
}

/// Sorted bounded executable mapping table.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MappingTable {
    mappings: Vec<ExecutableMapping>,
    truncated: bool,
}

impl MappingTable {
    /// Creates a sorted table and rejects overlapping executable ranges.
    pub fn new(mut mappings: Vec<ExecutableMapping>, max_mappings: usize) -> Result<Self> {
        if mappings.len() > max_mappings {
            return Err(ProfilerError::ProcessMetadata {
                pid: 0,
                reason: format!(
                    "{} executable mappings exceeds limit {max_mappings}",
                    mappings.len()
                ),
            });
        }
        for mapping in &mappings {
            if mapping.end <= mapping.start {
                return Err(maps_error("empty or reversed executable range"));
            }
            if mapping
                .file_offset
                .checked_add(mapping.end - mapping.start - 1)
                .is_none()
            {
                return Err(maps_error("executable file offset overflows"));
            }
        }
        mappings.sort_unstable_by_key(|mapping| mapping.start);
        for pair in mappings.windows(2) {
            if pair[0].end > pair[1].start {
                return Err(ProfilerError::ProcessMetadata {
                    pid: 0,
                    reason: "overlapping executable mappings".to_owned(),
                });
            }
        }
        Ok(Self {
            mappings,
            truncated: false,
        })
    }

    /// Returns the sorted mappings.
    #[must_use]
    pub fn as_slice(&self) -> &[ExecutableMapping] {
        &self.mappings
    }

    /// Whether input or mapping cardinality limits omitted executable ranges.
    #[must_use]
    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// Whether any retained display pathname was shortened.
    #[must_use]
    pub fn strings_truncated(&self) -> bool {
        self.mappings.iter().any(|mapping| mapping.path_truncated)
    }

    pub(crate) fn mark_input_truncated(&mut self) {
        self.truncated = true;
    }

    /// Resolves an address with logarithmic lookup.
    #[must_use]
    pub fn find(&self, address: u64) -> Option<&ExecutableMapping> {
        let index = self
            .mappings
            .partition_point(|mapping| mapping.start <= address);
        index
            .checked_sub(1)
            .and_then(|index| self.mappings.get(index))
            .filter(|mapping| mapping.contains(address))
    }
}

/// Parses executable mappings from Linux procfs maps text.
pub fn parse_proc_maps(
    input: &str,
    max_mappings: usize,
    max_path_bytes: usize,
) -> Result<MappingTable> {
    let mut mappings = Vec::new();
    let mut truncated = false;
    for line in input.lines() {
        let mut remaining = line;
        let range = next_field(&mut remaining)?;
        let permissions = next_field(&mut remaining)?.as_bytes();
        let offset = next_field(&mut remaining)?;
        let device = next_field(&mut remaining)?;
        let inode = next_field(&mut remaining)?;
        let path = remaining.trim_start_matches(char::is_whitespace);
        let path = (!path.is_empty()).then_some(path);
        if permissions.len() != 4
            || !matches!(permissions[0], b'r' | b'-')
            || !matches!(permissions[1], b'w' | b'-')
            || !matches!(permissions[2], b'x' | b'-')
            || !matches!(permissions[3], b'p' | b's')
        {
            return Err(maps_error("invalid maps permissions"));
        }
        let (start, end) = range
            .split_once('-')
            .ok_or_else(|| maps_error("malformed maps range"))?;
        let start = u64::from_str_radix(start, 16).map_err(|_| maps_error("invalid maps start"))?;
        let end = u64::from_str_radix(end, 16).map_err(|_| maps_error("invalid maps end"))?;
        if end <= start {
            return Err(maps_error("empty or reversed maps range"));
        }
        let file_offset =
            u64::from_str_radix(offset, 16).map_err(|_| maps_error("invalid maps offset"))?;
        let (major, minor) = device
            .split_once(':')
            .ok_or_else(|| maps_error("invalid maps device"))?;
        let device_major =
            u32::from_str_radix(major, 16).map_err(|_| maps_error("invalid maps device"))?;
        let device_minor =
            u32::from_str_radix(minor, 16).map_err(|_| maps_error("invalid maps device"))?;
        let inode = inode
            .parse()
            .map_err(|_| maps_error("invalid maps inode"))?;
        if permissions[2] != b'x' {
            continue;
        }
        if mappings.len() == max_mappings {
            truncated = true;
            break;
        }
        let (path, kind, path_truncated) = classify_path(path, max_path_bytes);
        mappings.push(ExecutableMapping {
            start,
            end,
            file_offset,
            path,
            kind,
            device_major,
            device_minor,
            inode,
            path_truncated,
        });
    }
    let mut table = MappingTable::new(mappings, max_mappings)?;
    table.truncated = truncated;
    Ok(table)
}

fn maps_error(reason: &'static str) -> ProfilerError {
    ProfilerError::ProcessMetadata {
        pid: 0,
        reason: reason.to_owned(),
    }
}

fn next_field<'a>(remaining: &mut &'a str) -> Result<&'a str> {
    *remaining = remaining.trim_start_matches(char::is_whitespace);
    if remaining.is_empty() {
        return Err(maps_error("missing maps field"));
    }
    let end = remaining
        .find(char::is_whitespace)
        .unwrap_or(remaining.len());
    let field = &remaining[..end];
    *remaining = &remaining[end..];
    Ok(field)
}

fn classify_path(path: Option<&str>, max_path_bytes: usize) -> (Option<String>, MappingKind, bool) {
    let Some(path) = path else {
        return (None, MappingKind::Anonymous, false);
    };
    if path.starts_with('[') && path.ends_with(']') {
        return (
            Some(truncate_utf8(path, max_path_bytes)),
            MappingKind::Special,
            path.len() > max_path_bytes,
        );
    }
    if let Some(path) = path.strip_suffix(" (deleted)") {
        return (
            Some(truncate_utf8(path, max_path_bytes)),
            MappingKind::DeletedFile,
            path.len() > max_path_bytes,
        );
    }
    (
        Some(truncate_utf8(path, max_path_bytes)),
        MappingKind::File,
        path.len() > max_path_bytes,
    )
}

pub(crate) fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: Procfs contains file-backed, anonymous, deleted, and special
    /// executable mappings.
    /// Guarantees: Mapping classification and address normalization remain
    /// deterministic.
    #[test]
    fn parses_and_classifies_executable_mappings() {
        let maps = "\
1000-2000 r-xp 00001000 00:00 1 /bin/workload\n\
2000-3000 rw-p 00002000 00:00 1 /bin/workload\n\
3000-4000 r-xp 00000000 00:00 0\n\
4000-5000 r-xp 00000000 00:00 2 /tmp/gone (deleted)\n\
5000-6000 r-xp 00000000 00:00 0 [vdso]\n";
        let table = parse_proc_maps(maps, 8, 1024).expect("maps should parse");
        assert_eq!(table.as_slice().len(), 4);
        assert_eq!(
            table.find(0x1800).and_then(|m| m.normalize(0x1800)),
            Some(0x1800)
        );
        assert_eq!(table.as_slice()[1].kind, MappingKind::Anonymous);
        assert_eq!(table.as_slice()[2].kind, MappingKind::DeletedFile);
        assert_eq!(table.as_slice()[3].kind, MappingKind::Special);
    }

    /// Scenario: Two executable ranges overlap in malformed procfs input.
    /// Guarantees: Binary-search mapping lookup never observes ambiguous ranges.
    #[test]
    fn overlapping_mappings_are_rejected() {
        let maps = "\
1000-3000 r-xp 00000000 00:00 1 /a\n\
2000-4000 r-xp 00000000 00:00 2 /b\n";
        assert!(parse_proc_maps(maps, 8, 1024).is_err());
    }

    /// Scenario: Host mapping cardinality exceeds the configured capacity.
    /// Guarantees: Mapping discovery stops rather than growing without bound.
    #[test]
    fn mapping_capacity_is_enforced() {
        let maps = "\
1000-2000 r-xp 00000000 00:00 1 /a\n\
2000-3000 r-xp 00000000 00:00 2 /b\n";
        let table = parse_proc_maps(maps, 1, 1024).expect("bounded subset");
        assert_eq!(table.as_slice().len(), 1);
        assert!(table.is_truncated());
    }

    /// Scenario: Procfs aligns columns with spaces and tabs and paths contain spaces.
    /// Guarantees: Tokenization skips field padding without splitting pathnames.
    #[test]
    fn aligned_fields_preserve_path_and_identity() {
        let table = parse_proc_maps(
            "1000-2000  r-xp\t00000100  08:10   321     /bin/with spaces \n",
            4,
            128,
        )
        .expect("aligned maps");
        let mapping = &table.as_slice()[0];
        assert_eq!(mapping.path.as_deref(), Some("/bin/with spaces "));
        assert_eq!(
            (mapping.device_major, mapping.device_minor, mapping.inode),
            (8, 16, 321)
        );
        assert_eq!(mapping.normalize(0x1fff), Some(0x10ff));
        assert!(!table.is_truncated());
    }

    /// Scenario: Executable maps have missing fields, invalid flags, or invalid arithmetic.
    /// Guarantees: Invalid executable metadata is rejected instead of silently ignored.
    #[test]
    fn malformed_executable_maps_are_rejected() {
        for maps in [
            "1000-2000 r-xp",
            "1000-2000 rx 0 08:01 1 /a",
            "1000-1000 r-xp 0 08:01 1 /a",
            "2000-1000 r-xp 0 08:01 1 /a",
            "1000-2000 r-xp ffffffffffffffff 08:01 1 /a",
            "1000-2000 r-xp 0 bad 1 /a",
            "1000-2000 r-xp 0 08:01 bad /a",
        ] {
            assert!(parse_proc_maps(maps, 4, 128).is_err(), "{maps}");
        }
    }

    /// Scenario: A pathname exceeds its display bound or cannot safely name a live file.
    /// Guarantees: Truncated, escaped, deleted, and anonymous paths cannot be opened.
    #[test]
    fn unsafe_paths_are_never_symbol_paths() {
        let table = parse_proc_maps(
            "1000-2000 r-xp 0 08:01 1 /bin/very-long-name\n\
             2000-3000 r-xp 0 08:01 2 /gone (deleted)\n\
             3000-4000 r-xp 0 00:00 0 [jit]\n\
             4000-5000 r-xp 0 00:00 0\n",
            4,
            4,
        )
        .expect("maps");
        assert!(table.strings_truncated());
        assert!(
            table
                .as_slice()
                .iter()
                .all(|mapping| mapping.symbol_path().is_none())
        );
        let escaped = parse_proc_maps("1000-2000 r-xp 0 08:01 1 /a\\012b", 1, 128)
            .expect("escaped display path");
        assert!(escaped.as_slice()[0].symbol_path().is_none());
    }

    /// Scenario: No mapping capacity is available, or a caller constructs an invalid range.
    /// Guarantees: Capacity exhaustion is explicit and unchecked ranges cannot enter a table.
    #[test]
    fn zero_capacity_and_manual_empty_ranges() {
        let maps = "1000-2000 r-xp 0 08:01 1 /a";
        let table = parse_proc_maps(maps, 0, 128).expect("empty bounded subset");
        assert!(table.is_truncated());
        assert!(table.as_slice().is_empty());
        let mut mapping = parse_proc_maps(maps, 1, 128).expect("maps").as_slice()[0].clone();
        mapping.end = mapping.start;
        assert!(MappingTable::new(vec![mapping.clone()], 1).is_err());
        mapping.end = mapping.start + 2;
        mapping.file_offset = u64::MAX;
        assert_eq!(mapping.normalize(mapping.start + 1), None);
        assert!(MappingTable::new(vec![mapping], 1).is_err());
    }

    /// Scenario: A live file has the same pathname but a different recorded device or inode.
    /// Guarantees: Backing-object validation rejects replacement rather than trusting a pathname.
    #[cfg(target_os = "linux")]
    #[test]
    fn backing_file_identity_checks_device_and_inode() {
        use std::os::unix::fs::MetadataExt;
        let directory = tempfile::Builder::new()
            .prefix("mapping-")
            .tempdir_in(".")
            .expect("project fixture directory");
        let path = directory.path().join("object");
        std::fs::write(&path, b"fixture").expect("fixture");
        let metadata = std::fs::metadata(&path).expect("file identity");
        let input = format!(
            "1000-2000 r-xp 0 {:x}:{:x} {} /object",
            nix::libc::major(metadata.dev()),
            nix::libc::minor(metadata.dev()),
            metadata.ino(),
        );
        let mut mapping = parse_proc_maps(&input, 1, 128).expect("maps").as_slice()[0].clone();
        assert!(mapping.matches_file_identity(&metadata));
        mapping.inode ^= 1;
        assert!(!mapping.matches_file_identity(&metadata));
        mapping.inode = metadata.ino();
        mapping.device_minor ^= 1;
        assert!(!mapping.matches_file_identity(&metadata));
        mapping.device_minor = nix::libc::minor(metadata.dev());
        mapping.path_truncated = true;
        assert!(!mapping.matches_file_identity(&metadata));
    }
}
