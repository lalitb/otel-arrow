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
    ADVISORY_PATH_FIXED_BYTES, ADVISORY_PATH_STORED_MAX_BYTES, AUDIT_REASON_MAX_BYTES,
    COMMITTED_FRONTIER_GUARD_LEN, FINGERPRINT_MAX_BYTES, NAMESPACE_ID_MAX_BYTES,
    TX_FRAME_CRC_BYTES, TX_HEADER_BYTES, WAL_MAX_NON_PROGRESS_OPS_PER_TX, WAL_MAX_OPS_PER_TX,
    WAL_MAX_TX_BODY_BYTES,
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
/// The fixed 36-byte transaction envelope header plus the trailing 4-byte
/// `frame_crc32c` around a transaction's operation frames. `sequence` and
/// `op_count` live inside the fixed header in this version, not as
/// additional body-prefix fields.
const TRANSACTION_OVERHEAD_BYTES: u64 = TX_HEADER_BYTES as u64 + TX_FRAME_CRC_BYTES as u64;
/// The `u16` length prefix of one variable-length field.
const VAR_LEN_PREFIX_BYTES: u64 = 2;
/// Widest encoded `locator`: the Windows variant (`kind`, `volume_serial`,
/// 16-byte file id). The POSIX variant is eight bytes shorter.
const LOCATOR_MAX_BYTES: u64 = 1 + 8 + 16;
/// Widest encoded `framing_resume`: the continuation variant (`kind`,
/// `record_start_offset`, `record_end_offset`, `next_fragment_index`).
const FRAMING_RESUME_MAX_BYTES: u64 = 1 + 8 + 8 + 4;
/// Encoded width of `framing_resume` when it is required to be `Clean`
/// (`register_file`'s initial framing-resume state): the one-byte `kind`
/// discriminant alone.
const FRAMING_RESUME_CLEAN_BYTES: u64 = 1;
/// Fixed encoded width of a `CommittedFrontierGuard`: `window_len: u16`
/// plus a 32-byte digest.
const COMMITTED_FRONTIER_GUARD_BYTES: u64 = COMMITTED_FRONTIER_GUARD_LEN as u64;
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
/// `file_id`, `file_epoch`, `committed_offset`, the committed-frontier
/// guard, the fingerprint length prefix, `ignored_header_bytes`, `locator`,
/// `framing_profile_version`, `framing_profile_digest`, `framing_resume`,
/// `lifecycle_state`, quarantine evidence, `last_seen_time_unix_nano`, and
/// the advisory path's fixed encoded overhead (`ADVISORY_PATH_FIXED_BYTES`;
/// `stored_path_bytes` is added separately by the caller).
const SNAPSHOT_RECORD_FIXED_BYTES: u64 = 16
    + 4
    + 8
    + COMMITTED_FRONTIER_GUARD_BYTES
    + VAR_LEN_PREFIX_BYTES
    + 4
    + LOCATOR_MAX_BYTES
    + 2
    + 32
    + FRAMING_RESUME_MAX_BYTES
    + 1
    + QUARANTINE_EVIDENCE_BYTES
    + 8
    + ADVISORY_PATH_FIXED_BYTES as u64;

/// `register_file` without its fingerprint and advisory-path stored bytes.
/// `framing_resume` is required to be `Clean` at registration time, so this
/// uses its one-byte encoding rather than the generic widest variant.
const REGISTER_FILE_FIXED_BYTES: u64 = OPERATION_HEADER_BYTES
    + 4
    + 8
    + COMMITTED_FRONTIER_GUARD_BYTES
    + VAR_LEN_PREFIX_BYTES
    + 4
    + LOCATOR_MAX_BYTES
    + 2
    + 32
    + FRAMING_RESUME_CLEAN_BYTES
    + 8
    + ADVISORY_PATH_FIXED_BYTES as u64;
/// `update_progress`, which carries no variable-length field.
const UPDATE_PROGRESS_BYTES: u64 = OPERATION_HEADER_BYTES
    + 8
    + 4
    + 8
    + COMMITTED_FRONTIER_GUARD_BYTES
    + FRAMING_RESUME_MAX_BYTES
    + 8
    + 1;
