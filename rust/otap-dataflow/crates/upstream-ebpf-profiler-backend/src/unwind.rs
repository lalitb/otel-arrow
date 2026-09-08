// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Native executable identity and frame-pointer unwind metadata encoding.

use std::fmt;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use object::{Object, ObjectSegment, SegmentFlags};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

use crate::error::BackendError;
use crate::statistics::DropReason;

/// Upstream stack-delta pages contain 16 address bits.
pub const STACK_DELTA_PAGE_BITS: u32 = 16;
/// Low-address mask for one stack-delta page.
pub const STACK_DELTA_PAGE_MASK: u64 = (1_u64 << STACK_DELTA_PAGE_BITS) - 1;
/// Direct-command flag stored in the 16-bit unwind-info field.
pub const STACK_DELTA_COMMAND_FLAG: u16 = 0x8000;
const MAX_EXECUTABLE_SEGMENTS: usize = 1024;

/// Stable 128-bit executable identity compatible with the upstream hash algorithm.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ExecutableId([u8; 16]);

impl ExecutableId {
    /// Hashes the first 4 KiB, last 4 KiB, and big-endian file length.
    pub fn from_reader(reader: &mut (impl Read + Seek)) -> Result<Self, BackendError> {
        let mut hasher = Sha256::new();
        let mut header = [0_u8; 4096];
        let header_len = crate::input::read_prefix(reader, &mut header)
            .map_err(|error| BackendError::ArtifactParse(format!("hash file header: {error}")))?;
        hasher.update(&header[..header_len]);
        let size = reader
            .seek(SeekFrom::End(0))
            .map_err(|error| BackendError::ArtifactParse(format!("seek file end: {error}")))?;
        let tail = size.min(4096);
        let _position = reader
            .seek(SeekFrom::End(-(tail as i64)))
            .map_err(|error| BackendError::ArtifactParse(format!("seek file trailer: {error}")))?;
        let mut trailer = [0_u8; 4096];
        let trailer_len = crate::input::read_prefix(reader, &mut trailer[..tail as usize])
            .map_err(|error| BackendError::ArtifactParse(format!("hash file trailer: {error}")))?;
        if trailer_len != tail as usize {
            return Err(BackendError::ArtifactParse(
                "executable shortened during identity read".to_owned(),
            ));
        }
        hasher.update(&trailer[..trailer_len]);
        hasher.update(size.to_be_bytes());
        let digest: [u8; 32] = hasher.finalize().into();
        let mut id = [0_u8; 16];
        id.copy_from_slice(&digest[..16]);
        Ok(Self(id))
    }

    /// Hashes an executable file.
    pub fn from_path(path: &Path) -> Result<Self, BackendError> {
        let mut file = std::fs::File::open(path).map_err(|source| BackendError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_reader(&mut file)
    }

    /// Returns the high 64 bits used by upstream BPF maps.
    #[must_use]
    pub const fn kernel_id(self) -> u64 {
        u64::from_be_bytes([
            self.0[0], self.0[1], self.0[2], self.0[3], self.0[4], self.0[5], self.0[6], self.0[7],
        ])
    }

    /// Returns all 16 identity bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Debug for ExecutableId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl fmt::Display for ExecutableId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&hex::encode(self.0))
    }
}

impl Serialize for ExecutableId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ExecutableId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let text = String::deserialize(deserializer)?;
        let bytes = hex::decode(&text).map_err(serde::de::Error::custom)?;
        let bytes: [u8; 16] = bytes.try_into().map_err(|value: Vec<u8>| {
            serde::de::Error::custom(format!("expected 16 bytes, got {}", value.len()))
        })?;
        Ok(Self(bytes))
    }
}

/// One ELF load segment needed to translate procfs file offsets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExecutableSegment {
    /// File offset of the segment.
    pub file_offset: u64,
    /// Bytes backed by the file.
    pub file_size: u64,
    /// ELF virtual address.
    pub virtual_address: u64,
    /// Bytes present in memory.
    pub memory_size: u64,
    /// Whether the segment is executable.
    pub executable: bool,
}

