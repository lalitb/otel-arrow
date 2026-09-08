// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Bounded `/proc/<pid>/maps` parsing and LPM range decomposition.

use serde::{Deserialize, Serialize};

pub(crate) const MAX_MAPS_LINE_BYTES: usize = 8192;

/// One mapping parsed from procfs.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct ProcessMapping {
    /// Inclusive virtual start.
    pub start: u64,
    /// Exclusive virtual end.
    pub end: u64,
    /// File offset at `start`.
    pub file_offset: u64,
    /// Device major number.
    pub device_major: u32,
    /// Device minor number.
    pub device_minor: u32,
    /// Backing inode.
    pub inode: u64,
    /// Mapping path with a trailing ` (deleted)` marker removed.
    pub path: String,
    /// Read permission.
    pub readable: bool,
    /// Write permission.
    pub writable: bool,
    /// Execute permission.
    pub executable: bool,
    /// Private rather than shared mapping.
    pub private: bool,
}

impl ProcessMapping {
    /// Returns the mapping length.
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    /// Returns whether the mapping has zero length.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// Returns whether native unwind metadata should be considered.
    #[must_use]
    pub fn is_executable_file(&self) -> bool {
        self.executable && self.inode != 0 && self.path.starts_with('/')
    }
}

/// A minimal power-of-two prefix covering part of a virtual range.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct LpmPrefix {
    /// First address covered.
    pub key: u64,
    /// Number of most-significant address bits compared.
    pub length: u32,
}

/// Mapping parser failure.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum MappingError {
    /// A procfs line exceeded its fixed parser bound.
    #[error("maps line has {actual} bytes, maximum is {maximum}")]
    LineTooLong {
        /// Actual byte count.
        actual: usize,
        /// Fixed maximum.
        maximum: usize,
    },
    /// A field was missing or malformed.
    #[error("invalid maps line field {field}: {value:?}")]
    InvalidField {
        /// Field label.
        field: &'static str,
        /// Rejected value.
        value: String,
    },
    /// A range was empty or reversed.
    #[error("invalid address range {start:#x}..{end:#x}")]
    InvalidRange {
        /// Start address.
        start: u64,
        /// End address.
        end: u64,
    },
}

/// Parses one procfs maps line.
pub fn parse_mapping_line(line: &str) -> Result<ProcessMapping, MappingError> {
    if line.len() > MAX_MAPS_LINE_BYTES {
        return Err(MappingError::LineTooLong {
            actual: line.len(),
            maximum: MAX_MAPS_LINE_BYTES,
        });
    }
    // Preserve the pathname verbatim: whitespace within a filename is not a
    // field separator after the inode.
    let mut remainder = line;
    let mut fields = std::iter::from_fn(|| {
        remainder = remainder.trim_start_matches(char::is_whitespace);
        if remainder.is_empty() {
            return None;
        }
        let end = remainder
            .find(char::is_whitespace)
            .unwrap_or(remainder.len());
        let field = &remainder[..end];
        remainder = &remainder[end..];
        Some(field)
    });
    let range = required(&mut fields, "address range")?;
    let permissions = required(&mut fields, "permissions")?;
    let file_offset = parse_hex(required(&mut fields, "file offset")?, "file offset")?;
    let device = required(&mut fields, "device")?;
    let inode_text = required(&mut fields, "inode")?;
    let inode = inode_text
        .parse::<u64>()
        .map_err(|_| MappingError::InvalidField {
            field: "inode",
            value: inode_text.to_owned(),
        })?;
    let path = remainder.trim_start_matches(char::is_whitespace);

    let (start_text, end_text) =
        range
            .split_once('-')
            .ok_or_else(|| MappingError::InvalidField {
                field: "address range",
                value: range.to_owned(),
            })?;
    let start = parse_hex(start_text, "address start")?;
    let end = parse_hex(end_text, "address end")?;
    if end <= start {
        return Err(MappingError::InvalidRange { start, end });
    }
    let (major_text, minor_text) =
        device
            .split_once(':')
            .ok_or_else(|| MappingError::InvalidField {
                field: "device",
                value: device.to_owned(),
            })?;
    let device_major = parse_hex(major_text, "device major")?;
    let device_minor = parse_hex(minor_text, "device minor")?;
    let permission_bytes = permissions.as_bytes();
    if permission_bytes.len() != 4
        || !matches!(permission_bytes[0], b'r' | b'-')
        || !matches!(permission_bytes[1], b'w' | b'-')
        || !matches!(permission_bytes[2], b'x' | b'-')
        || !matches!(permission_bytes[3], b'p' | b's')
    {
        return Err(MappingError::InvalidField {
            field: "permissions",
            value: permissions.to_owned(),
        });
    }

    Ok(ProcessMapping {
        start,
        end,
        file_offset,
        device_major: u32::try_from(device_major).map_err(|_| MappingError::InvalidField {
            field: "device major",
            value: major_text.to_owned(),
        })?,
        device_minor: u32::try_from(device_minor).map_err(|_| MappingError::InvalidField {
            field: "device minor",
            value: minor_text.to_owned(),
        })?,
        inode,
        path: path.strip_suffix(" (deleted)").unwrap_or(path).to_owned(),
        readable: permission_bytes[0] == b'r',
        writable: permission_bytes[1] == b'w',
        executable: permission_bytes[2] == b'x',
        private: permission_bytes[3] == b'p',
    })
}

