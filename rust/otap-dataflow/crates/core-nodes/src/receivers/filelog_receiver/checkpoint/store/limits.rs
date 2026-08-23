// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Checked worst-case size formulas for one checkpoint namespace's durable
//! artifacts.
//!
//! Recovery buffers a whole artifact in memory before it can decode it, so
//! the store needs a size cap for the snapshot and for the WAL. Those caps
//! are deliberately *not* independent knobs: a cap smaller than what the
//! same configuration can legally write would let a namespace become
//! unreadable by the very store that wrote it. Every cap here is therefore
//! derived from the receiver's own configuration -- `compact_after_bytes`,
//! `limits.max_tracked_files`, and `identity.fingerprint_bytes` -- plus the
//! format's own field maximums (advisory path, audit reason, namespace id,
//! operations per transaction), so the write side and the read side are two
//! views of one formula.
//!
//! Every field width below mirrors `docs/filelog-checkpoint-format.md` and
//! the encoders in [`super::super::snapshot`] and [`super::super::wal`]. The
//! unit tests at the bottom encode a genuinely worst-case record and a
//! worst-case operation of every kind and assert the formulas match the
//! bytes the codec actually produces, so a codec change that invalidates a
//! constant here fails the build's tests rather than silently narrowing a
//! bound.
//!
//! All arithmetic is checked. Nothing saturates: a configuration whose
//! worst case is not representable is reported as an error, because a
//! clamped bound would silently claim that an unrepresentable artifact fits.

use super::super::primitives::{
    ADVISORY_PATH_MAX_BYTES, AUDIT_REASON_MAX_BYTES, FINGERPRINT_MAX_BYTES, NAMESPACE_ID_MAX_BYTES,
    WAL_MAX_OPS_PER_TX,
};
use super::super::snapshot::{SNAPSHOT_FOOTER_LEN, SNAPSHOT_HEADER_LEN};
use super::super::wal::WAL_HEADER_LEN;

/// Practical ceiling on the worst case of any single durable checkpoint
/// artifact: one snapshot, or one WAL.
///
/// The ceiling exists because recovery reads a whole artifact into one
/// buffer before decoding it, so this value -- not the filesystem -- bounds
/// each encoded recovery buffer. The separate
/// [`RECOVERY_WORKING_BYTES_CEILING`] covers the sequential snapshot and WAL
/// phases plus decoded state. 1 GiB is generous next to the default
/// configuration (whose snapshot bound is about 50 MiB and whose WAL bound
/// is about 85 MiB), keeps every individual bound representable as a
/// `usize` on a 32-bit target, and prevents a pathological configuration
/// from silently committing the collector to a multi-gigabyte artifact.
pub const ARTIFACT_BYTES_CEILING: u64 = 1024 * 1024 * 1024;
/// Ceiling on the conservative logical working set required to recover one
/// namespace.
///
/// Recovery processes the snapshot before the WAL and applies WAL
/// transactions one at a time, but the raw WAL, recovered table, and one
/// decoded transaction can still overlap. This independent ceiling prevents
/// two individually legal artifacts from multiplying into a multi-gigabyte
/// startup allocation.
pub const RECOVERY_WORKING_BYTES_CEILING: u64 = 1024 * 1024 * 1024;

/// Name used in diagnostics for the snapshot bound.
const SNAPSHOT_ARTIFACT: &str = "checkpoint snapshot";
/// Name used in diagnostics for the WAL bound.
const WAL_ARTIFACT: &str = "checkpoint WAL";
/// Actionable remedy for a snapshot bound that cannot be honored.
const SNAPSHOT_REMEDY: &str = "reduce limits.max_tracked_files or identity.fingerprint_bytes";
/// Actionable remedy for a WAL bound that cannot be honored.
const WAL_REMEDY: &str = "reduce checkpoint.compact_after_bytes or identity.fingerprint_bytes";
/// Actionable remedy for a combined recovery working set that cannot be
/// honored.
const RECOVERY_REMEDY: &str = "reduce limits.max_tracked_files, \
                               checkpoint.compact_after_bytes, or \
                               identity.fingerprint_bytes";

