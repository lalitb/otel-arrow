// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Shared constants, normalized value types, and checked byte-level codec
//! helpers for the version-1 filelog checkpoint format.
//!
//! Every constant and field width here mirrors
//! `docs/filelog-checkpoint-format.md` exactly; that document is the
//! authoritative source and this module must be kept in lockstep with it.
//!
//! All multi-byte integers are big-endian. All variable-length fields are
//! read and written through [`ByteReader`] / [`ByteWriter`], which perform
//! checked-arithmetic bounds validation before any slicing or allocation, so
//! a corrupted or adversarial length prefix can never cause an out-of-bounds
//! slice, an integer overflow, or an unbounded allocation.

use crc::{CRC_32_ISCSI, Crc};

use super::error::DecodeError;

/// On-disk format version for the `CURRENT` marker, snapshot, and WAL
/// headers. Independent of [`FRAMING_PROFILE_VERSION`].
pub const FORMAT_VERSION: u16 = 1;

/// Canonical-serialization/digest recipe version for the framing profile.
///
/// Version 2 also binds the identity-evidence profile so a receiver cannot
/// resume durable state after an unacknowledged fingerprint-window or
/// ignored-header change.
pub const FRAMING_PROFILE_VERSION: u16 = 2;
/// Current raw-prefix fingerprint algorithm/profile.
pub const FINGERPRINT_PROFILE_VERSION: u16 = 1;

/// Magic bytes for the `CURRENT` marker file.
pub const CURRENT_MAGIC: &[u8; 8] = b"FLOGCUR\0";
/// Magic bytes for a snapshot file header.
pub const SNAPSHOT_MAGIC: &[u8; 8] = b"FLOGSNP\0";
/// Magic bytes for a snapshot file footer.
pub const SNAPSHOT_FOOTER_MAGIC: &[u8; 8] = b"FLOGSFT\0";
/// Magic bytes for a WAL file header.
pub const WAL_MAGIC: &[u8; 8] = b"FLOGWAL\0";

/// Maximum stored fingerprint length, in bytes (`u16::MAX`).
pub const FINGERPRINT_MAX_BYTES: usize = u16::MAX as usize;
/// Maximum stored advisory path length, in bytes.
pub const ADVISORY_PATH_MAX_BYTES: usize = 4096;
/// Maximum stored audit-reason length, in bytes.
pub const AUDIT_REASON_MAX_BYTES: usize = 1024;
/// Maximum stored administrative namespace-id length, in bytes.
pub const NAMESPACE_ID_MAX_BYTES: usize = 256;
/// Maximum stored framing-profile regex pattern length, in bytes.
pub const FRAMING_PATTERN_MAX_BYTES: usize = 4096;
/// Maximum number of operations a single WAL transaction may contain.
pub const WAL_MAX_OPS_PER_TX: u16 = 4096;

/// The sole valid `reset_after_truncate` reason in this version: an explicit
/// `on_truncate: read_new` policy decision.
pub const TRUNCATE_RESET_REASON_READ_NEW: u16 = 0x0001;

/// Reserved reason-code value that an encoder MUST NOT produce.
pub const REASON_CODE_RESERVED: u16 = 0x0000;
/// Quarantine reason: recovery evidence existed but could not be inherited
/// unambiguously and the configured policy was `fail`.
pub const QUARANTINE_REASON_RECOVERY_MISMATCH: u16 = 0x0003;

/// `update_metadata` presence bit: a runtime locator value is present.
pub const METADATA_LOCATOR_PRESENT: u8 = 0x01;
/// `update_metadata` presence bit: an advisory path value is present.
pub const METADATA_PATH_PRESENT: u8 = 0x02;
/// All presence bits not yet assigned in v1; MUST be zero.
pub const METADATA_PRESENCE_RESERVED_MASK: u8 = !(METADATA_LOCATOR_PRESENT | METADATA_PATH_PRESENT);

/// Opaque 128-bit durable file identity. Never a UUID-format value by
/// contract; simply 16 bytes of OS randomness assigned once at registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileId(pub [u8; 16]);

impl FileId {
    /// Builds a `FileId` from raw bytes (test/tooling helper).
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }
}

/// Durable lifecycle state discriminant (`lifecycle_state`,
/// `expected_prior_state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleState {
    /// The file is actively tracked and its offset advances on Ack.
    Active,
    /// The file was finalized after a move/create rotation.
    RotatedFinalized,
    /// The file is durably quarantined pending operator action.
    Quarantined,
}

impl LifecycleState {
    pub(super) fn to_wire(self) -> u8 {
        match self {
            LifecycleState::Active => 0x01,
            LifecycleState::RotatedFinalized => 0x02,
            LifecycleState::Quarantined => 0x03,
        }
    }

    pub(super) fn from_wire(value: u8, field: &'static str) -> Result<Self, DecodeError> {
        match value {
            0x01 => Ok(LifecycleState::Active),
            0x02 => Ok(LifecycleState::RotatedFinalized),
            0x03 => Ok(LifecycleState::Quarantined),
            other => Err(DecodeError::UnknownDiscriminant {
                field,
                value: other as u32,
            }),
        }
    }
}