/// Minimal ELF layout needed by the native-only synchronizer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutableLayout {
    /// Loadable ELF segments.
    pub segments: Vec<ExecutableSegment>,
}

impl ExecutableLayout {
    /// Parses load segments from ELF bytes.
    pub fn parse(bytes: &[u8]) -> Result<Self, BackendError> {
        let file = object::File::parse(bytes)
            .map_err(|error| BackendError::ArtifactParse(format!("executable ELF: {error}")))?;
        let mut segments = Vec::new();
        for segment in file.segments() {
            if segments.len() == MAX_EXECUTABLE_SEGMENTS {
                return Err(BackendError::Capacity("executable load segments"));
            }
            let (file_offset, file_size) = segment.file_range();
            if file_offset.checked_add(file_size).is_none()
                || segment.address().checked_add(segment.size()).is_none()
            {
                return Err(BackendError::ArtifactParse(
                    "overflowing ELF segment range".to_owned(),
                ));
            }
            let executable = matches!(
                segment.flags(),
                SegmentFlags::Elf { p_flags } if p_flags & object::elf::PF_X != 0
            );
            segments.push(ExecutableSegment {
                file_offset,
                file_size,
                virtual_address: segment.address(),
                memory_size: segment.size(),
                executable,
            });
        }
        segments.sort_by_key(|segment| (segment.virtual_address, segment.file_offset));
        if segments.is_empty() {
            return Err(BackendError::ArtifactParse(
                "executable has no loadable segments".to_owned(),
            ));
        }
        Ok(Self { segments })
    }

    /// Maps an executable procfs file offset into ELF virtual address space.
    ///
    /// Linux maps whole pages, including the prefix before an unaligned
    /// executable segment. Non-executable segments cannot determine its bias.
    /// Invalid page sizes and ambiguous executable aliases return `None`.
    #[must_use]
    pub fn virtual_address_for_file_offset(&self, offset: u64, page_size: u64) -> Option<u64> {
        if !page_size.is_power_of_two() {
            return None;
        }
        let mask = page_size - 1;
        let mut address = None;
        for segment in self
            .segments
            .iter()
            .filter(|segment| segment.executable && segment.file_size != 0)
        {
            let start = segment.file_offset & !mask;
            let end = segment
                .file_offset
                .checked_add(segment.file_size)?
                .checked_add(mask)?
                & !mask;
            if offset < start || offset >= end {
                continue;
            }
            let virtual_start = segment
                .virtual_address
                .checked_sub(segment.file_offset - start)?;
            let candidate = virtual_start.checked_add(offset - start)?;
            if address.is_some_and(|previous| previous != candidate) {
                return None;
            }
            address = Some(candidate);
        }
        address
    }

    /// Returns executable virtual ranges.
    #[must_use]
    pub fn executable_ranges(&self) -> Vec<(u64, u64)> {
        self.segments
            .iter()
            .filter(|segment| segment.executable && segment.memory_size != 0)
            .filter_map(|segment| {
                segment
                    .virtual_address
                    .checked_add(segment.memory_size)
                    .map(|end| (segment.virtual_address, end))
            })
            .collect()
    }
}

/// Four-byte entry stored in an inner stack-delta array.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StackDelta {
    /// Low 16 bits of an executable-relative address.
    pub address_low: u16,
    /// Direct command or unwind-info array index.
    pub unwind_info: u16,
}

/// Page lookup metadata stored in `stack_delta_page_to_info`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StackDeltaPage {
    /// Executable-relative 64 KiB page.
    pub page: u64,
    /// First delta index for this page.
    pub first_delta: u32,
    /// Deltas beginning in this page.
    pub delta_count: u16,
    /// Outer map bucket number.
    pub map_id: u16,
}

/// Complete bounded upload for one frame-pointer-enabled executable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FramePointerUnwindPlan {
    /// Selected `exe_id_to_<map_id>_stack_deltas` bucket.
    pub map_id: u16,
    /// Sorted stack deltas.
    pub deltas: Vec<StackDelta>,
    /// Page lookup records.
    pub pages: Vec<StackDeltaPage>,
    /// Logical encoded bytes.
    pub logical_bytes: usize,
}

