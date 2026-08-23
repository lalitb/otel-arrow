// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Codec for the version-1 snapshot file format.
//!
//! See `docs/filelog-checkpoint-format.md`, "Snapshot file format", for the
//! exact byte layout this module implements, including the no-torn-tail
//! policy: unlike the WAL, any structurally incomplete snapshot fails
//! recovery closed with no leniency.

use super::error::{DecodeError, EncodeError};
use super::primitives::{
    ADVISORY_PATH_MAX_BYTES, ByteReader, ByteWriter, FINGERPRINT_MAX_BYTES, FileId, FramingResume,
    LifecycleState, Locator, SNAPSHOT_FOOTER_MAGIC, SNAPSHOT_MAGIC, crc32c,
};

/// Fixed width of the snapshot header, in bytes.
pub const SNAPSHOT_HEADER_LEN: usize = 28;
/// Fixed width of the snapshot footer, in bytes.
pub const SNAPSHOT_FOOTER_LEN: usize = 24;

/// Immutable quarantine evidence, present only for a `Quarantined` record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuarantineEvidence {
    /// Opaque diagnostic reason code.
    pub reason_code: u16,
    /// Observed file size at the moment of quarantine.
    pub observed_size: u64,
    /// The epoch value in effect at the moment of quarantine.
    pub quarantine_epoch: u32,
    /// Quarantine timestamp, in Unix nanoseconds.
    pub quarantine_time_unix_nano: u64,
}

/// One decoded snapshot record: the five Appendix B contract groups
/// (identity, progress, framing, lifecycle, advisory metadata) for a single
/// `file_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotRecord {
    /// Durable opaque file identity; the record key.
    pub file_id: FileId,
    /// Current file epoch.
    pub file_epoch: u32,
    /// Committed (Ack'd) source-byte offset.
    pub committed_offset: u64,
    /// Current fingerprint matching evidence.
    pub fingerprint: Vec<u8>,
    /// Number of header bytes ignored when computing the fingerprint.
    pub ignored_header_bytes: u32,
    /// Current (or, if quarantined, immutable quarantine) runtime locator.
    pub locator: Locator,
    /// Framing-profile recipe version stored with this record.
    pub framing_profile_version: u16,
    /// Framing-profile digest stored with this record.
    pub framing_profile_digest: [u8; 32],
    /// Framing-resume state.
    pub framing_resume: FramingResume,
    /// Durable lifecycle state.
    pub lifecycle_state: LifecycleState,
    /// Quarantine evidence; `Some` iff `lifecycle_state == Quarantined`.
    pub quarantine_evidence: Option<QuarantineEvidence>,
    /// Last-seen timestamp, in Unix nanoseconds.
    pub last_seen_time_unix_nano: u64,
    /// Advisory path bytes (matching/diagnostic evidence, not identity).
    pub advisory_path: Vec<u8>,
}

impl SnapshotRecord {
    fn encode_payload(&self) -> Result<Vec<u8>, EncodeError> {
        let mut out = ByteWriter::new();
        out.write_bytes(&self.file_id.0);
        out.write_u32(self.file_epoch);
        out.write_u64(self.committed_offset);
        out.write_var_bytes(
            "snapshot_record.fingerprint",
            &self.fingerprint,
            FINGERPRINT_MAX_BYTES,
        )?;
        out.write_u32(self.ignored_header_bytes);
        self.locator.write(&mut out);
        out.write_u16(self.framing_profile_version);
        out.write_bytes(&self.framing_profile_digest);
        self.framing_resume.write(&mut out);
        out.write_u8(self.lifecycle_state.to_wire());
        match (&self.lifecycle_state, &self.quarantine_evidence) {
            (LifecycleState::Quarantined, Some(evidence)) => {
                out.write_u16(evidence.reason_code);
                out.write_u64(evidence.observed_size);
                out.write_u32(evidence.quarantine_epoch);
                out.write_u64(evidence.quarantine_time_unix_nano);
            }
            (LifecycleState::Quarantined, None) => {
                // The type allows constructing an inconsistent value. This
                // codec never fabricates placeholder evidence or silently
                // emits an ambiguous encoding; it fails closed with a
                // structured error a caller can handle, including in a
                // release build (not just under `debug_assert!`).
                return Err(EncodeError::MissingQuarantineEvidence {
                    file_id: self.file_id,
                });
            }
            (_, None) => {}
            (_, Some(_)) => {
                return Err(EncodeError::UnexpectedQuarantineEvidence {
                    file_id: self.file_id,
                });
            }
        }
        out.write_u64(self.last_seen_time_unix_nano);
        out.write_var_bytes(
            "snapshot_record.advisory_path",
            &self.advisory_path,
            ADVISORY_PATH_MAX_BYTES,
        )?;
        Ok(out.into_bytes())
    }