/// `record_len` + `record_crc32c` around a snapshot record payload, and
/// `op_len` + `op_crc32c` around a WAL operation payload.
const FRAME_OVERHEAD_BYTES: u64 = 4 + 4;
/// `tx_len` + `sequence` + `op_count` + `tx_crc32c` around a transaction's
/// operation frames.
const TRANSACTION_OVERHEAD_BYTES: u64 = 4 + 8 + 2 + 4;
/// The `u16` length prefix of one variable-length field.
const VAR_LEN_PREFIX_BYTES: u64 = 2;
/// Widest encoded `locator`: the Windows variant (`kind`, `volume_serial`,
/// 16-byte file id). The POSIX variant is eight bytes shorter.
const LOCATOR_MAX_BYTES: u64 = 1 + 8 + 16;
/// Widest encoded `framing_resume`: the continuation variant (`kind`,
/// `record_start_offset`, `next_fragment_index`).
const FRAMING_RESUME_MAX_BYTES: u64 = 1 + 8 + 4;
/// Quarantine evidence, present only in a quarantined snapshot record:
/// `reason_code`, `observed_size`, `quarantine_epoch`,
/// `quarantine_time_unix_nano`.
const QUARANTINE_EVIDENCE_BYTES: u64 = 2 + 8 + 4 + 8;
/// `op_code` + `file_id`, which every WAL operation payload starts with.
const OPERATION_HEADER_BYTES: u64 = 1 + 16;
/// Conservative multiplier from encoded snapshot bytes to the recovered
/// table's retained heap footprint. It covers table entries, hash buckets,
/// vector headers, and allocator overhead in addition to the variable bytes
/// already represented on disk.
const RECOVERED_TABLE_MULTIPLIER: u64 = 3;
/// Conservative multiplier from one encoded maximal transaction to its
/// decoded operations plus the touched-record scratch map used for atomic
/// application.
const DECODED_TRANSACTION_MULTIPLIER: u64 = 4;

/// Fixed-width part of one snapshot record payload, taking the widest
/// variant of every union-shaped field and including quarantine evidence:
/// `file_id`, `file_epoch`, `committed_offset`, the fingerprint length
/// prefix, `ignored_header_bytes`, `locator`, `framing_profile_version`,
/// `framing_profile_digest`, `framing_resume`, `lifecycle_state`,
/// quarantine evidence, `last_seen_time_unix_nano`, and the advisory-path
/// length prefix.
const SNAPSHOT_RECORD_FIXED_BYTES: u64 = 16
    + 4
    + 8
    + VAR_LEN_PREFIX_BYTES
    + 4
    + LOCATOR_MAX_BYTES
    + 2
    + 32
    + FRAMING_RESUME_MAX_BYTES
    + 1
    + QUARANTINE_EVIDENCE_BYTES
    + 8
    + VAR_LEN_PREFIX_BYTES;

/// `register_file` without its fingerprint and advisory-path bytes.
const REGISTER_FILE_FIXED_BYTES: u64 = OPERATION_HEADER_BYTES
    + 4
    + 8
    + VAR_LEN_PREFIX_BYTES
    + 4
    + LOCATOR_MAX_BYTES
    + 2
    + 32
    + FRAMING_RESUME_MAX_BYTES
    + 8
    + VAR_LEN_PREFIX_BYTES;
/// `update_progress`, which carries no variable-length field.
const UPDATE_PROGRESS_BYTES: u64 =
    OPERATION_HEADER_BYTES + 8 + 4 + 8 + FRAMING_RESUME_MAX_BYTES + 8 + 1;
/// `reset_after_truncate`, which carries no variable-length field.
const RESET_AFTER_TRUNCATE_BYTES: u64 =
    OPERATION_HEADER_BYTES + 4 + 8 + 4 + 8 + FRAMING_RESUME_MAX_BYTES + 8 + 2;
/// `update_fingerprint` without its two fingerprint values.
const UPDATE_FINGERPRINT_FIXED_BYTES: u64 =
    OPERATION_HEADER_BYTES + 4 + VAR_LEN_PREFIX_BYTES + VAR_LEN_PREFIX_BYTES;
/// `update_metadata` with both optional values present and the advisory
/// path at its maximum.
const UPDATE_METADATA_BYTES: u64 = OPERATION_HEADER_BYTES
    + 1
    + LOCATOR_MAX_BYTES
    + 8
    + VAR_LEN_PREFIX_BYTES
    + ADVISORY_PATH_MAX_BYTES as u64;
