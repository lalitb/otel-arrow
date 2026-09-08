// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Bounded native object-file symbol lookup.

use std::{
    collections::{HashMap, VecDeque},
    fs::Metadata,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

use object::{Object, ObjectSection, ObjectSymbol, SectionFlags, SectionKind, SymbolKind};

use crate::{
    DropReason, MappingFileIdentity, ProfilerError, Result,
    process::{open_regular_file, read_file_prefix, validate_read_limit},
};

/// Native symbol information for one normalized instruction address.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedSymbol {
    /// Function or symbol name.
    pub name: String,
    /// Symbol-relative instruction offset.
    pub offset: u64,
}

/// Fixed cache counters and current occupancy, without pathname labels.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SymbolStatistics {
    /// Retained function ranges across all files.
    pub cache_entries: usize,
    /// Retained file indexes, including negative entries.
    pub cache_files: usize,
    /// Lookups served without rereading an object.
    pub cache_hits: u64,
    /// File indexes removed for capacity or replacement.
    pub evictions: u64,
    /// File, string, or symbol-entry bounds that omitted metadata.
    pub capacity_rejections: u64,
}

/// Injectable symbol lookup boundary.
pub trait SymbolResolver: Send {
    /// Resolves one file-relative instruction address.
    fn resolve(&mut self, path: &Path, file_offset: u64) -> Result<Option<ResolvedSymbol>>;

    /// Resolves a mapped object with its observed device/inode identity.
    /// File-reading implementations must validate the object actually read
    /// against `expected`; external/injected resolvers can use their own
    /// authoritative symbol store via the default implementation.
    fn resolve_backing(
        &mut self,
        path: &Path,
        file_offset: u64,
        _expected: MappingFileIdentity,
    ) -> Result<Option<ResolvedSymbol>> {
        self.resolve(path, file_offset)
    }

    /// Returns fixed cache counters, or empty counters for injected resolvers.
    fn statistics(&self) -> SymbolStatistics {
        SymbolStatistics::default()
    }
}