/// `reset_after_truncate` without its replacement fingerprint. The resulting
/// guard is always the format-defined empty guard and is not carried.
const RESET_AFTER_TRUNCATE_BYTES: u64 = OPERATION_HEADER_BYTES
    + 4
    + 8
    + 4
    + 8
    + FRAMING_RESUME_MAX_BYTES
    + VAR_LEN_PREFIX_BYTES
    + 8
    + 2;
/// `update_fingerprint` without its two fingerprint values.
const UPDATE_FINGERPRINT_FIXED_BYTES: u64 =
    OPERATION_HEADER_BYTES + 4 + VAR_LEN_PREFIX_BYTES + VAR_LEN_PREFIX_BYTES;
/// `update_metadata` with the advisory path present at its maximum
/// (truncated) size. Never carries a locator: the locator is immutable for
/// a `file_id`.
const UPDATE_METADATA_BYTES: u64 = OPERATION_HEADER_BYTES
    + 1
    + 4
    + 1
    + 8
    + ADVISORY_PATH_FIXED_BYTES as u64
    + ADVISORY_PATH_STORED_MAX_BYTES as u64;
/// `quarantine_file`, which carries no variable-length field.
const QUARANTINE_FILE_BYTES: u64 = OPERATION_HEADER_BYTES + 4 + 2 + LOCATOR_MAX_BYTES + 8 + 4 + 8;
/// `reset_quarantined_file` without its replacement fingerprint, with
/// namespace ID and audit reason at their maxima.
const RESET_QUARANTINED_FILE_BYTES: u64 = OPERATION_HEADER_BYTES
    + 4
    + 1
    + 4
    + 8
    + COMMITTED_FRONTIER_GUARD_BYTES
    + FRAMING_RESUME_MAX_BYTES
    + VAR_LEN_PREFIX_BYTES
    + 8
    + VAR_LEN_PREFIX_BYTES
    + NAMESPACE_ID_MAX_BYTES as u64
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
            ADVISORY_PATH_STORED_MAX_BYTES as u64,
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

/// The largest complete WAL operation frame across all seven non-progress
/// operations, including its length prefix and checksum.
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
            ADVISORY_PATH_STORED_MAX_BYTES as u64,
        ],
    )?;
    let update_fingerprint = sum(
        WAL_ARTIFACT,
        WAL_REMEDY,
        &[
            UPDATE_FINGERPRINT_FIXED_BYTES,
            fingerprint_bytes.saturating_sub(1),
            fingerprint_bytes,
        ],
    )?;
    let reset_after_truncate = sum(
        WAL_ARTIFACT,
        WAL_REMEDY,
        &[RESET_AFTER_TRUNCATE_BYTES, fingerprint_bytes],
    )?;
    let reset_quarantined_file = sum(
        WAL_ARTIFACT,
        WAL_REMEDY,
        &[RESET_QUARANTINED_FILE_BYTES, fingerprint_bytes],
    )?;
    let widest = [
        register_file,
        reset_after_truncate,
        update_fingerprint,
        UPDATE_METADATA_BYTES,
        QUARANTINE_FILE_BYTES,
        reset_quarantined_file,
        REMOVE_FILE_BYTES,
    ]
    .into_iter()
    .max()
    .unwrap_or(0);
    sum(WAL_ARTIFACT, WAL_REMEDY, &[widest, FRAME_OVERHEAD_BYTES])
}

/// The largest complete `update_progress` operation frame, the only
/// operation kind a progress-only transaction may contain.
pub fn progress_operation_bytes() -> Result<u64, LimitsError> {
    sum(
        WAL_ARTIFACT,
        WAL_REMEDY,
        &[UPDATE_PROGRESS_BYTES, FRAME_OVERHEAD_BYTES],
    )
}