/// `quarantine_file`, which carries no variable-length field.
const QUARANTINE_FILE_BYTES: u64 = OPERATION_HEADER_BYTES + 4 + 2 + LOCATOR_MAX_BYTES + 8 + 4 + 8;
/// `reset_quarantined_file` with the audit reason at its maximum.
const RESET_QUARANTINED_FILE_BYTES: u64 = OPERATION_HEADER_BYTES
    + 4
    + 1
    + 4
    + 8
    + FRAMING_RESUME_MAX_BYTES
    + 8
    + VAR_LEN_PREFIX_BYTES
    + AUDIT_REASON_MAX_BYTES as u64;
/// Administrative `remove_file` with the namespace id and audit reason at
/// their maximums; the non-administrative form writes two empty fields and
/// is strictly smaller.
const REMOVE_FILE_BYTES: u64 = OPERATION_HEADER_BYTES
    + 4
    + 1
    + 2
    + 8
    + 1
    + VAR_LEN_PREFIX_BYTES
    + NAMESPACE_ID_MAX_BYTES as u64
    + VAR_LEN_PREFIX_BYTES
    + AUDIT_REASON_MAX_BYTES as u64;

/// A configuration whose worst-case durable artifacts cannot be honored.
///
/// These variants are configuration errors, not runtime failures: they are
/// reported by config validation before a namespace is ever opened, and by
/// [`super::CheckpointStore::open`] for options built by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LimitsError {
    /// The worst case is not representable in `u64`.
    #[error("the worst-case {artifact} size for this configuration overflows u64; {remedy}")]
    Overflow {
        /// Which artifact's bound could not be computed.
        artifact: &'static str,
        /// The configuration knob to reduce.
        remedy: &'static str,
    },
    /// The worst case is representable but larger than a checkpoint
    /// artifact may become and still be read back in one buffer.
    #[error(
        "the worst-case {artifact} size for this configuration is {computed} bytes, exceeding \
         the {ceiling}-byte maximum a checkpoint artifact may reach and still be recovered; \
         {remedy}"
    )]
    ExceedsCeiling {
        /// Which artifact's bound is too large.
        artifact: &'static str,
        /// The computed worst case.
        computed: u64,
        /// The ceiling it exceeded.
        ceiling: u64,
        /// The configuration knob to reduce.
        remedy: &'static str,
    },
    /// The conservative combined recovery working set is larger than the
    /// fixed startup-memory ceiling.
    #[error(
        "the worst-case checkpoint recovery working set for this configuration is {computed} \
         bytes, exceeding the {ceiling}-byte maximum; {remedy}"
    )]
    RecoveryExceedsCeiling {
        /// Conservative logical working-set estimate.
        computed: u64,
        /// Ceiling the estimate exceeded.
        ceiling: u64,
        /// Configuration knobs that reduce the estimate.
        remedy: &'static str,
    },
    /// The configured fingerprint window is wider than the format's
    /// `u16`-length-prefixed `fingerprint` field, so it could never be
    /// encoded at all.
    #[error(
        "identity.fingerprint_bytes ({fingerprint_bytes}) exceeds the checkpoint format's \
         {max}-byte fingerprint field maximum"
    )]
    FingerprintUnrepresentable {
        /// The configured fingerprint window.
        fingerprint_bytes: u64,
        /// The format's maximum.
        max: u64,
    },
}

/// The worst-case durable sizes one configuration implies.
///
/// A [`super::CheckpointStore`] both refuses to read an artifact larger
/// than these bounds and refuses to write one, so the two sides can never
/// disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreLimits {
    /// Largest snapshot file this configuration can produce, and therefore
    /// the largest one recovery will read.
    pub max_snapshot_bytes: u64,
    /// Largest WAL file this configuration can produce while compaction is
    /// honored, and therefore the largest one recovery will read.
    pub max_wal_bytes: u64,
    /// Largest single WAL transaction the format allows for this
    /// configuration.
    pub max_transaction_bytes: u64,
    /// Largest single snapshot record this configuration can produce.
    pub max_snapshot_record_bytes: u64,
    /// Conservative maximum logical working set during recovery.
    pub max_recovery_working_bytes: u64,
}