#[derive(Clone, Debug)]
struct SymbolRange {
    file_offset: u64,
    size: u64,
    name: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SymbolFailure {
    Unavailable,
    NotRegular,
    Read,
    TooLarge,
    Malformed,
    Changed,
    WrongBacking,
    Capacity,
    Allocation,
}

impl SymbolFailure {
    fn error(self) -> ProfilerError {
        if self == Self::Allocation {
            return ProfilerError::Allocation("symbol index");
        }
        if matches!(self, Self::Capacity | Self::TooLarge) {
            return ProfilerError::Capacity(DropReason::SymbolCapacity);
        }
        ProfilerError::ProcessMetadata {
            pid: 0,
            reason: match self {
                Self::Unavailable => "symbol object unavailable",
                Self::NotRegular => "symbol object is not a regular file",
                Self::Read => "symbol object read failed",
                Self::Malformed => "symbol object or symbol table is malformed",
                Self::Changed => "symbol object changed during lookup",
                Self::WrongBacking => "symbol object does not match the executable mapping",
                Self::TooLarge | Self::Capacity | Self::Allocation => unreachable!(),
            }
            .to_owned(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FileIdentity {
    size: u64,
    modified: SystemTime,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    change_seconds: i64,
    #[cfg(unix)]
    change_nanoseconds: i64,
}

impl FileIdentity {
    fn from_metadata(metadata: &Metadata) -> std::result::Result<Self, SymbolFailure> {
        if !metadata.is_file() {
            return Err(SymbolFailure::NotRegular);
        }
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Ok(Self {
            size: metadata.len(),
            modified: metadata
                .modified()
                .map_err(|_| SymbolFailure::Unavailable)?,
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
            #[cfg(unix)]
            change_seconds: metadata.ctime(),
            #[cfg(unix)]
            change_nanoseconds: metadata.ctime_nsec(),
        })
    }
}

#[derive(Debug)]
struct CachedFile {
    requested: Option<MappingFileIdentity>,
    identity: Option<FileIdentity>,
    ranges: Vec<SymbolRange>,
    incomplete: Option<SymbolFailure>,
    checked_at: Instant,
}

/// Native resolver with independently bounded file, function, byte, and string
/// counts. Successful indexes validate file identity on each lookup. Negative
/// indexes retry after one second, avoiding repeated reads of inaccessible or
/// malformed files. Eviction is FIFO and includes negative entries.
#[derive(Debug)]
pub struct ObjectSymbolResolver {
    max_entries: usize,
    max_files: usize,
    max_file_bytes: usize,
    max_string_bytes: usize,
    total_entries: usize,
    cache: HashMap<PathBuf, CachedFile>,
    insertion_order: VecDeque<PathBuf>,
    statistics: SymbolStatistics,
}

impl ObjectSymbolResolver {
    /// Creates a resolver with a default 256-file and 4096-byte string bound.
    /// Invalid zero or overflowing bounds produce typed errors on lookup.
    #[must_use]
    pub fn new(max_entries: usize, max_file_bytes: usize) -> Self {
        Self::unchecked_limits(max_entries, max_entries.min(256), max_file_bytes, 4096)
    }

    /// Creates a resolver with independently validated resource limits.
    pub fn with_limits(
        max_entries: usize,
        max_files: usize,
        max_file_bytes: usize,
        max_string_bytes: usize,
    ) -> Result<Self> {
        let resolver =
            Self::unchecked_limits(max_entries, max_files, max_file_bytes, max_string_bytes);
        resolver.validate()?;
        Ok(resolver)
    }

    fn unchecked_limits(
        max_entries: usize,
        max_files: usize,
        max_file_bytes: usize,
        max_string_bytes: usize,
    ) -> Self {
        Self {
            max_entries,
            max_files,
            max_file_bytes,
            max_string_bytes,
            total_entries: 0,
            cache: HashMap::new(),
            insertion_order: VecDeque::new(),
            statistics: SymbolStatistics::default(),
        }
    }

    fn validate(&self) -> Result<()> {
        validate_read_limit(self.max_file_bytes)?;
        if self.max_entries == 0 || self.max_files == 0 || self.max_string_bytes == 0 {
            return Err(ProfilerError::invalid(
                "symbol_cache",
                "cache bounds must be positive",
            ));
        }
        Ok(())
    }

    fn remove(&mut self, path: &Path) {
        if let Some(removed) = self.cache.remove(path) {
            self.total_entries -= removed.ranges.len();
            self.insertion_order.retain(|entry| entry != path);
            self.statistics.evictions = self.statistics.evictions.saturating_add(1);
        }
    }

    fn evict_oldest(&mut self) -> bool {
        let Some(path) = self.insertion_order.front().cloned() else {
            return false;
        };
        self.remove(&path);
        true
    }

    fn reject_capacity(&mut self) {
        self.statistics.capacity_rejections = self.statistics.capacity_rejections.saturating_add(1);
    }

    fn load(&mut self, path: &Path, expected: Option<MappingFileIdentity>) -> Result<()> {
        self.validate()?;
        if path.as_os_str().len() > self.max_string_bytes {
            self.reject_capacity();
            return Err(SymbolFailure::Capacity.error());
        }
        if self.cache.get(path).is_some_and(|entry| {
            entry.requested == expected
                && entry.ranges.is_empty()
                && entry.incomplete.is_some()
                && entry.checked_at.elapsed() < Duration::from_secs(1)
        }) {
            self.statistics.cache_hits = self.statistics.cache_hits.saturating_add(1);
            return Ok(());
        }
        let identity = std::fs::metadata(path)
            .map_err(|_| SymbolFailure::Unavailable)
            .and_then(|metadata| {
                if expected.is_some_and(|expected| !expected.matches(&metadata)) {
                    Err(SymbolFailure::WrongBacking)
                } else {
                    FileIdentity::from_metadata(&metadata)
                }
            });
        if let (Some(entry), Ok(identity)) = (self.cache.get_mut(path), &identity)
            && entry.identity.as_ref() == Some(identity)
            && (entry.incomplete.is_none()
                || !entry.ranges.is_empty()
                || matches!(
                    entry.incomplete,
                    Some(
                        SymbolFailure::Malformed
                            | SymbolFailure::TooLarge
                            | SymbolFailure::Capacity
                    )
                ))
        {
            entry.checked_at = Instant::now();
            self.statistics.cache_hits = self.statistics.cache_hits.saturating_add(1);
            return Ok(());
        }
        self.remove(path);
        while self.cache.len() >= self.max_files {
            if !self.evict_oldest() {
                return Err(ProfilerError::InternalInvariant(
                    "symbol file capacity".to_owned(),
                ));
            }
        }
        let mut entry = CachedFile {
            requested: expected,
            identity: identity.as_ref().ok().cloned(),
            ranges: Vec::new(),
            incomplete: None,
            checked_at: Instant::now(),
        };
        match identity {
            Ok(identity) => match self.read_index(path, &identity) {
                Ok((ranges, incomplete)) => {
                    entry.ranges = ranges;
                    entry.incomplete = incomplete;
                }
                Err(failure) => entry.incomplete = Some(failure),
            },
            Err(failure) => entry.incomplete = Some(failure),
        }
        if matches!(
            entry.incomplete,
            Some(SymbolFailure::Capacity | SymbolFailure::TooLarge)
        ) {
            self.reject_capacity();
        }
        self.cache
            .try_reserve(1)
            .map_err(|_| ProfilerError::Allocation("symbol file cache"))?;
        self.insertion_order
            .try_reserve(1)
            .map_err(|_| ProfilerError::Allocation("symbol eviction queue"))?;
        self.total_entries += entry.ranges.len();
        self.insertion_order.push_back(path.to_path_buf());
        let _previous = self.cache.insert(path.to_path_buf(), entry);
        Ok(())
    }

    fn read_index(
        &mut self,
        path: &Path,
        identity: &FileIdentity,
    ) -> std::result::Result<(Vec<SymbolRange>, Option<SymbolFailure>), SymbolFailure> {
        if identity.size > self.max_file_bytes as u64 {
            return Err(SymbolFailure::TooLarge);
        }
        let mut file = open_regular_file(path).map_err(|_| SymbolFailure::Read)?;
        let opened = file.metadata().map_err(|_| SymbolFailure::Read)?;
        if FileIdentity::from_metadata(&opened)? != *identity {
            return Err(SymbolFailure::Changed);
        }
        let bounded =
            read_file_prefix(&mut file, path, self.max_file_bytes).map_err(
                |error| match error {
                    ProfilerError::Allocation(_) => SymbolFailure::Allocation,
                    _ => SymbolFailure::Read,
                },
            )?;
        if bounded.truncated {
            return Err(SymbolFailure::TooLarge);
        }
        let after = file.metadata().map_err(|_| SymbolFailure::Read)?;
        let visible = std::fs::metadata(path).map_err(|_| SymbolFailure::Unavailable)?;
        if FileIdentity::from_metadata(&after)? != *identity
            || FileIdentity::from_metadata(&visible)? != *identity
        {
            return Err(SymbolFailure::Changed);
        }
        let object =
            object::File::parse(bounded.bytes.as_slice()).map_err(|_| SymbolFailure::Malformed)?;
        let mut ranges = Vec::new();
        let mut incomplete = None;
        for symbol in object.symbols().chain(object.dynamic_symbols()) {
            if symbol.kind() != SymbolKind::Text || !symbol.is_definition() {
                continue;
            }
            let Some(section_index) = symbol.section_index() else {
                continue;
            };
            let section = object
                .section_by_index(section_index)
                .map_err(|_| SymbolFailure::Malformed)?;
            if section.kind() != SectionKind::Text {
                continue;
            }
            if let SectionFlags::Elf { sh_flags } = section.flags()
                && sh_flags & u64::from(object::elf::SHF_EXECINSTR) == 0
            {
                continue;
            }
            let Some((section_offset, section_size)) = section.file_range() else {
                continue;
            };
            if section_offset
                .checked_add(section_size)
                .is_none_or(|end| end > bounded.bytes.len() as u64)
                || section.address().checked_add(section.size()).is_none()
            {
                let _failure = incomplete.get_or_insert(SymbolFailure::Malformed);
                continue;
            }
            let Some(relative) = symbol.address().checked_sub(section.address()) else {
                continue;
            };
            let size = symbol.size();
            let Some(end) = relative
                .checked_add(size)
                .filter(|_| symbol.address().checked_add(size).is_some())
            else {
                let _failure = incomplete.get_or_insert(SymbolFailure::Malformed);
                continue;
            };
            if relative >= section_size || end > section_size || end > section.size() {
                continue;
            }
            let Some(file_offset) = section_offset.checked_add(relative) else {
                let _failure = incomplete.get_or_insert(SymbolFailure::Malformed);
                continue;
            };
            if file_offset.checked_add(size).is_none() {
                let _failure = incomplete.get_or_insert(SymbolFailure::Malformed);
                continue;
            }
            let name = match symbol.name() {
                Ok(name) if !name.is_empty() => name,
                Ok(_) => continue,
                Err(_) => {
                    let _failure = incomplete.get_or_insert(SymbolFailure::Malformed);
                    continue;
                }
            };
            if name.len() > self.max_string_bytes {
                incomplete = Some(SymbolFailure::Capacity);
                continue;
            }
            if ranges.len() == self.max_entries {
                incomplete = Some(SymbolFailure::Capacity);
                break;
            }
            // Existing indexes and the temporary index share the entry bound.
            while self.total_entries >= self.max_entries - ranges.len() {
                if !self.evict_oldest() {
                    return Err(SymbolFailure::Capacity);
                }
            }
            if ranges.len() == ranges.capacity() {
                let capacity = ranges
                    .capacity()
                    .saturating_mul(2)
                    .max(16)
                    .min(self.max_entries);
                ranges
                    .try_reserve_exact(capacity - ranges.len())
                    .map_err(|_| SymbolFailure::Allocation)?;
            }
            ranges.push(SymbolRange {
                file_offset,
                size,
                name: name.to_owned(),
            });
        }
        ranges.sort_unstable_by(|left, right| {
            (left.file_offset, left.size, &left.name).cmp(&(
                right.file_offset,
                right.size,
                &right.name,
            ))
        });
        ranges.dedup_by(|left, right| {
            left.file_offset == right.file_offset
                && left.size == right.size
                && left.name == right.name
        });
        // Deduplication must not leave a large allocation charged as a tiny
        // index; cached range capacity equals the retained entry count.
        Ok((ranges.into_boxed_slice().into_vec(), incomplete))
    }

    fn resolve_loaded(&self, path: &Path, file_offset: u64) -> Result<Option<ResolvedSymbol>> {
        let entry = self
            .cache
            .get(path)
            .ok_or_else(|| ProfilerError::InternalInvariant("missing symbol index".to_owned()))?;
        let index = entry
            .ranges
            .partition_point(|symbol| symbol.file_offset <= file_offset);
        for symbol in entry.ranges[..index].iter().rev() {
            let offset = file_offset - symbol.file_offset;
            if (symbol.size == 0 && offset == 0) || (symbol.size != 0 && offset < symbol.size) {
                return Ok(Some(ResolvedSymbol {
                    name: symbol.name.clone(),
                    offset,
                }));
            }
        }
        match entry.incomplete {
            Some(failure) => Err(failure.error()),
            None => Ok(None),
        }
    }
}

impl SymbolResolver for ObjectSymbolResolver {
    fn resolve(&mut self, path: &Path, file_offset: u64) -> Result<Option<ResolvedSymbol>> {
        self.load(path, None)?;
        self.resolve_loaded(path, file_offset)
    }

    fn resolve_backing(
        &mut self,
        path: &Path,
        file_offset: u64,
        expected: MappingFileIdentity,
    ) -> Result<Option<ResolvedSymbol>> {
        // The identity observed here is checked again on the opened descriptor
        // before/after reading; replacement between stat and open is rejected.
        self.load(path, Some(expected))?;
        self.resolve_loaded(path, file_offset)
    }

    fn statistics(&self) -> SymbolStatistics {
        SymbolStatistics {
            cache_entries: self.total_entries,
            cache_files: self.cache.len(),
            ..self.statistics
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use object::{
        Architecture, BinaryFormat, Endianness, SymbolFlags, SymbolScope,
        write::{Object as WriteObject, Symbol, SymbolSection},
    };
    use tempfile::TempDir;

    fn directory() -> TempDir {
        tempfile::Builder::new()
            .prefix("symbols-")
            .tempdir_in(".")
            .expect("project fixture directory")
    }

    fn fixture(kind: u16, name: &str) -> (Vec<u8>, u64) {
        let mut object =
            WriteObject::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
        let text = object.add_section(Vec::new(), b".text".to_vec(), SectionKind::Text);
        let _text_offset = object.append_section_data(text, &[0x90; 64], 16);
        let data = object.add_section(Vec::new(), b".data".to_vec(), SectionKind::Data);
        let _data_offset = object.append_section_data(data, &[0; 16], 8);
        for (symbol_name, section, value, size, symbol_kind) in [
            (
                name.as_bytes(),
                SymbolSection::Section(text),
                0,
                16,
                SymbolKind::Text,
            ),
            (
                b"zero".as_slice(),
                SymbolSection::Section(text),
                32,
                0,
                SymbolKind::Text,
            ),
            (
                b"undefined".as_slice(),
                SymbolSection::Undefined,
                0,
                0,
                SymbolKind::Text,
            ),
            (
                b"not_text".as_slice(),
                SymbolSection::Section(text),
                16,
                8,
                SymbolKind::Data,
            ),
            (
                b"data_text".as_slice(),
                SymbolSection::Section(data),
                0,
                8,
                SymbolKind::Text,
            ),
        ] {
            let _symbol = object.add_symbol(Symbol {
                name: symbol_name.to_vec(),
                value,
                size,
                kind: symbol_kind,
                scope: SymbolScope::Linkage,
                weak: false,
                section,
                flags: SymbolFlags::None,
            });
        }
        let mut bytes = object.write().expect("write independent ELF fixture");
        let parsed = object::File::parse(bytes.as_slice()).expect("parse fixture");
        let text = parsed.section_by_name(".text").expect("text section");
        let text_index = text.index().0;
        let file_offset = text.file_range().expect("text bytes").0;
        let symtab = parsed
            .section_by_name(".symtab")
            .expect("symbol table")
            .file_range()
            .expect("symbol bytes");
        let section_headers =
            u64::from_le_bytes(bytes[40..48].try_into().expect("e_shoff")) as usize;
        let section_size =
            u16::from_le_bytes(bytes[58..60].try_into().expect("e_shentsize")) as usize;
        let virtual_address = if kind == object::elf::ET_EXEC {
            0x40_1000_u64
        } else {
            0x1000_u64
        };
        bytes[16..18].copy_from_slice(&kind.to_le_bytes());
        let address_field = section_headers + text_index * section_size + 16;
        bytes[address_field..address_field + 8].copy_from_slice(&virtual_address.to_le_bytes());
        for index in (symtab.0 as usize..(symtab.0 + symtab.1) as usize).step_by(24) {
            let section =
                u16::from_le_bytes(bytes[index + 6..index + 8].try_into().expect("section"));
            if usize::from(section) == text_index {
                let value =
                    u64::from_le_bytes(bytes[index + 8..index + 16].try_into().expect("value"));
                bytes[index + 8..index + 16]
                    .copy_from_slice(&(virtual_address + value).to_le_bytes());
            }
        }
        (bytes, file_offset)
    }

    /// Scenario: ET_EXEC and ET_DYN functions have virtual addresses unlike file offsets.
    /// Guarantees: Native lookup translates through the containing executable section.
    #[test]
    fn translates_executable_and_shared_object_offsets() {
        let directory = directory();
        for kind in [object::elf::ET_EXEC, object::elf::ET_DYN] {
            let (bytes, offset) = fixture(kind, "work");
            let path = directory.path().join(format!("elf-{kind}"));
            std::fs::write(&path, bytes).expect("fixture");
            let mut resolver = ObjectSymbolResolver::with_limits(8, 2, 4096, 256).expect("limits");
            assert_eq!(
                resolver.resolve(&path, offset + 3).expect("resolution"),
                Some(ResolvedSymbol {
                    name: "work".to_owned(),
                    offset: 3,
                })
            );
            assert_eq!(
                resolver
                    .resolve(&path, 0x40_1003)
                    .expect("not a file offset"),
                None
            );
            assert_eq!(resolver.statistics().cache_hits, 1);
            assert_eq!(resolver.statistics().cache_entries, 2);
        }
    }

    /// Scenario: ELF tables include undefined, non-text, data-section, and zero-size symbols.
    /// Guarantees: Only defined executable functions resolve and zero sizes match exactly.
    #[test]
    fn excludes_non_code_and_bounds_zero_size() {
        let directory = directory();
        let (bytes, offset) = fixture(object::elf::ET_DYN, "work");
        let path = directory.path().join("functions");
        std::fs::write(&path, bytes).expect("fixture");
        let mut resolver = ObjectSymbolResolver::new(8, 4096);
        assert_eq!(
            resolver.resolve(&path, offset + 16).expect("function end"),
            None
        );
        assert_eq!(
            resolver
                .resolve(&path, offset + 32)
                .expect("exact zero symbol"),
            Some(ResolvedSymbol {
                name: "zero".to_owned(),
                offset: 0,
            })
        );
        assert_eq!(
            resolver
                .resolve(&path, offset + 33)
                .expect("past zero symbol"),
            None
        );
        assert_eq!(
            resolver
                .resolve(&path, offset + 64)
                .expect("non-code section"),
            None
        );
        assert_eq!(resolver.resolve(&path, 0).expect("undefined address"), None);
    }

    /// Scenario: A valid object has no symbol or dynamic-symbol entries.
    /// Guarantees: Stripped files return no function and still use a bounded cached index.
    #[test]
    fn stripped_object_is_cached_without_false_symbols() {
        let directory = directory();
        let path = directory.path().join("stripped");
        let mut object =
            WriteObject::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
        let text = object.add_section(Vec::new(), b".text".to_vec(), SectionKind::Text);
        let _offset = object.append_section_data(text, &[0x90; 16], 16);
        std::fs::write(&path, object.write().expect("stripped ELF")).expect("fixture");
        let mut resolver = ObjectSymbolResolver::with_limits(2, 1, 4096, 256).expect("limits");
        assert_eq!(
            resolver.resolve(&path, 64).expect("stripped resolution"),
            None
        );
        assert_eq!(
            resolver
                .resolve(&path, 64)
                .expect("cached stripped resolution"),
            None
        );
        assert_eq!(resolver.statistics().cache_files, 1);
        assert_eq!(resolver.statistics().cache_entries, 0);
        assert_eq!(resolver.statistics().cache_hits, 1);
    }

    /// Scenario: A malformed or missing object is requested repeatedly.
    /// Guarantees: Failures are fixed categories and negative hits avoid repeated parsing.
    #[test]
    fn malformed_and_missing_objects_are_negative_cached() {
        let directory = directory();
        for exists in [false, true] {
            let path = directory
                .path()
                .join(if exists { "malformed" } else { "missing" });
            if exists {
                std::fs::write(&path, b"not an ELF object").expect("fixture");
            }
            let mut resolver = ObjectSymbolResolver::with_limits(4, 2, 4096, 256).expect("limits");
            let first = resolver.resolve(&path, 0).expect_err("failure").to_string();
            let second = resolver
                .resolve(&path, 1)
                .expect_err("cached failure")
                .to_string();
            assert_eq!(first, second);
            assert!(!first.contains(path.to_str().expect("fixture path")));
            assert_eq!(resolver.statistics().cache_files, 1);
            assert_eq!(resolver.statistics().cache_hits, 1);
            assert_eq!(resolver.statistics().cache_entries, 0);
        }
    }

    /// Scenario: A missing file appears after a negative lookup retry interval.
    /// Guarantees: Negative caching is bounded in time and permits successful recovery.
    #[test]
    fn negative_cache_retries_newly_available_objects() {
        let directory = directory();
        let path = directory.path().join("appeared");
        let mut resolver = ObjectSymbolResolver::new(8, 4096);
        assert!(resolver.resolve(&path, 0).is_err());
        let (bytes, offset) = fixture(object::elf::ET_DYN, "ready");
        std::fs::write(&path, bytes).expect("new fixture");
        resolver
            .cache
            .get_mut(&path)
            .expect("negative entry")
            .checked_at = Instant::now() - Duration::from_secs(2);
        assert_eq!(
            resolver
                .resolve(&path, offset)
                .expect("retry")
                .expect("function")
                .name,
            "ready"
        );
        assert_eq!(resolver.statistics().cache_files, 1);
    }

    /// Scenario: A pathname is atomically replaced by another ELF object.
    /// Guarantees: Device, inode, size, and modification identity invalidate old functions.
    #[test]
    fn pathname_replacement_refreshes_symbols() {
        let directory = directory();
        let path = directory.path().join("current");
        let replacement = directory.path().join("replacement");
        let (bytes, offset) = fixture(object::elf::ET_EXEC, "before");
        std::fs::write(&path, bytes).expect("original");
        let mut resolver = ObjectSymbolResolver::new(8, 4096);
        assert_eq!(
            resolver
                .resolve(&path, offset)
                .expect("lookup")
                .expect("function")
                .name,
            "before"
        );
        let (bytes, _) = fixture(object::elf::ET_EXEC, "after");
        std::fs::write(&replacement, bytes).expect("replacement");
        std::fs::rename(&replacement, &path).expect("atomic replacement");
        assert_eq!(
            resolver
                .resolve(&path, offset)
                .expect("refresh")
                .expect("function")
                .name,
            "after"
        );
        assert_eq!(resolver.statistics().evictions, 1);
        assert_eq!(resolver.statistics().cache_files, 1);
    }

    /// Scenario: An object is modified in place, keeping its pathname and inode.
    /// Guarantees: Size and modification metadata invalidate the previously cached functions.
    #[test]
    fn in_place_object_changes_refresh_symbols() {
        let directory = directory();
        let path = directory.path().join("modified");
        let (bytes, offset) = fixture(object::elf::ET_DYN, "before");
        std::fs::write(&path, bytes).expect("original");
        let mut resolver = ObjectSymbolResolver::new(8, 4096);
        let before = resolver
            .resolve(&path, offset)
            .expect("lookup")
            .expect("function");
        assert_eq!(before.name, "before");
        let (bytes, _) = fixture(object::elf::ET_DYN, "replacement_function");
        std::fs::write(&path, bytes).expect("modify in place");
        let after = resolver
            .resolve(&path, offset)
            .expect("refresh")
            .expect("function");
        assert_eq!(after.name, "replacement_function");
        assert_eq!(resolver.statistics().evictions, 1);
    }

    /// Scenario: An executable pathname is replaced while an old mapping is
    /// still represented in a profile.
    /// Guarantees: Native lookup rejects the wrong inode instead of assigning
    /// the replacement's symbols, and a new mapping can resolve immediately.
    #[cfg(target_os = "linux")]
    #[test]
    fn mapped_lookup_requires_the_observed_backing_file() {
        use std::os::unix::fs::MetadataExt;
        let identity = |path: &Path| {
            let metadata = std::fs::metadata(path).expect("fixture metadata");
            MappingFileIdentity {
                device_major: nix::libc::major(metadata.dev()),
                device_minor: nix::libc::minor(metadata.dev()),
                inode: metadata.ino(),
            }
        };
        let directory = directory();
        let path = directory.path().join("mapped");
        let replacement = directory.path().join("replacement");
        let (bytes, offset) = fixture(object::elf::ET_DYN, "original");
        std::fs::write(&path, bytes).expect("original ELF");
        let original = identity(&path);
        let mut resolver = ObjectSymbolResolver::new(8, 4096);
        assert_eq!(
            resolver
                .resolve_backing(&path, offset, original)
                .expect("matching object")
                .expect("original symbol")
                .name,
            "original",
        );
        let (bytes, _) = fixture(object::elf::ET_DYN, "replacement");
        std::fs::write(&replacement, bytes).expect("replacement ELF");
        std::fs::rename(&replacement, &path).expect("replace pathname");
        assert!(resolver.resolve_backing(&path, offset, original).is_err());
        assert_eq!(
            resolver
                .resolve_backing(&path, offset, identity(&path))
                .expect("new mapping")
                .expect("replacement symbol")
                .name,
            "replacement",
        );
    }

    /// Scenario: More distinct files and function ranges arrive than the configured cache allows.
    /// Guarantees: File counts and allocated range slots stay bounded and evictions are counted.
    #[test]
    fn file_and_total_entry_capacities_are_bounded() {
        let directory = directory();
        let mut resolver = ObjectSymbolResolver::with_limits(3, 2, 4096, 256).expect("limits");
        for index in 0..5 {
            let (bytes, offset) = fixture(object::elf::ET_DYN, "work");
            let path = directory.path().join(format!("file-{index}"));
            std::fs::write(&path, bytes).expect("fixture");
            assert!(resolver.resolve(&path, offset).expect("lookup").is_some());
            assert!(resolver.statistics().cache_files <= 2);
            assert!(resolver.statistics().cache_entries <= 3);
            for entry in resolver.cache.values() {
                assert_eq!(entry.ranges.capacity(), entry.ranges.len());
            }
        }
        assert!(resolver.statistics().evictions >= 4);
        for index in 0..5 {
            assert!(
                resolver
                    .resolve(&directory.path().join(format!("missing-{index}")), 0)
                    .is_err()
            );
            assert!(resolver.statistics().cache_files <= 2);
            assert!(resolver.statistics().cache_entries <= 3);
        }
    }

    /// Scenario: The symbol-entry budget admits only part of an executable's useful index.
    /// Guarantees: Retained functions resolve while omitted ranges return explicit capacity failure.
    #[test]
    fn partial_index_returns_useful_symbols_and_explicit_capacity() {
        let directory = directory();
        let (bytes, offset) = fixture(object::elf::ET_DYN, "work");
        let path = directory.path().join("partial");
        std::fs::write(&path, bytes).expect("fixture");
        let mut resolver = ObjectSymbolResolver::with_limits(1, 1, 4096, 256).expect("limits");
        assert!(
            resolver
                .resolve(&path, offset)
                .expect("retained function")
                .is_some()
        );
        assert!(matches!(
            resolver.resolve(&path, offset + 32),
            Err(ProfilerError::Capacity(DropReason::SymbolCapacity))
        ));
        assert_eq!(resolver.statistics().cache_entries, 1);
        assert_eq!(resolver.statistics().capacity_rejections, 1);
    }

    /// Scenario: Object byte limits, string limits, or zero and overflowing bounds are supplied.
    /// Guarantees: No unbounded allocation or empty-capacity cache insertion is permitted.
    #[test]
    fn object_and_configuration_bounds_are_enforced() {
        let directory = directory();
        let path = directory.path().join("large");
        std::fs::write(&path, [0_u8; 32]).expect("fixture");
        let mut resolver = ObjectSymbolResolver::new(4, 8);
        assert!(matches!(
            resolver.resolve(&path, 0),
            Err(ProfilerError::Capacity(_))
        ));
        assert!(resolver.resolve(&path, 0).is_err());
        assert_eq!(resolver.statistics().capacity_rejections, 1);
        assert_eq!(resolver.statistics().cache_hits, 1);
        for (entries, files, bytes, strings) in [
            (0, 1, 32, 16),
            (1, 0, 32, 16),
            (1, 1, 0, 16),
            (1, 1, usize::MAX, 16),
            (1, 1, 32, 0),
        ] {
            assert!(ObjectSymbolResolver::with_limits(entries, files, bytes, strings).is_err());
        }
        let mut zero = ObjectSymbolResolver::new(0, 4096);
        assert!(zero.resolve(&path, 0).is_err());
        assert_eq!(zero.statistics().cache_files, 0);
        let mut short_path = ObjectSymbolResolver::with_limits(4, 1, 4096, 1).expect("limits");
        assert!(matches!(
            short_path.resolve(&path, 0),
            Err(ProfilerError::Capacity(_))
        ));
        assert_eq!(short_path.statistics().cache_files, 0);
    }

    /// Scenario: A valid ELF function has a name larger than the configured string budget.
    /// Guarantees: Oversized names are not copied into the cache or returned as arbitrary aliases.
    #[test]
    fn symbol_names_obey_the_string_budget() {
        let directory = directory();
        let path = directory.path().join("names");
        let (bytes, offset) = fixture(object::elf::ET_DYN, &"x".repeat(512));
        std::fs::write(&path, bytes).expect("fixture");
        let mut resolver = ObjectSymbolResolver::with_limits(4, 1, 4096, 128).expect("limits");
        assert!(matches!(
            resolver.resolve(&path, offset),
            Err(ProfilerError::Capacity(_))
        ));
        assert_eq!(resolver.statistics().capacity_rejections, 1);
        assert!(
            resolver.cache[&path]
                .ranges
                .iter()
                .all(|range| range.name.len() <= 128)
        );
        assert_eq!(
            resolver
                .resolve(&path, offset + 32)
                .expect("short name")
                .expect("zero")
                .name,
            "zero"
        );
    }

    /// Scenario: A shared object exposes only its dynamic symbol table.
    /// Guarantees: Defined dynamic functions receive the same file-offset translation as static ones.
    #[test]
    fn dynamic_symbol_table_resolves_without_static_symbols() {
        let directory = directory();
        let path = directory.path().join("dynamic");
        let (mut bytes, offset) = fixture(object::elf::ET_DYN, "exported");
        let object = object::File::parse(bytes.as_slice()).expect("fixture");
        let symtab_index = object.section_by_name(".symtab").expect("symtab").index().0;
        let headers = u64::from_le_bytes(bytes[40..48].try_into().expect("e_shoff")) as usize;
        let header_size =
            u16::from_le_bytes(bytes[58..60].try_into().expect("e_shentsize")) as usize;
        let section_type = headers + symtab_index * header_size + 4;
        bytes[section_type..section_type + 4]
            .copy_from_slice(&object::elf::SHT_DYNSYM.to_le_bytes());
        std::fs::write(&path, bytes).expect("dynamic ELF");
        let mut resolver = ObjectSymbolResolver::new(8, 4096);
        assert_eq!(
            resolver.resolve(&path, offset + 4).expect("dynamic lookup"),
            Some(ResolvedSymbol {
                name: "exported".to_owned(),
                offset: 4,
            })
        );
    }

    /// Scenario: A symbol advertises an overflowing size in an otherwise readable ELF file.
    /// Guarantees: Checked range arithmetic rejects the symbol without matching arbitrary code.
    #[test]
    fn overflowing_symbol_range_is_not_resolved() {
        let directory = directory();
        let path = directory.path().join("overflow");
        let (mut bytes, offset) = fixture(object::elf::ET_DYN, "overflow");
        let object = object::File::parse(bytes.as_slice()).expect("fixture");
        let symtab = object
            .section_by_name(".symtab")
            .expect("symtab")
            .file_range()
            .expect("symbol table range");
        for index in (symtab.0 as usize..(symtab.0 + symtab.1) as usize).step_by(24) {
            let value = u64::from_le_bytes(bytes[index + 8..index + 16].try_into().expect("value"));
            if value == 0x1000 {
                bytes[index + 16..index + 24].copy_from_slice(&u64::MAX.to_le_bytes());
            }
        }
        std::fs::write(&path, bytes).expect("malformed symbol size");
        let mut resolver = ObjectSymbolResolver::new(8, 4096);
        assert!(resolver.resolve(&path, offset).is_err());
        assert_eq!(
            resolver
                .resolve(&path, offset + 32)
                .expect("valid zero symbol")
                .expect("zero")
                .name,
            "zero"
        );
    }
}