/// The largest single WAL transaction across both transaction classes: the
/// non-progress class (up to `WAL_MAX_NON_PROGRESS_OPS_PER_TX` worst-case
/// non-progress operations) and the progress-only class (up to
/// `WAL_MAX_OPS_PER_TX` worst-case `update_progress` operations), each
/// additionally capped at the hard `WAL_MAX_TX_BODY_BYTES` (16 MiB) body
/// limit every transaction class is subject to before allocation.
pub fn transaction_bytes(fingerprint_bytes: u64) -> Result<u64, LimitsError> {
    let non_progress_operation = operation_bytes(fingerprint_bytes)?;
    let non_progress_body = non_progress_operation
        .checked_mul(u64::from(WAL_MAX_NON_PROGRESS_OPS_PER_TX))
        .ok_or(LimitsError::Overflow {
            artifact: WAL_ARTIFACT,
            remedy: WAL_REMEDY,
        })?
        .min(WAL_MAX_TX_BODY_BYTES);

    let progress_operation = progress_operation_bytes()?;
    let progress_body = progress_operation
        .checked_mul(u64::from(WAL_MAX_OPS_PER_TX))
        .ok_or(LimitsError::Overflow {
            artifact: WAL_ARTIFACT,
            remedy: WAL_REMEDY,
        })?
        .min(WAL_MAX_TX_BODY_BYTES);

    let widest_body = non_progress_body.max(progress_body);
    sum(
        WAL_ARTIFACT,
        WAL_REMEDY,
        &[widest_body, TRANSACTION_OVERHEAD_BYTES],
    )
}