    /// Encodes this record as a self-delimiting `record_len || payload ||
    /// record_crc32c` frame.
    pub fn encode(&self) -> Result<Vec<u8>, EncodeError> {
        let payload = self.encode_payload()?;
        let mut out = ByteWriter::new();
        // `payload` sizes in this codec never approach `u32::MAX`; the
        // documented field maximums bound it well below that.
        out.write_u32(payload.len() as u32);
        out.write_bytes(&payload);
        let crc = crc32c(out.as_bytes());
        out.write_u32(crc);
        Ok(out.into_bytes())
    }

    fn decode_payload(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut input = ByteReader::new(bytes);
        let mut file_id_bytes = [0u8; 16];
        file_id_bytes.copy_from_slice(input.read_exact(16)?);
        let file_id = FileId(file_id_bytes);
        let file_epoch = input.read_u32()?;
        let committed_offset = input.read_u64()?;
        let fingerprint = input
            .read_var_bytes("snapshot_record.fingerprint", FINGERPRINT_MAX_BYTES)?
            .to_vec();
        let ignored_header_bytes = input.read_u32()?;
        let locator = Locator::read(&mut input)?;
        let framing_profile_version = input.read_u16()?;
        let mut framing_profile_digest = [0u8; 32];
        framing_profile_digest.copy_from_slice(input.read_exact(32)?);
        let framing_resume = FramingResume::read(&mut input)?;
        let lifecycle_state =
            LifecycleState::from_wire(input.read_u8()?, "snapshot_record.lifecycle_state")?;
        let quarantine_evidence = if lifecycle_state == LifecycleState::Quarantined {
            let reason_code = input.read_u16()?;
            let observed_size = input.read_u64()?;
            let quarantine_epoch = input.read_u32()?;
            let quarantine_time_unix_nano = input.read_u64()?;
            Some(QuarantineEvidence {
                reason_code,
                observed_size,
                quarantine_epoch,
                quarantine_time_unix_nano,
            })
        } else {
            None
        };
        let last_seen_time_unix_nano = input.read_u64()?;
        let advisory_path = input
            .read_var_bytes("snapshot_record.advisory_path", ADVISORY_PATH_MAX_BYTES)?
            .to_vec();
        if input.remaining() != 0 {
            return Err(DecodeError::UnconsumedBytes {
                context: "snapshot_record",
                declared: bytes.len(),
                consumed: bytes.len() - input.remaining(),
            });
        }
        Ok(SnapshotRecord {
            file_id,
            file_epoch,
            committed_offset,
            fingerprint,
            ignored_header_bytes,
            locator,
            framing_profile_version,
            framing_profile_digest,
            framing_resume,
            lifecycle_state,
            quarantine_evidence,
            last_seen_time_unix_nano,
            advisory_path,
        })
    }

    /// Decodes one self-delimiting record frame from the front of `input`,
    /// returning the record and the number of bytes consumed from `input`.
    pub fn decode(input: &[u8]) -> Result<(Self, usize), DecodeError> {
        let mut reader = ByteReader::new(input);
        let record_len = reader.read_u32()? as usize;
        let payload = reader.read_exact(record_len)?;
        let stored_crc = reader.read_u32()?;
        let consumed = reader.position();
        let computed_crc = crc32c(&input[0..consumed - 4]);
        if stored_crc != computed_crc {
            return Err(DecodeError::ChecksumMismatch {
                context: "snapshot_record",
                expected: stored_crc,
                computed: computed_crc,
            });
        }
        let record = Self::decode_payload(payload)?;
        Ok((record, consumed))
    }
}

/// Encodes a complete snapshot file (header, records, footer) for the given
/// `generation`.
pub fn encode_snapshot(
    generation: u64,
    records: &[SnapshotRecord],
) -> Result<Vec<u8>, EncodeError> {
    // `file_id` is the record key: a snapshot with two records sharing a
    // `file_id` is not a well-formed table and must never be written, not
    // just rejected on the way back in.
    let mut seen_file_ids = std::collections::HashSet::with_capacity(records.len());
    for record in records {
        if !seen_file_ids.insert(record.file_id) {
            return Err(EncodeError::DuplicateFileId {
                file_id: record.file_id,
            });
        }
    }

    let mut out = ByteWriter::new();
    out.write_bytes(SNAPSHOT_MAGIC);
    out.write_u16(super::primitives::FORMAT_VERSION);
    out.write_u16(0); // flags, reserved
    out.write_u64(generation);
    // Record counts in practice stay well below u32::MAX (bounded by
    // configured max_tracked_files in a later stage); this cast is exact for
    // any input this codec is expected to see.
    out.write_u32(records.len() as u32);
    let header_crc = crc32c(out.as_bytes());
    out.write_u32(header_crc);

    let mut total_record_bytes: u64 = 0;
    for record in records {
        let frame = record.encode()?;
        total_record_bytes += frame.len() as u64;
        out.write_bytes(&frame);
    }

    out.write_bytes(SNAPSHOT_FOOTER_MAGIC);
    out.write_u64(total_record_bytes);
    out.write_u32(records.len() as u32);
    let footer_start = out.as_bytes().len() - 20;
    let footer_crc = crc32c(&out.as_bytes()[footer_start..]);
    out.write_u32(footer_crc);

    Ok(out.into_bytes())
}

