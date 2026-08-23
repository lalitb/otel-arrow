// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Codec for the fixed-width binary `CURRENT` marker.
//!
//! See `docs/filelog-checkpoint-format.md`, "The `CURRENT` marker" section,
//! for the exact 24-byte layout this module implements.

use super::error::DecodeError;
use super::primitives::{ByteReader, ByteWriter, CURRENT_MAGIC, FORMAT_VERSION, crc32c};

/// Total encoded width of the `CURRENT` marker, in bytes.
pub const CURRENT_MARKER_LEN: usize = 24;

/// Encodes a `CURRENT` marker selecting `generation`.
#[must_use]
pub fn encode_current_marker(generation: u64) -> Vec<u8> {
    let mut out = ByteWriter::new();
    out.write_bytes(CURRENT_MAGIC);
    out.write_u16(FORMAT_VERSION);
    out.write_u16(0); // flags, reserved
    out.write_u64(generation);
    let crc = crc32c(out.as_bytes());
    out.write_u32(crc);
    let bytes = out.into_bytes();
    debug_assert_eq!(bytes.len(), CURRENT_MARKER_LEN);
    bytes
}

/// Decodes and validates a `CURRENT` marker, returning the selected
/// generation number.
///
/// Fails closed on any length mismatch, bad magic, unsupported
/// `format_version`, nonzero reserved `flags`, or CRC-32C mismatch. There is
/// no torn-tail leniency here: the marker is written and synced as one small
/// atomic replacement, never appended to.
pub fn decode_current_marker(bytes: &[u8]) -> Result<u64, DecodeError> {
    if bytes.len() != CURRENT_MARKER_LEN {
        return Err(DecodeError::Truncated {
            needed: CURRENT_MARKER_LEN,
            available: bytes.len(),
        });
    }
    let mut reader = ByteReader::new(bytes);
    let magic = reader.read_exact(8)?;
    if magic != CURRENT_MAGIC {
        return Err(DecodeError::BadMagic {
            context: "CURRENT marker",
        });
    }
    let format_version = reader.read_u16()?;
    if format_version != FORMAT_VERSION {
        return Err(DecodeError::UnsupportedFormatVersion {
            context: "CURRENT marker",
            found: format_version,
        });
    }
    let flags = reader.read_u16()?;
    if flags != 0 {
        return Err(DecodeError::ReservedFieldNonZero {
            field: "CURRENT.flags",
            value: flags as u64,
        });
    }
    let generation = reader.read_u64()?;
    let stored_crc = reader.read_u32()?;
    let computed_crc = crc32c(&bytes[0..20]);
    if stored_crc != computed_crc {
        return Err(DecodeError::ChecksumMismatch {
            context: "CURRENT marker",
            expected: stored_crc,
            computed: computed_crc,
        });
    }
    Ok(generation)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: encoding then decoding a `CURRENT` marker for a chosen
    /// generation number.
    /// Guarantees: round-tripping through the codec preserves the exact
    /// generation value and produces exactly `CURRENT_MARKER_LEN` bytes.
    #[test]
    fn round_trips_generation_number() {
        let bytes = encode_current_marker(42);
        assert_eq!(bytes.len(), CURRENT_MARKER_LEN);
        assert_eq!(decode_current_marker(&bytes).unwrap(), 42);
    }

    /// Scenario: a `CURRENT` marker whose trailing CRC-32C byte has been
    /// flipped, leaving the length and magic untouched.
    /// Guarantees: decoding fails closed with a checksum mismatch rather
    /// than returning a plausible-looking but unverified generation number.
    #[test]
    fn corrupt_checksum_fails_closed() {
        let mut bytes = encode_current_marker(7);
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        assert!(matches!(
            decode_current_marker(&bytes),
            Err(DecodeError::ChecksumMismatch { .. })
        ));
    }

    /// Scenario: a `CURRENT` marker whose `format_version` field has been
    /// bumped to a value this codec does not recognize.
    /// Guarantees: decoding fails closed with a distinct
    /// `UnsupportedFormatVersion` error rather than misinterpreting the
    /// remaining bytes under the current version's layout.
    #[test]
    fn unsupported_version_fails_closed() {
        let mut bytes = encode_current_marker(1);
        bytes[8..10].copy_from_slice(&2u16.to_be_bytes());
        assert!(matches!(
            decode_current_marker(&bytes),
            Err(DecodeError::UnsupportedFormatVersion { found: 2, .. })
        ));
    }

    /// Scenario: a byte buffer that is not exactly `CURRENT_MARKER_LEN`
    /// (24) bytes long, both shorter and longer than the fixed width.
    /// Guarantees: decoding fails closed with `Truncated` rather than
    /// reading past the end of a short buffer or silently ignoring extra
    /// bytes in a long one; the `CURRENT` marker has no self-delimiting
    /// length field of its own, so the exact-length check is the only
    /// thing preventing either failure mode.
    #[test]
    fn wrong_size_fails_closed() {
        let short = vec![0u8; CURRENT_MARKER_LEN - 1];
        assert!(matches!(
            decode_current_marker(&short),
            Err(DecodeError::Truncated {
                needed: CURRENT_MARKER_LEN,
                ..
            })
        ));

        let mut long = encode_current_marker(1);
        long.push(0x00);
        assert!(matches!(
            decode_current_marker(&long),
            Err(DecodeError::Truncated {
                needed: CURRENT_MARKER_LEN,
                ..
            })
        ));
    }

    /// Scenario: a `CURRENT` marker whose leading `magic` bytes have been
    /// overwritten with an unrelated value, leaving the length and every
    /// other field (including the CRC, recomputed over the corrupted magic)
    /// intact.
    /// Guarantees: decoding fails closed with `BadMagic`, distinguishing
    /// "this is not a `CURRENT` marker at all" from a checksum-covered
    /// corruption of an otherwise-recognized marker.
    #[test]
    fn bad_magic_fails_closed() {
        let mut bytes = encode_current_marker(1);
        bytes[0..8].copy_from_slice(b"NOTFLOG\0");
        let crc = crc32c(&bytes[0..20]);
        bytes[20..24].copy_from_slice(&crc.to_be_bytes());
        assert!(matches!(
            decode_current_marker(&bytes),
            Err(DecodeError::BadMagic { .. })
        ));
    }

    /// Scenario: a `CURRENT` marker whose reserved `flags` field has been
    /// set to a nonzero value, with the CRC recomputed so the corruption is
    /// isolated to `flags` alone.
    /// Guarantees: decoding fails closed with `ReservedFieldNonZero`; v1
    /// defines no flag bits, so any nonzero value is rejected rather than
    /// silently ignored.
    #[test]
    fn nonzero_flags_fails_closed() {
        let mut bytes = encode_current_marker(1);
        bytes[10..12].copy_from_slice(&1u16.to_be_bytes());
        let crc = crc32c(&bytes[0..20]);
        bytes[20..24].copy_from_slice(&crc.to_be_bytes());
        assert!(matches!(
            decode_current_marker(&bytes),
            Err(DecodeError::ReservedFieldNonZero {
                field: "CURRENT.flags",
                ..
            })
        ));
    }
}