/// Normalized, platform-neutral runtime locator. Never a native `stat` or
/// `FILE_ID_INFO` structure; only the specific integer/byte-array values
/// needed for equality comparison, copied out explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Locator {
    /// No runtime locator recorded (for example, Windows identity
    /// unavailable).
    Unspecified,
    /// Normalized POSIX `(st_dev, st_ino)`, both widened to `u64`.
    PosixDevIno {
        /// Device identifier.
        dev: u64,
        /// Inode number.
        ino: u64,
    },
    /// Normalized Windows `(volume_serial, FILE_ID_INFO.FileId)`.
    WindowsVolumeFileId {
        /// 64-bit `FILE_ID_INFO.VolumeSerialNumber`.
        volume_serial: u64,
        /// 128-bit file identifier, stored verbatim.
        file_id: [u8; 16],
    },
}

impl Locator {
    pub(super) fn write(&self, out: &mut ByteWriter) {
        match *self {
            Locator::Unspecified => out.write_u8(0x00),
            Locator::PosixDevIno { dev, ino } => {
                out.write_u8(0x01);
                out.write_u64(dev);
                out.write_u64(ino);
            }
            Locator::WindowsVolumeFileId {
                volume_serial,
                file_id,
            } => {
                out.write_u8(0x02);
                out.write_u64(volume_serial);
                out.write_bytes(&file_id);
            }
        }
    }

    pub(super) fn read(input: &mut ByteReader<'_>) -> Result<Self, DecodeError> {
        let kind = input.read_u8()?;
        match kind {
            0x00 => Ok(Locator::Unspecified),
            0x01 => {
                let dev = input.read_u64()?;
                let ino = input.read_u64()?;
                Ok(Locator::PosixDevIno { dev, ino })
            }
            0x02 => {
                let volume_serial = input.read_u64()?;
                let mut file_id = [0u8; 16];
                file_id.copy_from_slice(input.read_exact(16)?);
                Ok(Locator::WindowsVolumeFileId {
                    volume_serial,
                    file_id,
                })
            }
            other => Err(DecodeError::UnknownDiscriminant {
                field: "locator.kind",
                value: other as u32,
            }),
        }
    }
}

/// Framing-resume state (INV-FR1): `Clean` or `Continuation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FramingResume {
    /// The next complete source unit starts a new logical record.
    Clean,
    /// A split logical record is in progress.
    Continuation {
        /// Source-byte offset where the original record started.
        record_start_offset: u64,
        /// Index of the next fragment to emit.
        next_fragment_index: u32,
    },
}

impl FramingResume {
    pub(super) fn write(&self, out: &mut ByteWriter) {
        match *self {
            FramingResume::Clean => out.write_u8(0x00),
            FramingResume::Continuation {
                record_start_offset,
                next_fragment_index,
            } => {
                out.write_u8(0x01);
                out.write_u64(record_start_offset);
                out.write_u32(next_fragment_index);
            }
        }
    }

    pub(super) fn read(input: &mut ByteReader<'_>) -> Result<Self, DecodeError> {
        let kind = input.read_u8()?;
        match kind {
            0x00 => Ok(FramingResume::Clean),
            0x01 => {
                let record_start_offset = input.read_u64()?;
                let next_fragment_index = input.read_u32()?;
                Ok(FramingResume::Continuation {
                    record_start_offset,
                    next_fragment_index,
                })
            }
            other => Err(DecodeError::UnknownDiscriminant {
                field: "framing_resume.kind",
                value: other as u32,
            }),
        }
    }
}

/// Computes CRC-32C (Castagnoli), matching the iSCSI CRC-32C parametrization
/// used throughout this format. This is deliberately **not**
/// `crc32fast`'s default IEEE 802.3 polynomial.
#[must_use]
pub fn crc32c(bytes: &[u8]) -> u32 {
    const CASTAGNOLI: Crc<u32> = Crc::<u32>::new(&CRC_32_ISCSI);
    CASTAGNOLI.checksum(bytes)
}

/// A cursor over an input byte slice that performs checked-arithmetic bounds
/// validation on every read. No method ever panics on adversarial input; all
/// failures return [`DecodeError`].
pub struct ByteReader<'a> {
    input: &'a [u8],
    pos: usize,
}

impl<'a> ByteReader<'a> {
    /// Wraps `input` for checked, position-tracked reads from the start.
    #[must_use]
    pub fn new(input: &'a [u8]) -> Self {
        Self { input, pos: 0 }
    }

