// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Bounds Aya's eager kernel-BTF read before constructing its loader.

use std::{fs::OpenOptions, os::unix::fs::OpenOptionsExt, path::Path};

use nix::sys::statfs::{SYSFS_MAGIC, fstatfs};

use crate::{ProfilerError, ProgramConfig, Result};

pub(crate) struct BtfShape {
    pub(crate) string_start: usize,
    pub(crate) string_len: usize,
    pub(crate) types: usize,
    pub(crate) members: usize,
}

pub(crate) fn kernel_btf(config: &ProgramConfig) -> Result<()> {
    let path = Path::new("/sys/kernel/btf/vmlinux");
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !config.require_btf => {
            return Ok(());
        }
        Err(source) => {
            return Err(ProfilerError::Io {
                operation: "preflight kernel BTF",
                path: path.into(),
                source,
            });
        }
    };
    let filesystem = fstatfs(&file).map_err(|error| {
        ProfilerError::ProgramLoad(format!("cannot identify kernel BTF filesystem: {error}"))
    })?;
    if filesystem.filesystem_type() != SYSFS_MAGIC {
        return Err(ProfilerError::ProgramLoad(
            "Aya's eager BTF input must be the immutable kernel sysfs export".to_owned(),
        ));
    }
    let metadata = file.metadata().map_err(|source| ProfilerError::Io {
        operation: "inspect kernel BTF",
        path: path.into(),
        source,
    })?;
    if metadata.len() == 0 || metadata.len() > config.max_kernel_btf_bytes as u64 {
        return Err(ProfilerError::ProgramLoad(
            "kernel BTF exceeds configured byte limit".to_owned(),
        ));
    }
    let bytes = super::procfs::read_handle(file, path, config.max_kernel_btf_bytes)?;
    let shape = inspect(
        &bytes,
        config.max_kernel_btf_types,
        config.max_kernel_btf_members,
    )?;
    let _bounded_records = (shape.types, shape.members);
    // vmlinux BTF is immutable for the running kernel. Mutable substitutes are
    // rejected above; no namespace/mount changes are made to hide BTF from Aya.
    Ok(())
}

pub(crate) fn inspect(bytes: &[u8], max_types: usize, max_members: usize) -> Result<BtfShape> {
    if bytes.len() < 24 || bytes[..4] != [0x9f, 0xeb, 1, 0] || word(bytes, 4)? != 24 {
        return Err(invalid("unsupported BTF header"));
    }
    let type_start = 24usize
        .checked_add(word(bytes, 8)? as usize)
        .ok_or_else(|| invalid("type offset overflow"))?;
    let type_end = type_start
        .checked_add(word(bytes, 12)? as usize)
        .ok_or_else(|| invalid("type length overflow"))?;
    let string_start = 24usize
        .checked_add(word(bytes, 16)? as usize)
        .ok_or_else(|| invalid("string offset overflow"))?;
    let string_len = word(bytes, 20)? as usize;
    let string_end = string_start
        .checked_add(string_len)
        .ok_or_else(|| invalid("string length overflow"))?;
    if type_end > string_start
        || string_end != bytes.len()
        || string_len == 0
        || bytes.get(string_start) != Some(&0)
    {
        return Err(invalid("invalid BTF section bounds"));
    }
    let mut cursor = type_start;
    let mut types = 0usize;
    let mut members = 0usize;
    while cursor < type_end {
        if type_end - cursor < 12 {
            return Err(invalid("truncated BTF type"));
        }
        let name = word(bytes, cursor)? as usize;
        let info = word(bytes, cursor + 4)?;
        if name >= string_len || info & 0x60ff_0000 != 0 {
            return Err(invalid("invalid BTF type metadata"));
        }
        let kind = (info >> 24) & 0x1f;
        let count = (info & 0xffff) as usize;
        let (width, variable) = match kind {
            1 | 14 | 17 => (4, false),
            3 => (12, false),
            4 | 5 | 15 | 19 => (12, true),
            6 | 13 => (8, true),
            2 | 7..=12 | 16 | 18 => (0, false),
            // In Aya Unknown has zero serialized size; rejecting it also
            // guarantees progress in the selected parser.
            _ => return Err(invalid("unsupported BTF type kind")),
        };
        let extra = if variable {
            members = members
                .checked_add(count)
                .ok_or_else(|| invalid("BTF member count overflow"))?;
            count
                .checked_mul(width)
                .ok_or_else(|| invalid("BTF member bytes overflow"))?
        } else {
            width
        };
        types = types
            .checked_add(1)
            .ok_or_else(|| invalid("BTF type count overflow"))?;
        if types > max_types || members > max_members {
            return Err(invalid("BTF type/member capacity reached"));
        }
        cursor = cursor
            .checked_add(12)
            .and_then(|cursor| cursor.checked_add(extra))
            .filter(|cursor| *cursor <= type_end)
            .ok_or_else(|| invalid("truncated BTF members"))?;
    }
    Ok(BtfShape {
        string_start,
        string_len,
        types,
        members,
    })
}