impl StoreLimits {
    /// Derives every bound from the three configuration knobs that govern
    /// durable size, and validates each artifact bound against
    /// [`ARTIFACT_BYTES_CEILING`] and the combined recovery estimate against
    /// [`RECOVERY_WORKING_BYTES_CEILING`].
    pub fn derive(
        compact_after_bytes: u64,
        max_tracked_files: u32,
        fingerprint_bytes: u64,
    ) -> Result<Self, LimitsError> {
        if fingerprint_bytes > FINGERPRINT_MAX_BYTES as u64 {
            return Err(LimitsError::FingerprintUnrepresentable {
                fingerprint_bytes,
                max: FINGERPRINT_MAX_BYTES as u64,
            });
        }
        let max_snapshot_record_bytes = snapshot_record_bytes(fingerprint_bytes)?;
        let max_snapshot_bytes = snapshot_bytes(max_tracked_files, fingerprint_bytes)?;
        let max_transaction_bytes = transaction_bytes(fingerprint_bytes)?;
        let max_wal_bytes = wal_bytes(compact_after_bytes, fingerprint_bytes)?;
        within_ceiling(SNAPSHOT_ARTIFACT, max_snapshot_bytes, SNAPSHOT_REMEDY)?;
        within_ceiling(WAL_ARTIFACT, max_wal_bytes, WAL_REMEDY)?;
        let max_recovery_working_bytes =
            recovery_working_bytes(max_snapshot_bytes, max_wal_bytes, max_transaction_bytes)?;
        if max_recovery_working_bytes > RECOVERY_WORKING_BYTES_CEILING {
            return Err(LimitsError::RecoveryExceedsCeiling {
                computed: max_recovery_working_bytes,
                ceiling: RECOVERY_WORKING_BYTES_CEILING,
                remedy: RECOVERY_REMEDY,
            });
        }
        Ok(Self {
            max_snapshot_bytes,
            max_wal_bytes,
            max_transaction_bytes,
            max_snapshot_record_bytes,
            max_recovery_working_bytes,
        })
    }
}

/// Conservative logical working-set bound for recovery.
///
/// Snapshot parsing can overlap the raw snapshot with decoded records and
/// the table under construction. WAL replay drops the snapshot buffer first
/// and retains only the recovered table, the raw WAL, one decoded
/// transaction, and that transaction's touched-record scratch map.
pub fn recovery_working_bytes(
    max_snapshot_bytes: u64,
    max_wal_bytes: u64,
    max_transaction_bytes: u64,
) -> Result<u64, LimitsError> {
    let snapshot_phase = max_snapshot_bytes
        .checked_mul(RECOVERED_TABLE_MULTIPLIER + 1)
        .ok_or(LimitsError::Overflow {
            artifact: "checkpoint recovery working set",
            remedy: RECOVERY_REMEDY,
        })?;
    let recovered_table = max_snapshot_bytes
        .checked_mul(RECOVERED_TABLE_MULTIPLIER)
        .ok_or(LimitsError::Overflow {
            artifact: "checkpoint recovery working set",
            remedy: RECOVERY_REMEDY,
        })?;
    let decoded_transaction = max_transaction_bytes
        .checked_mul(DECODED_TRANSACTION_MULTIPLIER)
        .ok_or(LimitsError::Overflow {
            artifact: "checkpoint recovery working set",
            remedy: RECOVERY_REMEDY,
        })?;
    let wal_phase = sum(
        "checkpoint recovery working set",
        RECOVERY_REMEDY,
        &[recovered_table, max_wal_bytes, decoded_transaction],
    )?;
    Ok(snapshot_phase.max(wal_phase))
}

/// The largest complete snapshot record frame, including its length prefix
/// and checksum, for a store configured with `fingerprint_bytes`.
pub fn snapshot_record_bytes(fingerprint_bytes: u64) -> Result<u64, LimitsError> {
    sum(
        SNAPSHOT_ARTIFACT,
        SNAPSHOT_REMEDY,
        &[
            SNAPSHOT_RECORD_FIXED_BYTES,
            fingerprint_bytes,
            ADVISORY_PATH_MAX_BYTES as u64,
            FRAME_OVERHEAD_BYTES,
        ],
    )
}

/// The largest complete snapshot file: header, `max_tracked_files`
/// worst-case records, and footer.
pub fn snapshot_bytes(max_tracked_files: u32, fingerprint_bytes: u64) -> Result<u64, LimitsError> {
    let record = snapshot_record_bytes(fingerprint_bytes)?;
    let records =
        record
            .checked_mul(u64::from(max_tracked_files))
            .ok_or(LimitsError::Overflow {
                artifact: SNAPSHOT_ARTIFACT,
                remedy: SNAPSHOT_REMEDY,
            })?;
    sum(
        SNAPSHOT_ARTIFACT,
        SNAPSHOT_REMEDY,
        &[
            SNAPSHOT_HEADER_LEN as u64,
            records,
            SNAPSHOT_FOOTER_LEN as u64,
        ],
    )
}