    /// Number of bytes not yet consumed.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.input.len() - self.pos
    }

    /// Current absolute read position, in bytes from the start of `input`.
    #[must_use]
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Reads exactly `len` bytes, checking `len` against the number of bytes
    /// actually remaining before slicing.
    pub fn read_exact(&mut self, len: usize) -> Result<&'a [u8], DecodeError> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or(DecodeError::ArithmeticOverflow {
                context: "read_exact offset",
            })?;
        if end > self.input.len() {
            return Err(DecodeError::Truncated {
                needed: len,
                available: self.remaining(),
            });
        }
        let slice = &self.input[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    /// Reads one `u8`.
    pub fn read_u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.read_exact(1)?[0])
    }

    /// Reads one big-endian `u16`.
    pub fn read_u16(&mut self) -> Result<u16, DecodeError> {
        let bytes = self.read_exact(2)?;
        Ok(u16::from_be_bytes(
            bytes.try_into().expect("checked length"),
        ))
    }

    /// Reads one big-endian `u32`.
    pub fn read_u32(&mut self) -> Result<u32, DecodeError> {
        let bytes = self.read_exact(4)?;
        Ok(u32::from_be_bytes(
            bytes.try_into().expect("checked length"),
        ))
    }

    /// Reads one big-endian `u64`.
    pub fn read_u64(&mut self) -> Result<u64, DecodeError> {
        let bytes = self.read_exact(8)?;
        Ok(u64::from_be_bytes(
            bytes.try_into().expect("checked length"),
        ))
    }

    /// Reads a `u16`-length-prefixed byte field, checking the declared
    /// length against both `max_len` and the bytes actually remaining
    /// before allocating or slicing.
    pub fn read_var_bytes(
        &mut self,
        field: &'static str,
        max_len: usize,
    ) -> Result<&'a [u8], DecodeError> {
        let len = self.read_u16()? as usize;
        if len > max_len {
            return Err(DecodeError::LengthExceedsMaximum {
                field,
                declared: len,
                max: max_len,
            });
        }
        self.read_exact(len)
    }

    /// Reads a `u16`-length-prefixed UTF-8 string field.
    pub fn read_var_string(
        &mut self,
        field: &'static str,
        max_len: usize,
    ) -> Result<&'a str, DecodeError> {
        let bytes = self.read_var_bytes(field, max_len)?;
        std::str::from_utf8(bytes).map_err(|_| DecodeError::InvalidUtf8 { field })
    }
}

/// An append-only byte buffer used to encode checkpoint records,
/// transactions, and operations.
#[derive(Debug, Default)]
pub struct ByteWriter {
    buf: Vec<u8>,
}

impl ByteWriter {
    /// Creates an empty writer.
    #[must_use]
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Returns the accumulated bytes, consuming the writer.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    /// Borrows the accumulated bytes so far.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    /// Writes raw bytes.
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Writes one `u8`.
    pub fn write_u8(&mut self, value: u8) {
        self.buf.push(value);
    }

    /// Writes one big-endian `u16`.
    pub fn write_u16(&mut self, value: u16) {
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    /// Writes one big-endian `u32`.
    pub fn write_u32(&mut self, value: u32) {
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    /// Writes one big-endian `u64`.
    pub fn write_u64(&mut self, value: u64) {
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    /// Writes a `u16`-length-prefixed byte field, rejecting a value longer
    /// than `max_len` before writing anything.
    pub fn write_var_bytes(
        &mut self,
        field: &'static str,
        bytes: &[u8],
        max_len: usize,
    ) -> Result<(), super::error::EncodeError> {
        if bytes.len() > max_len {
            return Err(super::error::EncodeError::FieldTooLong {
                field,
                len: bytes.len(),
                max: max_len,
            });
        }
        // `max_len` values in this format all fit in u16, so this cast is exact.
        self.write_u16(bytes.len() as u16);
        self.write_bytes(bytes);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: computing CRC-32C over the standard check string.
    /// Guarantees: the codec uses the Castagnoli parametrization (matching
    /// the published `CRC-32C("123456789") = 0xE3069283` reference vector),
    /// not `crc32fast`'s default IEEE 802.3 polynomial.
    #[test]
    fn crc32c_matches_published_check_value() {
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
    }

    /// Scenario: reading a `u32` when fewer than 4 bytes remain.
    /// Guarantees: the reader reports a structured `Truncated` error instead
    /// of panicking or silently zero-filling missing bytes.
    #[test]
    fn byte_reader_reports_truncation_without_panicking() {
        let mut reader = ByteReader::new(&[0x00, 0x01]);
        let err = reader.read_u32().expect_err("only two bytes available");
        assert!(matches!(
            err,
            DecodeError::Truncated {
                needed: 4,
                available: 2
            }
        ));
    }

    /// Scenario: a variable-length field declares a length larger than the
    /// field's documented maximum, even though the bytes are available.
    /// Guarantees: the declared-length check runs before any allocation or
    /// slicing driven by the untrusted length value.
    #[test]
    fn byte_reader_rejects_length_over_field_maximum() {
        let mut bytes = vec![0x00, 0x05];
        bytes.extend_from_slice(&[0u8; 5]);
        let mut reader = ByteReader::new(&bytes);
        let err = reader
            .read_var_bytes("test.field", 3)
            .expect_err("declared length 5 exceeds max 3");
        assert!(matches!(
            err,
            DecodeError::LengthExceedsMaximum {
                field: "test.field",
                declared: 5,
                max: 3,
            }
        ));
    }
}