/// The fully decoded contents of one snapshot file. Mirrors
/// [`super::wal::WalContents`] so a later stage (Stage 4) can cross-check
/// the `CURRENT` marker's generation, this snapshot's own `generation`, and
/// the WAL's `generation` against each other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotContents {
    /// The generation number recorded in the snapshot header.
    pub generation: u64,
    /// Every decoded record, in on-disk order.
    pub records: Vec<SnapshotRecord>,
}

/// Decodes and fully validates a complete snapshot file, returning its
/// generation and records in on-disk order.
///
/// Fails closed (with no torn-tail leniency) on any header, record, or
/// footer inconsistency, including trailing bytes after a structurally
/// complete footer.
pub fn decode_snapshot(bytes: &[u8]) -> Result<SnapshotContents, DecodeError> {
    if bytes.len() < SNAPSHOT_HEADER_LEN {
        return Err(DecodeError::Truncated {
            needed: SNAPSHOT_HEADER_LEN,
            available: bytes.len(),
        });
    }
    let mut reader = ByteReader::new(bytes);
    let magic = reader.read_exact(8)?;
    if magic != SNAPSHOT_MAGIC {
        return Err(DecodeError::BadMagic {
            context: "snapshot header",
        });
    }
    let format_version = reader.read_u16()?;
    if format_version != super::primitives::FORMAT_VERSION {
        return Err(DecodeError::UnsupportedFormatVersion {
            context: "snapshot header",
            found: format_version,
        });
    }
    let flags = reader.read_u16()?;
    if flags != 0 {
        return Err(DecodeError::ReservedFieldNonZero {
            field: "snapshot_header.flags",
            value: flags as u64,
        });
    }
    let generation = reader.read_u64()?;
    let record_count = reader.read_u32()?;
    let stored_header_crc = reader.read_u32()?;
    let computed_header_crc = crc32c(&bytes[0..24]);
    if stored_header_crc != computed_header_crc {
        return Err(DecodeError::ChecksumMismatch {
            context: "snapshot_header",
            expected: stored_header_crc,
            computed: computed_header_crc,
        });
    }

    let mut records = Vec::with_capacity(record_count.min(1 << 16) as usize);
    let mut seen_file_ids =
        std::collections::HashSet::with_capacity(record_count.min(1 << 16) as usize);
    let mut cursor = SNAPSHOT_HEADER_LEN;
    let mut total_record_bytes: u64 = 0;
    for _ in 0..record_count {
        let remaining = bytes.get(cursor..).ok_or(DecodeError::Truncated {
            needed: 1,
            available: 0,
        })?;
        let (record, consumed) = SnapshotRecord::decode(remaining)?;
        if !seen_file_ids.insert(record.file_id) {
            return Err(DecodeError::DuplicateFileId {
                file_id: record.file_id,
                context: "snapshot",
            });
        }
        cursor += consumed;
        total_record_bytes += consumed as u64;
        records.push(record);
    }

    let footer_bytes = bytes.get(cursor..).ok_or(DecodeError::Truncated {
        needed: SNAPSHOT_FOOTER_LEN,
        available: 0,
    })?;
    if footer_bytes.len() < SNAPSHOT_FOOTER_LEN {
        return Err(DecodeError::Truncated {
            needed: SNAPSHOT_FOOTER_LEN,
            available: footer_bytes.len(),
        });
    }
    let mut footer_reader = ByteReader::new(footer_bytes);
    let footer_magic = footer_reader.read_exact(8)?;
    if footer_magic != SNAPSHOT_FOOTER_MAGIC {
        return Err(DecodeError::BadMagic {
            context: "snapshot footer",
        });
    }
    let stored_total_record_bytes = footer_reader.read_u64()?;
    let stored_record_count_echo = footer_reader.read_u32()?;
    let stored_footer_crc = footer_reader.read_u32()?;
    let computed_footer_crc = crc32c(&footer_bytes[0..20]);
    if stored_footer_crc != computed_footer_crc {
        return Err(DecodeError::ChecksumMismatch {
            context: "snapshot_footer",
            expected: stored_footer_crc,
            computed: computed_footer_crc,
        });
    }
    if stored_record_count_echo != record_count {
        return Err(DecodeError::UnconsumedBytes {
            context: "snapshot_footer.record_count_echo",
            declared: record_count as usize,
            consumed: stored_record_count_echo as usize,
        });
    }
    if stored_total_record_bytes != total_record_bytes {
        return Err(DecodeError::UnconsumedBytes {
            context: "snapshot_footer.total_record_bytes",
            declared: stored_total_record_bytes as usize,
            consumed: total_record_bytes as usize,
        });
    }

    let trailing = bytes.len() - (cursor + SNAPSHOT_FOOTER_LEN);
    if trailing != 0 {
        return Err(DecodeError::TrailingBytes {
            context: "snapshot file",
            remaining: trailing,
        });
    }

    Ok(SnapshotContents {
        generation,
        records,
    })
}