/// The largest complete WAL operation frame across all eight operations,
/// including its length prefix and checksum.
pub fn operation_bytes(fingerprint_bytes: u64) -> Result<u64, LimitsError> {
    // Which operation is widest depends on the configuration: with a small
    // fingerprint window `register_file` wins on its advisory path, and
    // beyond roughly 4 KiB of fingerprint `update_fingerprint` wins because
    // it carries the expected *and* the replacement value.
    let register_file = sum(
        WAL_ARTIFACT,
        WAL_REMEDY,
        &[
            REGISTER_FILE_FIXED_BYTES,
            fingerprint_bytes,
            ADVISORY_PATH_MAX_BYTES as u64,
        ],
    )?;
    let update_fingerprint = sum(
        WAL_ARTIFACT,
        WAL_REMEDY,
        &[
            UPDATE_FINGERPRINT_FIXED_BYTES,
            fingerprint_bytes,
            fingerprint_bytes,
        ],
    )?;
    let widest = [
        register_file,
        UPDATE_PROGRESS_BYTES,
        RESET_AFTER_TRUNCATE_BYTES,
        update_fingerprint,
        UPDATE_METADATA_BYTES,
        QUARANTINE_FILE_BYTES,
        RESET_QUARANTINED_FILE_BYTES,
        REMOVE_FILE_BYTES,
    ]
    .into_iter()
    .max()
    .unwrap_or(0);
    sum(WAL_ARTIFACT, WAL_REMEDY, &[widest, FRAME_OVERHEAD_BYTES])
}

/// The largest single WAL transaction: the format's maximum number of
/// worst-case operations plus transaction framing.
pub fn transaction_bytes(fingerprint_bytes: u64) -> Result<u64, LimitsError> {
    let operation = operation_bytes(fingerprint_bytes)?;
    let operations =
        operation
            .checked_mul(u64::from(WAL_MAX_OPS_PER_TX))
            .ok_or(LimitsError::Overflow {
                artifact: WAL_ARTIFACT,
                remedy: WAL_REMEDY,
            })?;
    sum(
        WAL_ARTIFACT,
        WAL_REMEDY,
        &[operations, TRANSACTION_OVERHEAD_BYTES],
    )
}

/// The largest WAL file a store that honors its compaction threshold can
/// produce.
///
/// Compaction becomes due once the live WAL reaches `compact_after_bytes`,
/// so the largest WAL a caller can leave behind is one that was still just
/// under the threshold and then took one maximal transaction. The header
/// term additionally covers the degenerate configuration whose threshold is
/// smaller than a freshly created WAL's header, where compaction is due
/// immediately and the first transaction still has to fit.
pub fn wal_bytes(compact_after_bytes: u64, fingerprint_bytes: u64) -> Result<u64, LimitsError> {
    let transaction = transaction_bytes(fingerprint_bytes)?;
    sum(
        WAL_ARTIFACT,
        WAL_REMEDY,
        &[WAL_HEADER_LEN as u64, compact_after_bytes, transaction],
    )
}

fn sum(artifact: &'static str, remedy: &'static str, parts: &[u64]) -> Result<u64, LimitsError> {
    let mut total: u64 = 0;
    for part in parts {
        total = total
            .checked_add(*part)
            .ok_or(LimitsError::Overflow { artifact, remedy })?;
    }
    Ok(total)
}