/// Parses executable file mappings up to a fixed capacity.
#[must_use]
pub fn parse_executable_mappings(
    contents: &str,
    capacity: usize,
) -> (Vec<ProcessMapping>, u64, u64) {
    let mut mappings = Vec::with_capacity(capacity.min(128));
    let mut parse_errors = 0_u64;
    let mut capacity_drops = 0_u64;
    for line in contents.lines() {
        match parse_mapping_line(line) {
            Ok(mapping) if mapping.is_executable_file() => {
                if mappings.len() == capacity {
                    capacity_drops = capacity_drops.saturating_add(1);
                } else {
                    mappings.push(mapping);
                }
            }
            Ok(_) => {}
            Err(_) => parse_errors = parse_errors.saturating_add(1),
        }
    }
    (mappings, parse_errors, capacity_drops)
}

/// Decomposes `[start, end)` into the smallest ordered set of LPM prefixes.
#[must_use = "prefix decomposition failures must be handled"]
pub fn calculate_prefixes(start: u64, end: u64) -> Result<Vec<LpmPrefix>, MappingError> {
    if end <= start {
        return Err(MappingError::InvalidRange { start, end });
    }
    let mut prefixes = Vec::new();
    let mut current = start;
    while current < end {
        let mut block = current & current.wrapping_neg();
        if block == 0 {
            block = 1_u64 << 63;
        }
        while block > end - current {
            block >>= 1;
        }
        prefixes.push(LpmPrefix {
            key: current,
            length: 1 + block.leading_zeros(),
        });
        current += block;
    }
    Ok(prefixes)
}

fn required<'a>(
    fields: &mut impl Iterator<Item = &'a str>,
    field: &'static str,
) -> Result<&'a str, MappingError> {
    fields.next().ok_or_else(|| MappingError::InvalidField {
        field,
        value: String::new(),
    })
}

fn parse_hex(value: &str, field: &'static str) -> Result<u64, MappingError> {
    u64::from_str_radix(value, 16).map_err(|_| MappingError::InvalidField {
        field,
        value: value.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: A standard executable procfs mapping contains a deleted-file suffix.
    /// Guarantees: Numeric fields, permissions, and the original usable path are
    /// parsed without retaining the kernel suffix.
    #[test]
    fn parses_executable_mapping() {
        let mapping = parse_mapping_line(
            "55ff3f68a000-55ff3f740000 r-xp 00002000 08:12 42 /usr/bin/demo (deleted)",
        )
        .expect("valid mapping");
        assert_eq!(mapping.start, 0x55ff3f68a000);
        assert_eq!(mapping.file_offset, 0x2000);
        assert_eq!(mapping.device_major, 8);
        assert_eq!(mapping.device_minor, 0x12);
        assert_eq!(mapping.path, "/usr/bin/demo");
        assert!(mapping.is_executable_file());
    }

    /// Scenario: The range 10 through 22 is encoded for an LPM trie.
    /// Guarantees: Prefix generation matches the upstream minimal decomposition
    /// and covers the range without overlap.
    #[test]
    fn prefix_decomposition_matches_upstream() {
        assert_eq!(
            calculate_prefixes(10, 22).expect("valid range"),
            [
                LpmPrefix {
                    key: 10,
                    length: 63
                },
                LpmPrefix {
                    key: 12,
                    length: 62
                },
                LpmPrefix {
                    key: 16,
                    length: 62
                },
                LpmPrefix {
                    key: 20,
                    length: 63
                },
            ]
        );
    }

    /// Scenario: More executable mappings are present than configured capacity.
    /// Guarantees: Parsing continues, returned storage remains bounded, and
    /// excess mappings are counted separately from malformed lines.
    #[test]
    fn executable_mapping_capacity_is_bounded() {
        let line = "1000-2000 r-xp 00000000 08:01 1 /bin/demo";
        let input = format!("{line}\n{line}\n");
        let (mappings, parse_errors, drops) = parse_executable_mappings(&input, 1);
        assert_eq!(mappings.len(), 1);
        assert_eq!(parse_errors, 0);
        assert_eq!(drops, 1);
    }

    /// Scenario: An executable filename contains repeated spaces and a tab.
    /// Guarantees: Parsing does not rewrite the path into a different on-disk filename.
    #[test]
    fn preserves_pathname_whitespace() {
        let mapping = parse_mapping_line("1000-2000 r-xp 0 08:01 1 /tmp/two  spaces\tfile")
            .expect("mapping with whitespace");
        assert_eq!(mapping.path, "/tmp/two  spaces\tfile");
    }
}