impl FramePointerUnwindPlan {
    /// Creates direct frame-pointer commands over executable ELF segments.
    ///
    /// This intentionally supports only binaries compiled with reliable frame
    /// pointers. Full `.eh_frame`, `.debug_frame`, and Go pclntab extraction is
    /// outside this native-only milestone.
    pub fn from_layout(
        layout: &ExecutableLayout,
        max_deltas: usize,
        max_pages: usize,
        max_bytes: usize,
    ) -> Result<Self, DropReason> {
        if layout.segments.len() > MAX_EXECUTABLE_SEGMENTS {
            return Err(DropReason::StackDeltaCapacity);
        }
        let mut ranges = Vec::<(u64, u64)>::new();
        for segment in &layout.segments {
            if !segment.executable || segment.memory_size == 0 {
                continue;
            }
            let end = segment
                .virtual_address
                .checked_add(segment.memory_size)
                .ok_or(DropReason::MalformedRecord)?;
            ranges.push((segment.virtual_address, end));
        }
        ranges.sort_unstable();
        let mut merged = Vec::<(u64, u64)>::new();
        for (start, end) in ranges {
            match merged.last_mut() {
                Some(previous) if start <= previous.1 => {
                    previous.1 = previous.1.max(end);
                }
                _ => {
                    if merged.len() >= max_deltas / 2 {
                        return Err(DropReason::StackDeltaCapacity);
                    }
                    merged.push((start, end));
                }
            }
        }
        let mut commands = Vec::with_capacity(merged.len() * 2);
        for (start, end) in merged {
            commands.push((start, STACK_DELTA_COMMAND_FLAG | 4));
            commands.push((end, STACK_DELTA_COMMAND_FLAG));
        }
        commands.sort_unstable();
        commands.dedup();
        if commands.is_empty() {
            return Err(DropReason::StackDeltaCapacity);
        }
        if commands.len() > max_deltas {
            return Err(DropReason::StackDeltaCapacity);
        }
        let map_id = bucket_for_deltas(commands.len())?;
        let first_page = commands[0].0 & !STACK_DELTA_PAGE_MASK;
        let last_page = commands[commands.len() - 1].0 & !STACK_DELTA_PAGE_MASK;
        let page_count = usize::try_from((last_page - first_page) >> STACK_DELTA_PAGE_BITS)
            .ok()
            .and_then(|count| count.checked_add(1))
            .ok_or(DropReason::StackDeltaCapacity)?;
        if page_count > max_pages {
            return Err(DropReason::StackDeltaCapacity);
        }
        let logical_bytes = commands
            .len()
            .checked_mul(4)
            .and_then(|bytes| {
                page_count
                    .checked_mul(24)
                    .and_then(|pages| bytes.checked_add(pages))
            })
            .ok_or(DropReason::UnwindByteCapacity)?;
        if logical_bytes > max_bytes {
            return Err(DropReason::UnwindByteCapacity);
        }

        let deltas = commands
            .iter()
            .map(|(address, unwind_info)| StackDelta {
                address_low: (*address & STACK_DELTA_PAGE_MASK) as u16,
                unwind_info: *unwind_info,
            })
            .collect::<Vec<_>>();
        let mut pages = Vec::with_capacity(page_count);
        let mut first_delta = 0_usize;
        for page_index in 0..page_count {
            let page = first_page + ((page_index as u64) << STACK_DELTA_PAGE_BITS);
            while first_delta < commands.len() && commands[first_delta].0 < page {
                first_delta += 1;
            }
            let mut end_delta = first_delta;
            while end_delta < commands.len()
                && commands[end_delta].0 & !STACK_DELTA_PAGE_MASK == page
            {
                end_delta += 1;
            }
            pages.push(StackDeltaPage {
                page,
                first_delta: u32::try_from(first_delta)
                    .map_err(|_| DropReason::StackDeltaCapacity)?,
                delta_count: u16::try_from(end_delta - first_delta)
                    .map_err(|_| DropReason::StackDeltaCapacity)?,
                map_id,
            });
        }
        Ok(Self {
            map_id,
            deltas,
            pages,
            logical_bytes,
        })
    }
}