pub(crate) fn word(bytes: &[u8], offset: usize) -> Result<u32> {
    let end = offset
        .checked_add(4)
        .ok_or_else(|| invalid("BTF offset overflow"))?;
    let value: [u8; 4] = bytes
        .get(offset..end)
        .ok_or_else(|| invalid("truncated BTF word"))?
        .try_into()
        .map_err(|_| invalid("truncated BTF word"))?;
    Ok(u32::from_le_bytes(value))
}

pub(crate) fn inspect_ext(bytes: &[u8], btf: &[u8], shape: &BtfShape) -> Result<()> {
    if bytes.len() < 24 || bytes[..4] != [0x9f, 0xeb, 1, 0] {
        return Err(invalid("unsupported BTF.ext header"));
    }
    let header = word(bytes, 4)? as usize;
    if !matches!(header, 24 | 32) || header > bytes.len() {
        return Err(invalid("unsupported BTF.ext header length"));
    }
    if header == 32 && word(bytes, 28)? != 0 {
        return Err(invalid("CO-RE is outside the helper-only object contract"));
    }
    for (offset_field, expected_size) in [(8, 8usize), (16, 16)] {
        let len = word(bytes, offset_field + 4)? as usize;
        if len == 0 {
            continue;
        }
        let start = header
            .checked_add(word(bytes, offset_field)? as usize)
            .ok_or_else(|| invalid("BTF.ext offset overflow"))?;
        let end = start
            .checked_add(len)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| invalid("BTF.ext length overflow"))?;
        if len < 4 || word(bytes, start)? as usize != expected_size {
            return Err(invalid("unsupported BTF.ext record size"));
        }
        let mut cursor = start + 4;
        let mut sections = 0usize;
        while cursor < end {
            if end - cursor < 8 {
                return Err(invalid("truncated BTF.ext section"));
            }
            let name = word(bytes, cursor)? as usize;
            let count = word(bytes, cursor + 4)? as usize;
            if name >= shape.string_len {
                return Err(invalid("BTF.ext section name out of bounds"));
            }
            let name = &btf[shape.string_start + name..shape.string_start + shape.string_len];
            let terminator = name
                .iter()
                .take(257)
                .position(|byte| *byte == 0)
                .ok_or_else(|| invalid("BTF.ext section name exceeds 256 bytes"))?;
            if !matches!(
                &name[..terminator],
                b"perf_event/profile_cpu" | b"perf_event/profile_cpu_kernel"
            ) {
                return Err(invalid("unexpected BTF.ext section name"));
            }
            sections += 1;
            if sections > 128 {
                return Err(invalid("too many BTF.ext sections"));
            }
            cursor = cursor
                .checked_add(8)
                .and_then(|cursor| {
                    count
                        .checked_mul(expected_size)
                        .and_then(|len| cursor.checked_add(len))
                })
                .filter(|cursor| *cursor <= end)
                .ok_or_else(|| invalid("BTF.ext records exceed section"))?;
        }
    }
    Ok(())
}

fn invalid(reason: &'static str) -> ProfilerError {
    ProfilerError::ProgramLoad(reason.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(info: u32, members: &[u8]) -> Vec<u8> {
        let type_len = 12 + members.len() as u32;
        let mut bytes = vec![0x9f, 0xeb, 1, 0];
        for value in [24, 0, type_len, type_len, 1] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        for value in [0, info, 0] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes.extend_from_slice(members);
        bytes.push(0);
        bytes
    }

    /// Scenario: BTF contains a bounded structure with one member.
    /// Guarantees: Preflight computes allocation-driving type/member counts.
    #[test]
    fn btf_counts_are_bounded_before_aya() {
        let bytes = fixture((4 << 24) | 1, &[0; 12]);
        let shape = inspect(&bytes, 1, 1).expect("valid BTF");
        assert_eq!((shape.types, shape.members), (1, 1));
        assert!(inspect(&bytes, 1, 0).is_err());
        assert!(inspect(&bytes, 0, 1).is_err());
    }

    /// Scenario: BTF declares unknown kinds, oversized vectors, or bad sections.
    /// Guarantees: Aya never receives a non-progressing or out-of-range type stream.
    #[test]
    fn malformed_btf_is_rejected_without_allocation() {
        assert!(inspect(&fixture(0, &[]), 100, 100).is_err());
        assert!(inspect(&fixture((4 << 24) | 65535, &[]), 100, 100).is_err());
        let mut bytes = fixture(2 << 24, &[]);
        bytes[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(inspect(&bytes, 100, 100).is_err());
    }

    /// Scenario: A developer explicitly requests checking the running kernel's BTF.
    /// Guarantees: The same deployment preflight is exercisable without BPF privileges.
    #[test]
    fn opted_in_kernel_btf_preflight() {
        if std::env::var_os("OTEL_EBPF_PROFILER_CHECK_BTF").is_none() {
            return;
        }
        kernel_btf(&ProgramConfig::default()).expect("kernel BTF fits the configured envelope");
    }
}