fn within_ceiling(
    artifact: &'static str,
    computed: u64,
    remedy: &'static str,
) -> Result<(), LimitsError> {
    if computed > ARTIFACT_BYTES_CEILING {
        return Err(LimitsError::ExceedsCeiling {
            artifact,
            computed,
            ceiling: ARTIFACT_BYTES_CEILING,
            remedy,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::receivers::filelog_receiver::checkpoint::primitives::{
        FileId, FramingResume, LifecycleState, Locator,
    };
    use crate::receivers::filelog_receiver::checkpoint::snapshot::{
        QuarantineEvidence, SnapshotRecord, encode_snapshot,
    };
    use crate::receivers::filelog_receiver::checkpoint::wal::{
        Operation, QuarantineFile, RegisterFile, RemoveFile, ResetAfterTruncate,
        ResetQuarantineAction, ResetQuarantinedFile, Transaction, UpdateFingerprint,
        UpdateMetadata, UpdateProgress, encode_wal,
    };

    /// The widest locator, resume state, and evidence the codec can emit,
    /// so a fixture is a genuine worst case rather than a typical value.
    const WIDEST_LOCATOR: Locator = Locator::WindowsVolumeFileId {
        volume_serial: u64::MAX,
        file_id: [0xAB; 16],
    };
    const WIDEST_RESUME: FramingResume = FramingResume::Continuation {
        record_start_offset: u64::MAX,
        next_fragment_index: u32::MAX,
    };

    fn widest_snapshot_record(fingerprint_bytes: u64) -> SnapshotRecord {
        SnapshotRecord {
            file_id: FileId([1; 16]),
            file_epoch: u32::MAX,
            committed_offset: u64::MAX,
            fingerprint: vec![0x5A; fingerprint_bytes as usize],
            ignored_header_bytes: u32::MAX,
            locator: WIDEST_LOCATOR,
            framing_profile_version: 1,
            framing_profile_digest: [0x33; 32],
            framing_resume: WIDEST_RESUME,
            lifecycle_state: LifecycleState::Quarantined,
            quarantine_evidence: Some(QuarantineEvidence {
                reason_code: 0x0001,
                observed_size: u64::MAX,
                quarantine_epoch: u32::MAX,
                quarantine_time_unix_nano: u64::MAX,
            }),
            last_seen_time_unix_nano: u64::MAX,
            advisory_path: vec![b'p'; ADVISORY_PATH_MAX_BYTES],
        }
    }

    fn widest_operations(fingerprint_bytes: u64) -> Vec<Operation> {
        let file_id = FileId([2; 16]);
        let fingerprint = vec![0x5A; fingerprint_bytes as usize];
        vec![
            Operation::RegisterFile(RegisterFile {
                file_id,
                file_epoch: 1,
                committed_offset: 0,
                fingerprint: fingerprint.clone(),
                ignored_header_bytes: u32::MAX,
                locator: WIDEST_LOCATOR,
                framing_profile_version: 1,
                framing_profile_digest: [0x33; 32],
                framing_resume: WIDEST_RESUME,
                last_seen_time_unix_nano: u64::MAX,
                advisory_path: vec![b'p'; ADVISORY_PATH_MAX_BYTES],
            }),
            Operation::UpdateProgress(UpdateProgress {
                file_id,
                expected_committed_offset: 0,
                expected_file_epoch: 1,
                new_committed_offset: u64::MAX,
                new_framing_resume: WIDEST_RESUME,
                new_last_seen_time_unix_nano: u64::MAX,
                finalize: true,
            }),
            Operation::ResetAfterTruncate(ResetAfterTruncate {
                file_id,
                expected_active_epoch: 1,
                observed_truncated_size: u64::MAX,
                resulting_epoch: 2,
                new_committed_offset: 0,
                new_framing_resume: WIDEST_RESUME,
                reset_time_unix_nano: u64::MAX,
                reason_code: 0x0001,
            }),
            Operation::UpdateFingerprint(UpdateFingerprint {
                file_id,
                expected_file_epoch: 1,
                expected_fingerprint: fingerprint.clone(),
                new_fingerprint: fingerprint,
            }),
            Operation::UpdateMetadata(UpdateMetadata {
                file_id,
                locator: Some(WIDEST_LOCATOR),
                last_seen_time_unix_nano: u64::MAX,
                advisory_path: Some(vec![b'p'; ADVISORY_PATH_MAX_BYTES]),
            }),
            Operation::QuarantineFile(QuarantineFile {
                file_id,
                expected_file_epoch: 1,
                reason_code: 0x0001,
                locator: WIDEST_LOCATOR,
                observed_size: u64::MAX,
                quarantine_epoch: 1,
                quarantine_time_unix_nano: u64::MAX,
            }),
            Operation::ResetQuarantinedFile(ResetQuarantinedFile {
                file_id,
                expected_quarantine_epoch: 1,
                action: ResetQuarantineAction::ResetToBeginning,
                resulting_epoch: 2,
                resulting_offset: u64::MAX,
                new_framing_resume: WIDEST_RESUME,
                reset_time_unix_nano: u64::MAX,
                audit_reason: "a".repeat(AUDIT_REASON_MAX_BYTES),
            }),
            Operation::RemoveFile(RemoveFile {
                file_id,
                expected_file_epoch: 1,
                expected_prior_state: LifecycleState::Quarantined,
                removal_reason: 0x0001,
                removal_time_unix_nano: u64::MAX,
                administrative: true,
                namespace_id: Some("n".repeat(NAMESPACE_ID_MAX_BYTES)),
                audit_reason: Some("a".repeat(AUDIT_REASON_MAX_BYTES)),
            }),
        ]
    }

    /// Scenario: the widest snapshot record the codec can emit -- maximum
    /// fingerprint and advisory path, Windows locator, continuation resume
    /// state, and quarantine evidence -- is encoded for several configured
    /// fingerprint windows.
    /// Guarantees: `snapshot_record_bytes` equals the bytes the snapshot
    /// codec actually produces, so the derived snapshot cap can never be
    /// narrower than a record this store may write.
    #[test]
    fn snapshot_record_formula_matches_the_codec() {
        for fingerprint_bytes in [0u64, 16, 1_000, FINGERPRINT_MAX_BYTES as u64] {
            let record = widest_snapshot_record(fingerprint_bytes);
            let encoded = record.encode().expect("the widest record encodes");
            assert_eq!(
                snapshot_record_bytes(fingerprint_bytes).expect("the bound is representable"),
                encoded.len() as u64,
                "record bound disagrees with the codec at {fingerprint_bytes} fingerprint bytes"
            );
        }
    }

    /// Scenario: a complete snapshot file holding worst-case records is
    /// encoded and compared with the file-level formula.
    /// Guarantees: `snapshot_bytes` accounts for the header, every record
    /// frame, and the footer exactly, so a snapshot written at
    /// `max_tracked_files` capacity is still readable at its own cap.
    #[test]
    fn snapshot_file_formula_matches_the_codec() {
        let fingerprint_bytes = 1_000u64;
        let records: Vec<SnapshotRecord> = (0..4u8)
            .map(|index| {
                let mut record = widest_snapshot_record(fingerprint_bytes);
                record.file_id = FileId([index; 16]);
                record
            })
            .collect();
        let encoded = encode_snapshot(7, &records).expect("the snapshot encodes");
        assert_eq!(
            snapshot_bytes(4, fingerprint_bytes).expect("the bound is representable"),
            encoded.len() as u64
        );
    }

    /// Scenario: every one of the eight WAL operations is built at its
    /// widest legal shape and encoded, for a narrow and for a maximal
    /// fingerprint window.
    /// Guarantees: `operation_bytes` equals the widest frame the codec
    /// actually produces in both regimes -- `register_file` dominates for a
    /// narrow window and `update_fingerprint` for a wide one -- so the WAL
    /// cap covers any single operation this store may write.
    #[test]
    fn operation_formula_matches_the_widest_encoded_operation() {
        for fingerprint_bytes in [16u64, 1_000, FINGERPRINT_MAX_BYTES as u64] {
            let widest_encoded = widest_operations(fingerprint_bytes)
                .iter()
                .map(|operation| operation.encode().expect("the operation encodes").len() as u64)
                .max()
                .expect("there are eight operations");
            assert_eq!(
                operation_bytes(fingerprint_bytes).expect("the bound is representable"),
                widest_encoded,
                "operation bound disagrees with the codec at {fingerprint_bytes} fingerprint bytes"
            );
        }
    }

    /// Scenario: a maximal transaction -- the format's per-transaction
    /// operation maximum, every operation at its widest -- is encoded into
    /// a WAL and compared with the transaction and WAL formulas.
    /// Guarantees: `transaction_bytes` covers transaction framing plus the
    /// full operation count, and `wal_bytes` leaves room for exactly one
    /// such transaction on top of the compaction threshold, so honoring the
    /// threshold keeps every WAL readable.
    #[test]
    fn transaction_and_wal_formulas_bound_a_maximal_transaction() {
        let fingerprint_bytes = 16u64;
        let widest = widest_operations(fingerprint_bytes)
            .into_iter()
            .max_by_key(|operation| operation.encode().expect("the operation encodes").len())
            .expect("there are eight operations");
        let operations = vec![widest; WAL_MAX_OPS_PER_TX as usize];
        let transaction = Transaction {
            sequence: 1,
            operations,
        };
        let encoded = transaction.encode().expect("the transaction encodes");
        assert_eq!(
            transaction_bytes(fingerprint_bytes).expect("the bound is representable"),
            encoded.len() as u64
        );

        let compact_after_bytes = 4_096u64;
        let wal = encode_wal(0, &[transaction]).expect("the WAL encodes");
        let bound =
            wal_bytes(compact_after_bytes, fingerprint_bytes).expect("the bound is representable");
        assert_eq!(
            bound,
            WAL_HEADER_LEN as u64 + compact_after_bytes + encoded.len() as u64
        );
        assert!(
            (wal.len() as u64) < bound,
            "a maximal transaction must fit under the WAL bound"
        );
    }

    /// Scenario: snapshot and WAL recovery phases have different synthetic
    /// maxima, and each multiplication/addition is also exercised at
    /// `u64` overflow.
    /// Guarantees: the combined bound is the larger sequential phase, not
    /// their sum, and no intermediate term can wrap into an accepted value.
    #[test]
    fn recovery_working_set_formula_is_phase_aware_and_checked() {
        assert_eq!(
            recovery_working_bytes(10, 20, 5).expect("small bounds are representable"),
            70
        );
        assert!(matches!(
            recovery_working_bytes(u64::MAX, 0, 0),
            Err(LimitsError::Overflow {
                artifact: "checkpoint recovery working set",
                ..
            })
        ));
        assert!(matches!(
            recovery_working_bytes(0, u64::MAX, 1),
            Err(LimitsError::Overflow {
                artifact: "checkpoint recovery working set",
                ..
            })
        ));
    }

    /// Scenario: bounds derived for the shipped defaults, for the widest
    /// legal fingerprint window, and for configurations whose worst case
    /// overflows `u64` or exceeds the artifact ceiling.
    /// Guarantees: the default configuration stays far inside the ceiling,
    /// a representable-but-too-large configuration is rejected with the
    /// knob to reduce, and nothing saturates into a bound that would claim
    /// an unrepresentable artifact fits.
    #[test]
    fn derived_bounds_are_checked_and_ceiling_enforced() {
        let defaults = StoreLimits::derive(64 * 1024 * 1024, 10_000, 1_000)
            .expect("the default configuration is representable");
        assert!(defaults.max_snapshot_bytes <= ARTIFACT_BYTES_CEILING);
        assert!(defaults.max_wal_bytes <= ARTIFACT_BYTES_CEILING);
        assert!(defaults.max_recovery_working_bytes <= RECOVERY_WORKING_BYTES_CEILING);
        assert!(defaults.max_transaction_bytes < defaults.max_wal_bytes);
        assert!(defaults.max_snapshot_record_bytes < defaults.max_snapshot_bytes);
        assert!(
            (49 * 1024 * 1024..=51 * 1024 * 1024).contains(&defaults.max_snapshot_bytes),
            "the documented default snapshot estimate changed: {} bytes",
            defaults.max_snapshot_bytes
        );
        assert!(
            (84 * 1024 * 1024..=85 * 1024 * 1024).contains(&defaults.max_wal_bytes),
            "the documented default WAL estimate changed: {} bytes",
            defaults.max_wal_bytes
        );

        assert!(matches!(
            StoreLimits::derive(64 * 1024 * 1024, 10_000, FINGERPRINT_MAX_BYTES as u64 + 1),
            Err(LimitsError::FingerprintUnrepresentable { .. })
        ));
        assert!(matches!(
            StoreLimits::derive(64 * 1024 * 1024, u32::MAX, 1_000),
            Err(LimitsError::ExceedsCeiling {
                artifact: SNAPSHOT_ARTIFACT,
                ..
            })
        ));
        assert!(matches!(
            StoreLimits::derive(u64::MAX, 10_000, 1_000),
            Err(LimitsError::Overflow {
                artifact: WAL_ARTIFACT,
                ..
            })
        ));
        assert!(matches!(
            StoreLimits::derive(ARTIFACT_BYTES_CEILING, 10_000, 1_000),
            Err(LimitsError::ExceedsCeiling { .. })
                | Err(LimitsError::RecoveryExceedsCeiling { .. })
        ));
        assert!(matches!(
            StoreLimits::derive(64 * 1024 * 1024, 10_000, FINGERPRINT_MAX_BYTES as u64),
            Err(LimitsError::RecoveryExceedsCeiling { .. })
        ));
    }
}