/// The largest WAL file a store that honors its compaction threshold can
/// produce.
///
/// Compaction becomes byte-due once the live WAL body reaches
/// `compact_after_bytes`, so the largest WAL a caller can leave behind is
/// one whose body was still just under the threshold and then took one
/// maximal transaction. The fixed header is accounted separately and never
/// makes an empty WAL due.
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
        AdvisoryPath, CommittedFrontierGuard, FileId, FramingResume, LifecycleState, Locator,
        MAX_OPERATION_FRAME_BYTES, MAX_OPERATION_PAYLOAD_BYTES, MAX_PROGRESS_TX_BODY_BYTES,
        MAX_PROGRESS_TX_FRAME_BYTES, MAX_VALID_UPDATE_FINGERPRINT_FRAME_BYTES,
        MAX_VALID_UPDATE_FINGERPRINT_PAYLOAD_BYTES, REGISTER_FILE_MAX_OP_PAYLOAD_BYTES,
        SNAPSHOT_MAX_RECORD_FRAME_BYTES, SNAPSHOT_MAX_RECORD_PAYLOAD_BYTES, TX_MIN_BODY_BYTES,
        TX_MIN_FRAME_BYTES, UPDATE_PROGRESS_MAX_OP_FRAME_BYTES,
        UPDATE_PROGRESS_MAX_OP_PAYLOAD_BYTES, WAL_MAX_TX_FRAME_BYTES,
    };
    use crate::receivers::filelog_receiver::checkpoint::snapshot::{
        QuarantineEvidence, SnapshotRecord, encode_snapshot,
    };
    use crate::receivers::filelog_receiver::checkpoint::wal::{
        Operation, QuarantineFile, RegisterFile, RemoveFile, ResetAfterTruncate,
        ResetQuarantineAction, ResetQuarantinedFile, Transaction, UpdateFingerprint,
        UpdateMetadata, UpdateProgress, encode_wal,
    };

    /// The test namespace all fixtures in this module encode under.
    const TEST_NAMESPACE_ID: &str = "limits-test-namespace";

    /// The widest locator, resume state, and evidence the codec can emit,
    /// so a fixture is a genuine worst case rather than a typical value.
    /// `record_start_offset` is `u64::MAX - 1` (rather than `u64::MAX`) so
    /// it remains strictly less than a `committed_offset` of `u64::MAX`,
    /// satisfying the reachable-state invariant `SnapshotRecord::encode`
    /// enforces.
    const WIDEST_LOCATOR: Locator = Locator::WindowsVolumeFileId {
        volume_serial: u64::MAX,
        file_id: [0xAB; 16],
    };
    const WIDEST_RESUME: FramingResume = FramingResume::Continuation {
        record_start_offset: u64::MAX - 1,
        record_end_offset: 0,
        next_fragment_index: u32::MAX,
    };

    /// The widest encoded `CommittedFrontierGuard`: 34 bytes regardless of
    /// the offset value, paired with `committed_offset == u64::MAX` (whose
    /// required window is the full 64 bytes).
    fn widest_guard() -> CommittedFrontierGuard {
        CommittedFrontierGuard::compute(u64::MAX, &[0x5A; 64]).unwrap()
    }

    fn widest_snapshot_record(fingerprint_bytes: u64) -> SnapshotRecord {
        SnapshotRecord {
            file_id: FileId([1; 16]),
            file_epoch: u32::MAX,
            committed_offset: u64::MAX,
            committed_frontier_guard: widest_guard(),
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
            advisory_path: AdvisoryPath::from_unix_bytes(&vec![
                b'p';
                ADVISORY_PATH_STORED_MAX_BYTES
            ])
            .unwrap(),
        }
    }

    fn widest_operations(fingerprint_bytes: u64) -> Vec<Operation> {
        let file_id = FileId([2; 16]);
        let fingerprint = vec![0x5A; fingerprint_bytes as usize];
        let expected_fingerprint = fingerprint[..fingerprint.len().saturating_sub(1)].to_vec();
        vec![
            Operation::RegisterFile(RegisterFile {
                file_id,
                file_epoch: 1,
                committed_offset: 0,
                committed_frontier_guard: CommittedFrontierGuard::empty(),
                fingerprint: fingerprint.clone(),
                ignored_header_bytes: u32::MAX,
                locator: WIDEST_LOCATOR,
                framing_profile_version: 1,
                framing_profile_digest: [0x33; 32],
                framing_resume: FramingResume::Clean,
                last_seen_time_unix_nano: u64::MAX,
                advisory_path: AdvisoryPath::from_unix_bytes(&vec![
                    b'p';
                    ADVISORY_PATH_STORED_MAX_BYTES
                ])
                .unwrap(),
            }),
            Operation::UpdateProgress(UpdateProgress {
                file_id,
                expected_committed_offset: 0,
                expected_file_epoch: 1,
                new_committed_offset: u64::MAX,
                new_committed_frontier_guard: widest_guard(),
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
                new_fingerprint: fingerprint.clone(),
                reset_time_unix_nano: u64::MAX,
                reason_code: 0x0001,
            }),
            Operation::UpdateFingerprint(UpdateFingerprint {
                file_id,
                expected_file_epoch: 1,
                expected_fingerprint,
                new_fingerprint: fingerprint.clone(),
            }),
            Operation::UpdateMetadata(UpdateMetadata {
                file_id,
                expected_prior_state: LifecycleState::Quarantined,
                expected_file_epoch: 1,
                last_seen_time_unix_nano: u64::MAX,
                advisory_path: Some(
                    AdvisoryPath::from_unix_bytes(&vec![b'p'; ADVISORY_PATH_STORED_MAX_BYTES])
                        .unwrap(),
                ),
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
                new_committed_frontier_guard: widest_guard(),
                new_framing_resume: WIDEST_RESUME,
                new_fingerprint: fingerprint,
                action_time_unix_nano: u64::MAX,
                namespace_id: "n".repeat(NAMESPACE_ID_MAX_BYTES),
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
                record.locator = Locator::WindowsVolumeFileId {
                    volume_serial: u64::from(index),
                    file_id: [0xAB; 16],
                };
                record
            })
            .collect();
        let encoded =
            encode_snapshot(7, TEST_NAMESPACE_ID, &records).expect("the snapshot encodes");
        assert_eq!(
            snapshot_bytes(4, fingerprint_bytes).expect("the bound is representable"),
            encoded.len() as u64
        );
    }

    /// Scenario: every one of the seven non-progress WAL operations is
    /// built at its widest legal shape and encoded, for a narrow and for a
    /// maximal fingerprint window.
    /// Guarantees: `operation_bytes` equals the widest non-progress frame
    /// the codec actually produces in both regimes -- `register_file`
    /// dominates for a narrow window and `update_fingerprint` for a wide
    /// one -- so the non-progress transaction cap covers any single
    /// non-progress operation this store may write.
    #[test]
    fn operation_formula_matches_the_widest_encoded_operation() {
        for fingerprint_bytes in [16u64, 1_000, FINGERPRINT_MAX_BYTES as u64] {
            let widest_encoded = widest_operations(fingerprint_bytes)
                .iter()
                .filter(|operation| !matches!(operation, Operation::UpdateProgress(_)))
                .map(|operation| operation.encode().expect("the operation encodes").len() as u64)
                .max()
                .expect("there are seven non-progress operations");
            assert_eq!(
                operation_bytes(fingerprint_bytes).expect("the bound is representable"),
                widest_encoded,
                "operation bound disagrees with the codec at {fingerprint_bytes} fingerprint bytes"
            );
        }
    }

    /// Scenario: a maximal progress-only transaction (`WAL_MAX_OPS_PER_TX`
    /// widest `update_progress` operations) and a maximal non-progress
    /// transaction (`WAL_MAX_NON_PROGRESS_OPS_PER_TX` widest non-progress
    /// operations of one kind) are each encoded into a WAL and compared
    /// with the transaction and WAL formulas.
    /// Guarantees: `transaction_bytes` covers transaction framing plus the
    /// wider of the two per-class maxima, and `wal_bytes` leaves room for
    /// exactly one such transaction on top of the compaction threshold, so
    /// honoring the threshold keeps every WAL readable.
    #[test]
    fn transaction_and_wal_formulas_bound_a_maximal_transaction() {
        let fingerprint_bytes = 16u64;
        let widest_non_progress = widest_operations(fingerprint_bytes)
            .into_iter()
            .filter(|operation| !matches!(operation, Operation::UpdateProgress(_)))
            .max_by_key(|operation| operation.encode().expect("the operation encodes").len())
            .expect("there are seven non-progress operations");
        let non_progress_operations =
            vec![widest_non_progress; WAL_MAX_NON_PROGRESS_OPS_PER_TX as usize];
        let non_progress_tx = Transaction {
            sequence: 1,
            operations: non_progress_operations,
        };
        let non_progress_encoded = non_progress_tx
            .encode()
            .expect("the non-progress transaction encodes");

        let widest_progress = widest_operations(fingerprint_bytes)
            .into_iter()
            .find(|operation| matches!(operation, Operation::UpdateProgress(_)))
            .expect("widest_operations includes update_progress");
        let progress_operations = (0..WAL_MAX_OPS_PER_TX)
            .map(|index| {
                let Operation::UpdateProgress(mut progress) = widest_progress.clone() else {
                    unreachable!("the selected fixture is update_progress");
                };
                progress.file_id = FileId((u128::from(index) + 1).to_be_bytes());
                Operation::UpdateProgress(progress)
            })
            .collect();
        let progress_tx = Transaction {
            sequence: 2,
            operations: progress_operations,
        };
        let progress_encoded = progress_tx
            .encode()
            .expect("the progress transaction encodes");

        let bound = transaction_bytes(fingerprint_bytes).expect("the bound is representable");
        assert_eq!(
            bound,
            non_progress_encoded.len().max(progress_encoded.len()) as u64
        );

        let compact_after_bytes = 4_096u64;
        let wal = encode_wal(0, TEST_NAMESPACE_ID, &[non_progress_tx]).expect("the WAL encodes");
        let wal_bound =
            wal_bytes(compact_after_bytes, fingerprint_bytes).expect("the bound is representable");
        assert_eq!(
            wal_bound,
            WAL_HEADER_LEN as u64 + compact_after_bytes + bound
        );
        assert!(
            (wal.len() as u64) < wal_bound,
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
        // The WAL bound dropped from the pre-Stage-2A estimate now that a
        // transaction's worst case is the wider of two bounded classes
        // (256 non-progress operations, or 4096 progress operations, each
        // additionally capped by the 16 MiB hard transaction-body limit)
        // rather than `WAL_MAX_OPS_PER_TX` copies of the single widest
        // operation across all eight kinds.
        assert!(
            (64 * 1024 * 1024..=66 * 1024 * 1024).contains(&defaults.max_wal_bytes),
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

    /// Scenario: the widest snapshot record (maximum fingerprint,
    /// maximum/truncated advisory path, `Continuation` resume, Windows
    /// locator, and quarantine evidence) is independently encoded.
    /// Guarantees: its encoded frame and payload lengths equal the
    /// authoritative `SNAPSHOT_MAX_RECORD_FRAME_BYTES` /
    /// `SNAPSHOT_MAX_RECORD_PAYLOAD_BYTES` constants from
    /// `docs/filelog-checkpoint-format.md` exactly.
    #[test]
    fn snapshot_record_constants_match_the_codec() {
        let record = widest_snapshot_record(FINGERPRINT_MAX_BYTES as u64);
        let encoded = record.encode().expect("the widest record encodes");
        assert_eq!(encoded.len() as u64, SNAPSHOT_MAX_RECORD_FRAME_BYTES);
        assert_eq!(
            encoded.len() as u64 - FRAME_OVERHEAD_BYTES,
            SNAPSHOT_MAX_RECORD_PAYLOAD_BYTES
        );
    }

    /// Scenario: every non-progress operation is built at its widest legal
    /// shape with a maximum fingerprint window and independently encoded.
    /// Guarantees: the widest encoder output equals the maximum-valid
    /// fingerprint-extension constants (one byte below the structural decode
    /// ceiling), and `register_file` matches its published payload maximum.
    #[test]
    fn operation_constants_match_the_widest_encoded_operation() {
        let operations = widest_operations(FINGERPRINT_MAX_BYTES as u64);
        let widest_encoded = operations
            .iter()
            .filter(|operation| !matches!(operation, Operation::UpdateProgress(_)))
            .map(|operation| operation.encode().expect("the operation encodes").len() as u64)
            .max()
            .expect("there are seven non-progress operations");
        assert_eq!(widest_encoded, MAX_VALID_UPDATE_FINGERPRINT_FRAME_BYTES);
        assert_eq!(
            widest_encoded - FRAME_OVERHEAD_BYTES,
            MAX_VALID_UPDATE_FINGERPRINT_PAYLOAD_BYTES
        );

        let register_file_encoded = operations
            .iter()
            .find(|operation| matches!(operation, Operation::RegisterFile(_)))
            .expect("widest_operations includes register_file")
            .encode()
            .expect("register_file encodes");
        assert_eq!(
            register_file_encoded.len() as u64 - FRAME_OVERHEAD_BYTES,
            REGISTER_FILE_MAX_OP_PAYLOAD_BYTES
        );
    }

    /// Scenario: a strict fingerprint extension grows a 65,534-byte prefix
    /// to the format's 65,535-byte field maximum.
    /// Guarantees: the maximum-valid payload/frame constants match real
    /// encoder output and remain one byte below the structural equal-maximum
    /// allocation ceiling.
    #[test]
    fn maximum_valid_fingerprint_extension_constants_match_codec() {
        let expected = vec![0x5A; FINGERPRINT_MAX_BYTES - 1];
        let mut new_fingerprint = expected.clone();
        new_fingerprint.push(0xA5);
        let encoded = Operation::UpdateFingerprint(UpdateFingerprint {
            file_id: FileId([8; 16]),
            expected_file_epoch: 1,
            expected_fingerprint: expected,
            new_fingerprint,
        })
        .encode()
        .unwrap();
        assert_eq!(
            encoded.len() as u64,
            MAX_VALID_UPDATE_FINGERPRINT_FRAME_BYTES
        );
        assert_eq!(
            encoded.len() as u64 - FRAME_OVERHEAD_BYTES,
            MAX_VALID_UPDATE_FINGERPRINT_PAYLOAD_BYTES
        );
        assert_eq!(
            MAX_VALID_UPDATE_FINGERPRINT_FRAME_BYTES + 1,
            MAX_OPERATION_FRAME_BYTES
        );
        assert_eq!(
            MAX_VALID_UPDATE_FINGERPRINT_PAYLOAD_BYTES + 1,
            MAX_OPERATION_PAYLOAD_BYTES
        );
    }

    /// Scenario: the widest `update_progress` operation is independently
    /// encoded, and `WAL_MAX_OPS_PER_TX` copies of it are assembled into
    /// one progress-only transaction.
    /// Guarantees: the operation's frame and payload lengths equal
    /// `UPDATE_PROGRESS_MAX_OP_FRAME_BYTES` /
    /// `UPDATE_PROGRESS_MAX_OP_PAYLOAD_BYTES`, and the assembled
    /// transaction's body and frame lengths equal
    /// `MAX_PROGRESS_TX_BODY_BYTES` / `MAX_PROGRESS_TX_FRAME_BYTES`
    /// exactly.
    #[test]
    fn update_progress_and_progress_transaction_constants_match_the_codec() {
        let widest_progress = widest_operations(16)
            .into_iter()
            .find(|operation| matches!(operation, Operation::UpdateProgress(_)))
            .expect("widest_operations includes update_progress");
        let encoded_operation = widest_progress.encode().expect("update_progress encodes");
        assert_eq!(
            encoded_operation.len() as u64,
            UPDATE_PROGRESS_MAX_OP_FRAME_BYTES
        );
        assert_eq!(
            encoded_operation.len() as u64 - FRAME_OVERHEAD_BYTES,
            UPDATE_PROGRESS_MAX_OP_PAYLOAD_BYTES
        );

        let progress_tx = Transaction {
            sequence: 1,
            operations: (0..WAL_MAX_OPS_PER_TX)
                .map(|index| {
                    let Operation::UpdateProgress(mut progress) = widest_progress.clone() else {
                        unreachable!("the selected fixture is update_progress");
                    };
                    progress.file_id = FileId((u128::from(index) + 1).to_be_bytes());
                    Operation::UpdateProgress(progress)
                })
                .collect(),
        };
        let encoded_tx = progress_tx
            .encode()
            .expect("the progress transaction encodes");
        assert_eq!(encoded_tx.len() as u64, MAX_PROGRESS_TX_FRAME_BYTES);
        assert_eq!(
            encoded_tx.len() as u64 - TRANSACTION_OVERHEAD_BYTES,
            MAX_PROGRESS_TX_BODY_BYTES
        );
    }

    /// Scenario: a minimal strict `update_fingerprint` operation extends an
    /// empty fingerprint by one byte and is wrapped in its own transaction.
    /// Guarantees: the operation's frame length equals `TX_MIN_BODY_BYTES`
    /// and the assembled transaction's frame length equals
    /// `TX_MIN_FRAME_BYTES` exactly -- the smallest legal transaction this
    /// format allows.
    #[test]
    fn minimal_transaction_matches_tx_min_constants() {
        let minimal_operation = Operation::UpdateFingerprint(UpdateFingerprint {
            file_id: FileId([9; 16]),
            expected_file_epoch: 1,
            expected_fingerprint: Vec::new(),
            new_fingerprint: vec![1],
        });
        let encoded_operation = minimal_operation
            .encode()
            .expect("a minimal update_fingerprint encodes");
        assert_eq!(encoded_operation.len() as u64, u64::from(TX_MIN_BODY_BYTES));

        let tx = Transaction {
            sequence: 1,
            operations: vec![minimal_operation],
        };
        let encoded_tx = tx.encode().expect("the minimal transaction encodes");
        assert_eq!(encoded_tx.len() as u64, u64::from(TX_MIN_FRAME_BYTES));
    }

    /// Scenario: the format's hard transaction-body ceiling
    /// (`WAL_MAX_TX_BODY_BYTES`, 16 MiB) plus its fixed envelope overhead.
    /// Guarantees: `WAL_MAX_TX_FRAME_BYTES` equals `36 + 16,777,216 + 4`
    /// exactly, matching the published constant.
    #[test]
    fn wal_max_tx_frame_bytes_matches_the_hard_cap_arithmetic() {
        assert_eq!(WAL_MAX_TX_FRAME_BYTES, 36 + 16_777_216 + 4);
        assert_eq!(WAL_MAX_TX_FRAME_BYTES, 16_777_256);
    }
}