fn bucket_for_deltas(count: usize) -> Result<u16, DropReason> {
    let significant_bits = usize::BITS - count.saturating_sub(1).leading_zeros();
    let bucket = significant_bits.max(8);
    if bucket > 23 {
        return Err(DropReason::StackDeltaCapacity);
    }
    u16::try_from(bucket).map_err(|_| DropReason::StackDeltaCapacity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Scenario: The upstream executable-ID fixture bytes are hashed.
    /// Guarantees: Header/trailer/length hashing remains byte-compatible with
    /// the upstream Go implementation.
    #[test]
    fn executable_id_matches_upstream_fixture() {
        let mut bytes = vec![0x7f, b'E', b'L', b'F', 0x00, 0x01, 0x02, 0x03, 0x04];
        let id = ExecutableId::from_reader(&mut Cursor::new(&mut bytes)).expect("hash");
        assert_eq!(id.to_string(), "caf6e5907166ac76eef618e5f7f59cd9");
        assert_eq!(id.kernel_id(), 0xcaf6e5907166ac76);
    }

    /// Scenario: Frame-pointer commands span more than one 64 KiB page.
    /// Guarantees: Every page receives deterministic lookup metadata and empty
    /// middle pages point across page boundaries to the preceding command.
    #[test]
    fn frame_pointer_plan_builds_cross_page_metadata() {
        let layout = ExecutableLayout {
            segments: vec![ExecutableSegment {
                file_offset: 0,
                file_size: 0x30000,
                virtual_address: 0x1000,
                memory_size: 0x30000,
                executable: true,
            }],
        };
        let plan =
            FramePointerUnwindPlan::from_layout(&layout, 256, 16, 4096).expect("bounded plan");
        assert_eq!(plan.map_id, 8);
        assert!(plan.pages.len() >= 4);
        assert_eq!(plan.deltas[0].unwind_info, STACK_DELTA_COMMAND_FLAG | 4);
        assert_eq!(
            plan.deltas.last().expect("invalid terminator").unwind_info,
            STACK_DELTA_COMMAND_FLAG
        );
    }

    /// Scenario: The configured native-unwind byte budget is smaller than one plan.
    /// Guarantees: Metadata generation fails before allocating or uploading
    /// unbounded kernel state.
    #[test]
    fn unwind_byte_budget_is_enforced() {
        let layout = ExecutableLayout {
            segments: vec![ExecutableSegment {
                file_offset: 0,
                file_size: 4096,
                virtual_address: 0,
                memory_size: 4096,
                executable: true,
            }],
        };
        assert_eq!(
            FramePointerUnwindPlan::from_layout(&layout, 256, 16, 1),
            Err(DropReason::UnwindByteCapacity)
        );
    }

    /// Scenario: The current test executable is parsed without debug or BPF tooling.
    /// Guarantees: At least one executable load segment and file-offset mapping
    /// are discoverable using the pure-Rust metadata path.
    #[test]
    #[cfg(target_os = "linux")]
    fn current_executable_layout_is_parseable() {
        let path = std::env::current_exe().expect("current executable");
        let bytes = std::fs::read(path).expect("read executable");
        let layout = ExecutableLayout::parse(&bytes).expect("ELF layout");
        assert!(!layout.executable_ranges().is_empty());
    }

    /// Scenario: A reader returns short successful reads for the executable header.
    /// Guarantees: Executable IDs match a contiguous reader and do not depend on read chunking.
    #[test]
    fn executable_id_handles_short_reads() {
        struct ShortReader(Cursor<Vec<u8>>);
        impl Read for ShortReader {
            fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
                let count = output.len().min(3);
                self.0.read(&mut output[..count])
            }
        }
        impl Seek for ShortReader {
            fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
                self.0.seek(position)
            }
        }
        let bytes = vec![0x42; 9000];
        let expected = ExecutableId::from_reader(&mut Cursor::new(bytes.clone())).expect("ID");
        let actual = ExecutableId::from_reader(&mut ShortReader(Cursor::new(bytes))).expect("ID");
        assert_eq!(actual, expected);
    }

    /// Scenario: A file offset precedes a load segment or translates past the address space.
    /// Guarantees: ELF lookup returns None instead of eager arithmetic underflow or overflow.
    #[test]
    fn file_offset_lookup_is_checked() {
        let layout = ExecutableLayout {
            segments: vec![ExecutableSegment {
                file_offset: 4096,
                file_size: 4096,
                virtual_address: u64::MAX - 1,
                memory_size: 4096,
                executable: true,
            }],
        };
        assert_eq!(layout.virtual_address_for_file_offset(0, 1), None);
        assert_eq!(
            layout.virtual_address_for_file_offset(4096, 1),
            Some(u64::MAX - 1)
        );
        assert_eq!(layout.virtual_address_for_file_offset(4098, 1), None);
    }

    /// Scenario: LLD places executable bytes on a file page also used by a preceding read-only segment.
    /// Guarantees: Procfs offset 0x16000 maps to executable virtual address 0x17000, not the read-only alias.
    #[test]
    fn executable_mapping_uses_page_aligned_load_segment() {
        let layout = ExecutableLayout {
            segments: vec![
                ExecutableSegment {
                    file_offset: 0,
                    file_size: 0x166f0,
                    virtual_address: 0,
                    memory_size: 0x166f0,
                    executable: false,
                },
                ExecutableSegment {
                    file_offset: 0x166f0,
                    file_size: 0x2800,
                    virtual_address: 0x176f0,
                    memory_size: 0x2800,
                    executable: true,
                },
            ],
        };
        assert_eq!(
            layout.virtual_address_for_file_offset(0x16000, 4096),
            Some(0x17000)
        );
        assert_eq!(
            layout.virtual_address_for_file_offset(0x17000, 4096),
            Some(0x18000)
        );
        assert_eq!(layout.virtual_address_for_file_offset(0x10000, 4096), None);
    }

    /// Scenario: A large-page host aligns an executable mapping below its ELF segment offset.
    /// Guarantees: Translation uses the supplied host page size rather than assuming 4 KiB pages.
    #[test]
    fn executable_mapping_respects_large_pages() {
        let layout = ExecutableLayout {
            segments: vec![ExecutableSegment {
                file_offset: 0x166f0,
                file_size: 0x2800,
                virtual_address: 0x266f0,
                memory_size: 0x2800,
                executable: true,
            }],
        };
        assert_eq!(
            layout.virtual_address_for_file_offset(0x10000, 65536),
            Some(0x20000)
        );
        assert_eq!(layout.virtual_address_for_file_offset(0x10000, 4096), None);
        assert_eq!(layout.virtual_address_for_file_offset(0x10000, 0), None);
    }

    /// Scenario: Executable segments overlap and reach the last representable address page.
    /// Guarantees: The plan merges coverage and encodes the final page without overflow.
    #[test]
    fn overlapping_and_high_segments_are_bounded() {
        let segment = |start, size| ExecutableSegment {
            file_offset: 0,
            file_size: size,
            virtual_address: start,
            memory_size: size,
            executable: true,
        };
        let layout = ExecutableLayout {
            segments: vec![segment(0x1000, 0x2000), segment(0x2000, 0x3000)],
        };
        let plan = FramePointerUnwindPlan::from_layout(&layout, 4, 2, 1024).expect("merged plan");
        assert_eq!(plan.deltas.len(), 2);
        assert_eq!(plan.deltas[1].address_low, 0x5000);
        let layout = ExecutableLayout {
            segments: vec![segment(u64::MAX - 4096, 4096)],
        };
        let plan = FramePointerUnwindPlan::from_layout(&layout, 2, 1, 1024).expect("high page");
        assert_eq!(plan.pages.len(), 1);
        assert_eq!(plan.pages[0].delta_count, 2);
    }
}
