// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Conformance and unit tests for the version-1 filelog checkpoint codec.
//!
//! These tests consume the checked-in golden vectors in
//! [`super::test_vectors`] plus values built directly through this crate's
//! own encoder for pure round-trip coverage, and exercise every logical WAL
//! operation's exact transition restrictions from
//! `docs/filelog-checkpoint-format.md`.

use super::apply::CheckpointTable;
use super::current_marker;
use super::error::{ApplyError, DecodeError, EncodeError};
use super::primitives::{
    AdvisoryPath, CommittedFrontierGuard, FINGERPRINT_MAX_BYTES, FileId, FramingResume,
    LifecycleState, Locator, MAX_OPERATION_FRAME_BYTES, MAX_OPERATION_PAYLOAD_BYTES,
    REASON_CODE_RESERVED, TRUNCATE_RESET_REASON_READ_NEW, WAL_MAX_NON_PROGRESS_OPS_PER_TX,
    WAL_MAX_OPS_PER_TX, crc32c,
};
use super::snapshot::{self, QuarantineEvidence, SNAPSHOT_FOOTER_LEN, SnapshotRecord};
use super::test_vectors::*;
use super::wal::{
    self, Operation, QuarantineFile, RegisterFile, RemoveFile, ResetAfterTruncate,
    ResetQuarantineAction, ResetQuarantinedFile, Transaction, UpdateFingerprint, UpdateMetadata,
    UpdateProgress, WAL_HEADER_LEN,
};

const NAMESPACE: &str = "example-namespace";

/// Test-only zero-filled window guard: a deterministic, obviously-fake
/// `CommittedFrontierGuard` for tests that only need a structurally valid
/// guard and do not exercise real continuity evidence. Production code
/// must never do this; see [`super::primitives::CommittedFrontierWindow`]
/// for the real, non-fabricated runtime window.
fn zero_guard(committed_offset: u64) -> CommittedFrontierGuard {
    let window_len = committed_offset.min(64) as usize;
    CommittedFrontierGuard::compute(committed_offset, &vec![0u8; window_len]).unwrap()
}

fn sample_digest() -> [u8; 32] {
    [0x42; 32]
}

fn sample_register(file_id: FileId) -> RegisterFile {
    RegisterFile {
        file_id,
        file_epoch: 1,
        committed_offset: 0,
        committed_frontier_guard: CommittedFrontierGuard::empty(),
        fingerprint: b"fp".to_vec(),
        ignored_header_bytes: 0,
        locator: Locator::PosixDevIno { dev: 1, ino: 2 },
        framing_profile_version: 1,
        framing_profile_digest: sample_digest(),
        framing_resume: FramingResume::Clean,
        last_seen_time_unix_nano: 1,
        advisory_path: AdvisoryPath::from_unix_bytes(b"/var/log/a.log").unwrap(),
    }
}

/// A minimal, deterministic `Active` snapshot record: zero-length
/// fingerprint, an explicit `AdvisoryPath::Unavailable` advisory path, and
/// a fixed-shape `PosixDevIno` locator, so every field's byte offset within
/// its encoded frame is fixed and easy to compute for the
/// discriminant-corruption tests below.
fn sample_snapshot_record(file_id: FileId) -> SnapshotRecord {
    SnapshotRecord {
        file_id,
        file_epoch: 1,
        committed_offset: 0,
        committed_frontier_guard: CommittedFrontierGuard::empty(),
        fingerprint: Vec::new(),
        ignored_header_bytes: 0,
        locator: Locator::PosixDevIno { dev: 1, ino: 2 },
        framing_profile_version: 1,
        framing_profile_digest: sample_digest(),
        framing_resume: FramingResume::Clean,
        lifecycle_state: LifecycleState::Active,
        quarantine_evidence: None,
        last_seen_time_unix_nano: 1,
        advisory_path: AdvisoryPath::unavailable(),
    }
}

/// Recomputes the trailing 4-byte CRC-32C of a self-delimiting
/// `... || crc32c` frame (a snapshot record, a WAL operation, a WAL
/// transaction, or a fixed-width header/footer) after a test has mutated a
/// byte elsewhere in `frame`, so the test can isolate one field's
/// validation from an incidental checksum failure.
fn refresh_frame_crc32c(frame: &mut [u8]) {
    let crc_at = frame.len() - 4;
    let crc = crc32c(&frame[..crc_at]);
    frame[crc_at..].copy_from_slice(&crc.to_be_bytes());
}

/// Zeroes out a `u16`-length-prefixed field (identified by the frame offset
/// of its 2-byte length prefix), removing its content bytes and shrinking
/// the frame, then fixes up the enclosing operation's `op_len` prefix (the
/// first 4 bytes of `frame`) and recomputes the trailing CRC. Used to
/// hand-craft a decode-time "required field was empty" or "forbidden field
/// was present" scenario that the type-safe encoder refuses to construct.
fn zero_out_var_field(frame: &mut Vec<u8>, len_field_at: usize) {
    let field_len =
        u16::from_be_bytes(frame[len_field_at..len_field_at + 2].try_into().unwrap()) as usize;
    frame[len_field_at..len_field_at + 2].copy_from_slice(&0u16.to_be_bytes());
    let _ = frame.drain(len_field_at + 2..len_field_at + 2 + field_len);
    let new_op_len = (frame.len() - 8) as u32;
    frame[0..4].copy_from_slice(&new_op_len.to_be_bytes());
    refresh_frame_crc32c(frame);
}

/// Inserts `extra` bytes at frame offset `at` (which must fall strictly
/// between the 4-byte `op_len` prefix and the trailing 4-byte CRC), fixes
/// up `op_len`, and recomputes the trailing CRC. Used to hand-craft a
/// "declared length exceeds what the defined fields consumed" scenario, or
/// a forbidden-field-present scenario, that the type-safe encoder refuses
/// to construct.
fn insert_bytes_and_refresh(frame: &mut Vec<u8>, at: usize, extra: &[u8]) {
    let _ = frame.splice(at..at, extra.iter().copied());
    let new_op_len = (frame.len() - 8) as u32;
    frame[0..4].copy_from_slice(&new_op_len.to_be_bytes());
    refresh_frame_crc32c(frame);
}

/// Scenario: every operation whose v1 payload gained fields is independently
/// encoded and decoded with nondefault evidence.
/// Guarantees: truncate fingerprints, metadata guards, and quarantine-reset
/// fingerprint/namespace/action-time fields preserve exact wire order.
#[test]
fn hardened_operation_payloads_round_trip_exact_fields() {
    let operations = [
        Operation::ResetAfterTruncate(ResetAfterTruncate {
            file_id: FileId([201; 16]),
            expected_active_epoch: 7,
            observed_truncated_size: 11,
            resulting_epoch: 8,
            new_committed_offset: 0,
            new_framing_resume: FramingResume::Clean,
            new_fingerprint: b"truncate-fingerprint".to_vec(),
            reset_time_unix_nano: 12,
            reason_code: TRUNCATE_RESET_REASON_READ_NEW,
        }),
        Operation::UpdateMetadata(UpdateMetadata {
            file_id: FileId([202; 16]),
            expected_prior_state: LifecycleState::Quarantined,
            expected_file_epoch: 9,
            last_seen_time_unix_nano: 13,
            advisory_path: Some(AdvisoryPath::from_unix_bytes(b"/new/path").unwrap()),
        }),
        Operation::ResetQuarantinedFile(ResetQuarantinedFile {
            file_id: FileId([203; 16]),
            expected_quarantine_epoch: 10,
            action: ResetQuarantineAction::ResetToEnd,
            resulting_epoch: 11,
            resulting_offset: 4096,
            new_committed_frontier_guard: zero_guard(4096),
            new_framing_resume: FramingResume::Clean,
            new_fingerprint: b"reset-fingerprint".to_vec(),
            action_time_unix_nano: 14,
            namespace_id: NAMESPACE.to_owned(),
            audit_reason: "operator approved".to_owned(),
        }),
    ];

    for operation in operations {
        let encoded = operation.encode().unwrap();
        let (decoded, consumed) = Operation::decode(&encoded).unwrap();
        assert_eq!(consumed, encoded.len());
        assert_eq!(decoded, operation);
    }
}

/// Scenario: a checksum-valid `update_fingerprint` payload carries two
/// maximum-length equal values, the structural allocation ceiling but not a
/// semantically valid strict extension.
/// Guarantees: bounded decoding accepts exactly the structural maximum while
/// semantic replay remains responsible for rejecting the equal-length update.
#[test]
fn structural_maximum_fingerprint_operation_decodes_boundedly() {
    let mut payload = Vec::with_capacity(MAX_OPERATION_PAYLOAD_BYTES as usize);
    payload.push(0x04);
    payload.extend_from_slice(&[204; 16]);
    payload.extend_from_slice(&1u32.to_be_bytes());
    payload.extend_from_slice(&(FINGERPRINT_MAX_BYTES as u16).to_be_bytes());
    payload.extend(std::iter::repeat_n(0x5A, FINGERPRINT_MAX_BYTES));
    payload.extend_from_slice(&(FINGERPRINT_MAX_BYTES as u16).to_be_bytes());
    payload.extend(std::iter::repeat_n(0x5A, FINGERPRINT_MAX_BYTES));
    assert_eq!(payload.len() as u64, MAX_OPERATION_PAYLOAD_BYTES);

    let mut frame = Vec::with_capacity(MAX_OPERATION_FRAME_BYTES as usize);
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    let crc = crc32c(&frame);
    frame.extend_from_slice(&crc.to_be_bytes());
    assert_eq!(frame.len() as u64, MAX_OPERATION_FRAME_BYTES);
    let (decoded, consumed) = Operation::decode(&frame).unwrap();
    assert_eq!(consumed, frame.len());
    let Operation::UpdateFingerprint(update) = decoded else {
        panic!("structural maximum decoded as the wrong operation");
    };
    assert_eq!(update.expected_fingerprint.len(), FINGERPRINT_MAX_BYTES);
    assert_eq!(update.new_fingerprint.len(), FINGERPRINT_MAX_BYTES);
}

/// Scenario: decoding and re-encoding the independently generated
/// generation-7 `CURRENT` fixture.
/// Guarantees: the fixed marker bytes select exactly generation 7 and the
/// Rust encoder reproduces the independent 24-byte fixture byte-for-byte.
#[test]
fn current_marker_matches_independent_vector() {
    assert_eq!(
        current_marker::decode_current_marker(CURRENT_GENERATION_7).unwrap(),
        7
    );
    assert_eq!(
        current_marker::encode_current_marker(7),
        CURRENT_GENERATION_7
    );
}

/// Scenario: independently generated generation-0 snapshot and WAL fixtures
/// contain no records or transactions beyond their required headers/footer.
/// Guarantees: both decode as empty authoritative artifacts and Rust
/// re-encoding reproduces the exact independent bytes.
#[test]
fn empty_snapshot_and_wal_header_match_independent_vectors() {
    let snapshot =
        snapshot::decode_snapshot(SNAPSHOT_EMPTY_GENERATION_0, &TEST_NAMESPACE_DIGEST).unwrap();
    assert_eq!(snapshot.generation, 0);
    assert!(snapshot.records.is_empty());
    assert_eq!(
        snapshot::encode_snapshot(0, TEST_NAMESPACE_ID, &[]).unwrap(),
        SNAPSHOT_EMPTY_GENERATION_0
    );

    let wal = wal::decode_wal(WAL_HEADER_GENERATION_0, &TEST_NAMESPACE_DIGEST).unwrap();
    assert_eq!(wal.generation, 0);
    assert!(wal.transactions.is_empty());
    assert_eq!(wal.torn_tail_bytes, 0);
    assert_eq!(
        wal::encode_wal(0, TEST_NAMESPACE_ID, &[]).unwrap(),
        WAL_HEADER_GENERATION_0
    );
}

/// Scenario: decoding the checked-in two-record snapshot golden vector.
/// Guarantees: both records decode with exactly the field values the vector
/// was independently constructed with (identity, progress, framing,
/// lifecycle, and advisory groups), including a `Quarantined` record's
/// immutable evidence.
#[test]
fn decode_snapshot_two_records_matches_expected_fields() {
    let contents = snapshot::decode_snapshot(SNAPSHOT_TWO_RECORDS, &TEST_NAMESPACE_DIGEST).unwrap();
    assert_eq!(contents.generation, 7);
    let records = contents.records;
    assert_eq!(records.len(), 2);

    let active = &records[0];
    assert_eq!(active.file_id, FileId(SNAPSHOT_VECTOR_ACTIVE_FILE_ID));
    assert_eq!(active.file_epoch, 1);
    assert_eq!(active.committed_offset, 4);
    assert_eq!(
        active.committed_frontier_guard,
        CommittedFrontierGuard::compute(4, b"abc\n").unwrap()
    );
    assert_eq!(active.fingerprint, b"abc");
    assert_eq!(active.locator, Locator::PosixDevIno { dev: 7, ino: 42 });
    assert_eq!(active.framing_resume, FramingResume::Clean);
    assert_eq!(active.lifecycle_state, LifecycleState::Active);
    assert!(active.quarantine_evidence.is_none());
    assert_eq!(
        active.advisory_path,
        AdvisoryPath::from_unix_bytes(b"/var/log/app.log").unwrap()
    );

    let quarantined = &records[1];
    assert_eq!(
        quarantined.file_id,
        FileId(SNAPSHOT_VECTOR_QUARANTINED_FILE_ID)
    );
    assert_eq!(quarantined.committed_offset, 1000);
    assert_eq!(
        quarantined.locator,
        Locator::WindowsVolumeFileId {
            volume_serial: 0x0123_4567_89AB_CDEF,
            file_id: [0xBB; 16],
        }
    );
    assert_eq!(
        quarantined.framing_resume,
        FramingResume::Continuation {
            record_start_offset: 500,
            record_end_offset: 0,
            next_fragment_index: 2,
        }
    );
    assert_eq!(quarantined.lifecycle_state, LifecycleState::Quarantined);
    let evidence = quarantined.quarantine_evidence.as_ref().unwrap();
    assert_eq!(evidence.reason_code, 0x0003);
    assert_eq!(evidence.observed_size, 999);
    assert_eq!(evidence.quarantine_epoch, 1);
}

/// Scenario: decoding a snapshot containing one POSIX-locator record and one
/// Windows-locator record on whatever host platform happens to run this
/// test (never Linux/macOS-only or Windows-only in CI).
/// Guarantees: the codec never depends on the host's own native locator
/// type; both normalized locator kinds decode identically everywhere,
/// because `Locator` is plain data with no OS FFI.
#[test]
fn cross_platform_locators_decode_independently_of_host() {
    let records = snapshot::decode_snapshot(SNAPSHOT_TWO_RECORDS, &TEST_NAMESPACE_DIGEST)
        .unwrap()
        .records;
    assert!(matches!(records[0].locator, Locator::PosixDevIno { .. }));
    assert!(matches!(
        records[1].locator,
        Locator::WindowsVolumeFileId { .. }
    ));
}

/// Scenario: re-encoding the records decoded from the golden snapshot vector
/// through this crate's own encoder, then decoding the result again.
/// Guarantees: encode(decode(x)) reproduces exactly the original decoded
/// records, so the codec's own encoder and decoder agree with each other in
/// addition to agreeing with the independently constructed golden vector.
#[test]
fn snapshot_round_trips_through_own_encoder() {
    let original = snapshot::decode_snapshot(SNAPSHOT_TWO_RECORDS, &TEST_NAMESPACE_DIGEST).unwrap();
    let re_encoded =
        snapshot::encode_snapshot(original.generation, TEST_NAMESPACE_ID, &original.records)
            .unwrap();
    let decoded_again = snapshot::decode_snapshot(&re_encoded, &TEST_NAMESPACE_DIGEST).unwrap();
    assert_eq!(original, decoded_again);
}

/// Scenario: decoding and re-encoding the independently generated snapshot
/// fixture for the third reachable lifecycle.
/// Guarantees: `RotatedFinalized` state has clean framing and no quarantine
/// evidence, preserves its historical locator, and reproduces the exact
/// reference bytes.
#[test]
fn rotated_finalized_snapshot_matches_independent_vector() {
    let contents =
        snapshot::decode_snapshot(SNAPSHOT_ROTATED_FINALIZED, &TEST_NAMESPACE_DIGEST).unwrap();
    assert_eq!(contents.generation, 9);
    assert_eq!(contents.records.len(), 1);
    let record = &contents.records[0];
    assert_eq!(record.file_id, FileId(ROTATED_FILE_ID));
    assert_eq!(record.file_epoch, 3);
    assert_eq!(record.committed_offset, 4);
    assert_eq!(
        record.committed_frontier_guard,
        CommittedFrontierGuard::compute(4, b"abc\n").unwrap()
    );
    assert_eq!(record.fingerprint, b"done");
    assert_eq!(record.locator, Locator::PosixDevIno { dev: 7, ino: 43 });
    assert_eq!(record.framing_resume, FramingResume::Clean);
    assert_eq!(record.lifecycle_state, LifecycleState::RotatedFinalized);
    assert!(record.quarantine_evidence.is_none());
    assert_eq!(record.last_seen_time_unix_nano, 1234);
    assert_eq!(record.advisory_path, AdvisoryPath::unavailable());
    assert_eq!(
        snapshot::encode_snapshot(9, TEST_NAMESPACE_ID, &contents.records).unwrap(),
        SNAPSHOT_ROTATED_FINALIZED
    );
}

/// Scenario: a snapshot header whose `format_version` is `2`, which this
/// codec does not recognize.
/// Guarantees: decoding fails closed with `UnsupportedFormatVersion` even
/// though the header's own bytes are otherwise well-formed, and this check
/// runs before any record is parsed.
#[test]
fn snapshot_unsupported_version_fails_closed() {
    let err = snapshot::decode_snapshot(SNAPSHOT_BAD_VERSION, &TEST_NAMESPACE_DIGEST).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::UnsupportedFormatVersion { found: 2, .. }
    ));
}

/// Scenario: a structurally complete, valid snapshot file with one extra
/// trailing byte appended after its footer.
/// Guarantees: decoding fails closed with `TrailingBytes`; unlike the WAL, a
/// snapshot has no torn-tail tolerance, so unexpected trailing data is
/// always an error.
#[test]
fn snapshot_trailing_bytes_fail_closed() {
    let mut bytes = SNAPSHOT_TWO_RECORDS.to_vec();
    bytes.push(0x00);
    let err = snapshot::decode_snapshot(&bytes, &TEST_NAMESPACE_DIGEST).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::TrailingBytes { remaining: 1, .. }
    ));
}

/// Scenario: a byte inside the first record's advisory-path field of the
/// golden snapshot vector is flipped, changing the record's content without
/// changing its declared length.
/// Guarantees: the record's own CRC-32C no longer matches, so decoding fails
/// closed with a checksum mismatch rather than silently accepting corrupted
/// record content.
#[test]
fn snapshot_record_checksum_corruption_fails_closed() {
    let mut bytes = SNAPSHOT_TWO_RECORDS.to_vec();
    // Offset of a byte within record 1's advisory_path ("/var/log/app.log").
    let target = bytes
        .windows(4)
        .position(|w| w == b"/var")
        .expect("advisory_path bytes present in the vector");
    bytes[target] ^= 0xFF;
    let err = snapshot::decode_snapshot(&bytes, &TEST_NAMESPACE_DIGEST).unwrap_err();
    assert!(matches!(err, DecodeError::ChecksumMismatch { .. }));
}

/// Scenario: decoding the checked-in two-transaction WAL golden vector.
/// Guarantees: both transactions decode with strictly increasing sequences
/// (`1`, then `2`), the expected operation contents, and a clean (non-torn)
/// end of file.
#[test]
fn decode_wal_valid_two_tx_matches_expected_operations() {
    let contents = wal::decode_wal(WAL_VALID_TWO_TX, &TEST_NAMESPACE_DIGEST).unwrap();
    assert_eq!(contents.generation, 3);
    assert_eq!(contents.torn_tail_bytes, 0);
    assert_eq!(contents.transactions.len(), 2);
    assert_eq!(contents.transactions[0].sequence, 1);
    assert_eq!(contents.transactions[1].sequence, 2);

    match &contents.transactions[0].operations[0] {
        Operation::RegisterFile(op) => {
            assert_eq!(op.file_id, FileId(WAL_VECTOR_FILE_ID));
            assert_eq!(op.committed_offset, 0);
            assert_eq!(op.fingerprint, b"seedfp");
        }
        other => panic!("expected register_file, found {other:?}"),
    }
    match &contents.transactions[1].operations[0] {
        Operation::UpdateProgress(op) => {
            assert_eq!(op.file_id, FileId(WAL_VECTOR_FILE_ID));
            assert_eq!(op.expected_committed_offset, 0);
            assert_eq!(op.new_committed_offset, 4096);
            assert!(!op.finalize);
        }
        other => panic!("expected update_progress, found {other:?}"),
    }
}

/// Scenario: replaying the golden WAL vector's transactions against a fresh
/// checkpoint table, in order.
/// Guarantees: `register_file` followed by `update_progress` produces the
/// expected end state (`Active`, `committed_offset == 4096`), matching the
/// realistic register-then-Ack sequence the vector represents.
#[test]
fn replay_wal_valid_two_tx_updates_table() {
    let contents = wal::decode_wal(WAL_VALID_TWO_TX, &TEST_NAMESPACE_DIGEST).unwrap();
    let mut table = CheckpointTable::new();
    table.replay(&contents.transactions, NAMESPACE).unwrap();
    let record = table.get(&FileId(WAL_VECTOR_FILE_ID)).unwrap();
    assert_eq!(record.committed_offset, 4096);
    assert_eq!(record.lifecycle_state, LifecycleState::Active);
}

/// Scenario: the independently generated generation-5 WAL contains one
/// complete transaction for every v1 operation in semantic replay order.
/// Guarantees: all exact payload fields decode, Rust re-encoding reproduces
/// the reference bytes, `keep_failed` preserves operational state, and the
/// final administrative removal leaves the table empty.
#[test]
fn all_operations_wal_matches_independent_vector_and_replays() {
    let contents = wal::decode_wal(WAL_ALL_OPERATIONS, &TEST_NAMESPACE_DIGEST).unwrap();
    assert_eq!(contents.generation, 5);
    assert_eq!(contents.torn_tail_bytes, 0);
    assert_eq!(contents.transactions.len(), 8);
    for (index, transaction) in contents.transactions.iter().enumerate() {
        assert_eq!(transaction.sequence, index as u64 + 1);
        assert_eq!(transaction.operations.len(), 1);
    }
    assert!(matches!(
        contents.transactions[0].operations[0],
        Operation::RegisterFile(_)
    ));
    assert!(matches!(
        contents.transactions[1].operations[0],
        Operation::UpdateProgress(_)
    ));
    assert!(matches!(
        contents.transactions[2].operations[0],
        Operation::UpdateFingerprint(_)
    ));
    assert!(matches!(
        contents.transactions[3].operations[0],
        Operation::UpdateMetadata(_)
    ));
    assert!(matches!(
        contents.transactions[4].operations[0],
        Operation::ResetAfterTruncate(_)
    ));
    assert!(matches!(
        contents.transactions[5].operations[0],
        Operation::QuarantineFile(_)
    ));
    assert!(matches!(
        contents.transactions[6].operations[0],
        Operation::ResetQuarantinedFile(_)
    ));
    assert!(matches!(
        contents.transactions[7].operations[0],
        Operation::RemoveFile(_)
    ));

    let Operation::UpdateMetadata(metadata) = &contents.transactions[3].operations[0] else {
        unreachable!("operation kind checked above");
    };
    assert_eq!(metadata.expected_prior_state, LifecycleState::Active);
    assert_eq!(metadata.expected_file_epoch, 1);
    assert_eq!(
        metadata.advisory_path,
        Some(AdvisoryPath::from_unix_bytes(b"/var/log/new.log").unwrap())
    );
    let Operation::ResetAfterTruncate(reset) = &contents.transactions[4].operations[0] else {
        unreachable!("operation kind checked above");
    };
    assert_eq!(reset.new_fingerprint, b"new");
    let Operation::ResetQuarantinedFile(reset) = &contents.transactions[6].operations[0] else {
        unreachable!("operation kind checked above");
    };
    assert_eq!(reset.action, ResetQuarantineAction::KeepFailed);
    assert_eq!(reset.new_fingerprint, b"new");
    assert_eq!(reset.action_time_unix_nano, 105);
    assert_eq!(reset.namespace_id, TEST_NAMESPACE_ID);
    assert_eq!(reset.audit_reason, "keep failed");
    assert_eq!(
        wal::encode_wal(5, TEST_NAMESPACE_ID, &contents.transactions).unwrap(),
        WAL_ALL_OPERATIONS
    );

    let mut table = CheckpointTable::new();
    table
        .replay(&contents.transactions[..7], TEST_NAMESPACE_ID)
        .unwrap();
    let preserved = table.get(&FileId(WAL_VECTOR_FILE_ID)).unwrap();
    assert_eq!(preserved.lifecycle_state, LifecycleState::Quarantined);
    assert_eq!(preserved.file_epoch, 2);
    assert_eq!(preserved.committed_offset, 0);
    assert_eq!(preserved.committed_frontier_guard, zero_guard(0));
    assert_eq!(preserved.framing_resume, FramingResume::Clean);
    assert_eq!(preserved.fingerprint, b"new");
    assert_eq!(preserved.last_seen_time_unix_nano, 103);
    assert_eq!(
        preserved.advisory_path,
        AdvisoryPath::from_unix_bytes(b"/var/log/new.log").unwrap()
    );

    table
        .apply_transaction(&contents.transactions[7], TEST_NAMESPACE_ID)
        .unwrap();
    assert!(table.is_empty());
}

/// Scenario: the independent all-operation fixture's `keep_failed`
/// fingerprint is changed without changing any frame length, and all enclosing
/// CRCs are recomputed.
/// Guarantees: the structurally valid WAL decodes, replay rejects the
/// operational mutation as `KeepFailedStateChange`, and quarantine state from
/// the preceding transactions remains unchanged.
#[test]
fn independent_keep_failed_mutation_fails_closed() {
    let mut wal_bytes = WAL_ALL_OPERATIONS.to_vec();
    let mut transaction_start = WAL_HEADER_LEN;
    for sequence in 1..7 {
        let body_len = u32::from_be_bytes(
            wal_bytes[transaction_start + 20..transaction_start + 24]
                .try_into()
                .unwrap(),
        ) as usize;
        transaction_start += 36 + body_len + 4;
        assert_eq!(
            u64::from_be_bytes(
                wal_bytes[transaction_start + 12..transaction_start + 20]
                    .try_into()
                    .unwrap()
            ),
            sequence + 1
        );
    }
    let body_len = u32::from_be_bytes(
        wal_bytes[transaction_start + 20..transaction_start + 24]
            .try_into()
            .unwrap(),
    ) as usize;
    let operation_start = transaction_start + 36;
    let operation_len = u32::from_be_bytes(
        wal_bytes[operation_start..operation_start + 4]
            .try_into()
            .unwrap(),
    ) as usize;
    let fingerprint_start = operation_start + 4 + 69 + 2;
    wal_bytes[fingerprint_start] = b'x';
    let operation_crc_at = operation_start + 4 + operation_len;
    let operation_crc = crc32c(&wal_bytes[operation_start..operation_crc_at]);
    wal_bytes[operation_crc_at..operation_crc_at + 4].copy_from_slice(&operation_crc.to_be_bytes());
    let transaction_crc_at = transaction_start + 36 + body_len;
    let transaction_crc = crc32c(&wal_bytes[transaction_start..transaction_crc_at]);
    wal_bytes[transaction_crc_at..transaction_crc_at + 4]
        .copy_from_slice(&transaction_crc.to_be_bytes());

    let contents = wal::decode_wal(&wal_bytes, &TEST_NAMESPACE_DIGEST).unwrap();
    let mut table = CheckpointTable::new();
    let error = table
        .replay(&contents.transactions[..7], TEST_NAMESPACE_ID)
        .unwrap_err();
    assert!(matches!(error, ApplyError::KeepFailedStateChange { .. }));
    let record = table.get(&FileId(WAL_VECTOR_FILE_ID)).unwrap();
    assert_eq!(record.lifecycle_state, LifecycleState::Quarantined);
    assert_eq!(record.fingerprint, b"new");
}

/// Scenario: a WAL whose final transaction is structurally incomplete (a
/// `tx_len` prefix declaring far more body bytes than actually follow it).
/// Guarantees: decoding discards exactly the incomplete trailing bytes as a
/// torn tail rather than failing, and every preceding, complete transaction
/// is still returned intact -- the sole leniency this format grants.
#[test]
fn wal_torn_tail_is_discarded_without_error() {
    let contents = wal::decode_wal(WAL_TORN_TAIL, &TEST_NAMESPACE_DIGEST).unwrap();
    assert_eq!(contents.transactions.len(), 2);
    assert_eq!(contents.torn_tail_bytes, 24);
}

/// Scenario: a WAL whose final transaction is structurally complete (its
/// declared `tx_len` matches the bytes actually present) but whose
/// `tx_crc32c` has been corrupted.
/// Guarantees: decoding fails closed with a checksum mismatch instead of
/// discarding it as a torn tail -- a complete frame with a bad checksum is
/// corruption, never a torn write, even at the physical end of the file.
#[test]
fn wal_corrupt_final_transaction_fails_closed_not_torn() {
    let err = wal::decode_wal(WAL_CORRUPT_FINAL_TX, &TEST_NAMESPACE_DIGEST).unwrap_err();
    assert!(matches!(err, DecodeError::ChecksumMismatch { .. }));
}

/// Scenario: a WAL whose final transaction has a structurally complete,
/// CRC-valid 36-byte header declaring a sequence that does not follow the
/// prior transaction, and whose body is then truncated short (fewer bytes
/// follow than `body_len` plus the trailing frame CRC require).
/// Guarantees: `scan_one_transaction` validates `sequence` from the
/// complete, CRC-checked header before ever consulting `body_len` to
/// classify the remaining suffix, so this is reported as
/// `SequenceOutOfOrder` corruption -- never silently discarded as a torn
/// tail just because the body that follows happens to look truncated.
#[test]
fn wal_out_of_sequence_header_with_incomplete_body_is_corruption_not_torn() {
    let first = Transaction {
        sequence: 1,
        operations: vec![Operation::RegisterFile(sample_register(FileId([7; 16])))],
    };
    let mut wal_bytes =
        wal::encode_wal(1, TEST_NAMESPACE_ID, std::slice::from_ref(&first)).unwrap();

    // A structurally valid transaction (own header CRC and frame CRC both
    // correct) at sequence `5`, which cannot legally follow sequence `1`.
    let bad_sequence = Transaction {
        sequence: 5,
        operations: vec![Operation::RegisterFile(sample_register(FileId([8; 16])))],
    };
    let encoded_bad = bad_sequence.encode().unwrap();
    // Keep the complete 36-byte header (with its valid header CRC) but
    // truncate the body/frame-CRC suffix that follows it, so `remaining <
    // needed` would otherwise look like an ordinary torn tail.
    wal_bytes.extend_from_slice(&encoded_bad[..36 + 4]);

    let err = wal::decode_wal(&wal_bytes, &TEST_NAMESPACE_DIGEST).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::SequenceOutOfOrder {
            expected: 2,
            found: 5,
        }
    ));
}

/// Guarantees: decoding fails closed with `UnsupportedFormatVersion` before
/// any transaction is parsed, exactly like the snapshot header case; this is
/// the cross-version/migration-policy conformance vector for the WAL.
#[test]
fn wal_unsupported_version_fails_closed() {
    let err = wal::decode_wal(WAL_BAD_VERSION, &TEST_NAMESPACE_DIGEST).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::UnsupportedFormatVersion { found: 2, .. }
    ));
}

/// Scenario: an unsupported `format_version` header whose stored CRC-32C
/// would also fail to validate under the (wrong) current-version layout.
/// Guarantees: `UnsupportedFormatVersion` is reported, not
/// `ChecksumMismatch`; version compatibility is checked strictly before
/// integrity, so an operator sees "migration required" rather than a
/// misleading corruption report.
#[test]
fn unsupported_version_is_distinguished_from_checksum_corruption() {
    let snapshot_err =
        snapshot::decode_snapshot(SNAPSHOT_BAD_VERSION, &TEST_NAMESPACE_DIGEST).unwrap_err();
    assert!(matches!(
        snapshot_err,
        DecodeError::UnsupportedFormatVersion { .. }
    ));
    let wal_err = wal::decode_wal(WAL_BAD_VERSION, &TEST_NAMESPACE_DIGEST).unwrap_err();
    assert!(matches!(
        wal_err,
        DecodeError::UnsupportedFormatVersion { .. }
    ));
}

/// Scenario: encoding a transaction built directly through this crate's own
/// types, then decoding the result.
/// Guarantees: `decode(encode(x)) == x` for a transaction containing a
/// `quarantine_file` operation, covering an operation shape not present in
/// the checked-in golden WAL vector.
#[test]
fn wal_transaction_round_trips_through_own_encoder() {
    let file_id = FileId([0x55; 16]);
    let operation = Operation::QuarantineFile(QuarantineFile {
        file_id,
        expected_file_epoch: 1,
        reason_code: 0x0002,
        locator: Locator::PosixDevIno { dev: 3, ino: 4 },
        observed_size: 128,
        quarantine_epoch: 1,
        quarantine_time_unix_nano: 42,
    });
    let transaction = Transaction {
        sequence: 1,
        operations: vec![operation],
    };
    let encoded =
        wal::encode_wal(1, TEST_NAMESPACE_ID, std::slice::from_ref(&transaction)).unwrap();
    let decoded = wal::decode_wal(&encoded, &TEST_NAMESPACE_DIGEST).unwrap();
    assert_eq!(decoded.transactions, vec![transaction]);
}

// ---------------------------------------------------------------------
// Apply-layer (replay) tests: exact transition restrictions per operation.
// ---------------------------------------------------------------------

/// Scenario: `register_file` for a brand-new `file_id` with `file_epoch`
/// set to `2` instead of `1`.
/// Guarantees: replay rejects registration at any epoch other than `1`, the
/// wire-level enforcement of "registration always begins a file's first
/// epoch."
#[test]
fn register_file_requires_epoch_one() {
    let mut table = CheckpointTable::new();
    let mut op = sample_register(FileId([1; 16]));
    op.file_epoch = 2;
    let err = table
        .apply_transaction(
            &Transaction {
                sequence: 1,
                operations: vec![Operation::RegisterFile(op)],
            },
            NAMESPACE,
        )
        .unwrap_err();
    assert!(matches!(err, ApplyError::ImpossibleTransition { .. }));
}

/// Scenario: `register_file` for a brand-new `file_id` whose `locator` is
/// `Locator::Unspecified`.
/// Guarantees: replay rejects an `Unspecified` locator at insertion, before
/// any identical-replay check could otherwise wave it through, matching
/// `SnapshotRecord::validate_reachable_state`'s reachable-state invariant
/// that an `Active` record always has a recognized, non-`Unspecified`
/// locator.
#[test]
fn register_file_rejects_unspecified_locator() {
    let mut table = CheckpointTable::new();
    let mut op = sample_register(FileId([15; 16]));
    op.locator = Locator::Unspecified;
    let err = table
        .apply_transaction(
            &Transaction {
                sequence: 1,
                operations: vec![Operation::RegisterFile(op)],
            },
            NAMESPACE,
        )
        .unwrap_err();
    assert!(matches!(err, ApplyError::ImpossibleTransition { .. }));
}

/// Scenario: `register_file` for a brand-new `file_id` whose
/// `framing_profile_version` is `0`.
/// Guarantees: replay rejects a zero `framing_profile_version` at
/// insertion, matching the snapshot's reachable-state invariant that this
/// field is always nonzero.
#[test]
fn register_file_rejects_zero_framing_profile_version() {
    let mut table = CheckpointTable::new();
    let mut op = sample_register(FileId([16; 16]));
    op.framing_profile_version = 0;
    let err = table
        .apply_transaction(
            &Transaction {
                sequence: 1,
                operations: vec![Operation::RegisterFile(op)],
            },
            NAMESPACE,
        )
        .unwrap_err();
    assert!(matches!(err, ApplyError::ImpossibleTransition { .. }));
}

/// Scenario: a `register_file` with an `Unspecified` locator is replayed
/// twice, bit-for-bit identical both times.
/// Guarantees: the unconditional locator validation runs before the
/// identical-replay comparison, so a second, byte-identical replay of an
/// already-rejected (never durably inserted) registration still fails
/// closed rather than being waved through as "benign" because it matches
/// a nonexistent prior record.
#[test]
fn register_file_rejects_unspecified_locator_on_replay_too() {
    let mut table = CheckpointTable::new();
    let mut op = sample_register(FileId([17; 16]));
    op.locator = Locator::Unspecified;
    let transaction = Transaction {
        sequence: 1,
        operations: vec![Operation::RegisterFile(op.clone())],
    };
    let _ = table
        .apply_transaction(&transaction, NAMESPACE)
        .unwrap_err();
    let err = table
        .apply_transaction(&transaction, NAMESPACE)
        .unwrap_err();
    assert!(matches!(err, ApplyError::ImpossibleTransition { .. }));
    assert!(table.get(&op.file_id).is_none());
}

/// change); the differing replay fails closed as a conflicting
/// registration rather than silently overwriting durable identity.
#[test]
fn register_file_identical_replay_is_idempotent_but_conflict_fails_closed() {
    let file_id = FileId([2; 16]);
    let mut table = CheckpointTable::new();
    let register = sample_register(file_id);
    let tx = |op: RegisterFile, seq: u64| Transaction {
        sequence: seq,
        operations: vec![Operation::RegisterFile(op)],
    };
    table
        .apply_transaction(&tx(register.clone(), 1), NAMESPACE)
        .unwrap();
    table
        .apply_transaction(&tx(register.clone(), 2), NAMESPACE)
        .expect("identical replay is idempotent");
    assert_eq!(table.len(), 1);

    let mut conflicting = register;
    conflicting.fingerprint = b"different".to_vec();
    let err = table
        .apply_transaction(&tx(conflicting, 3), NAMESPACE)
        .unwrap_err();
    assert!(matches!(err, ApplyError::ConflictingRegistration { .. }));
}

/// Scenario: `update_progress` naming an `expected_file_epoch` that does not
/// match the record's current (post-truncate-reset) epoch.
/// Guarantees: an Ack computed against a stale epoch can never advance the
/// replacement stream -- the mechanism behind "an earlier-epoch Ack cannot
/// advance the resulting stream."
#[test]
fn update_progress_rejects_stale_epoch() {
    let file_id = FileId([3; 16]);
    let mut table = CheckpointTable::new();
    table
        .apply_transaction(
            &Transaction {
                sequence: 1,
                operations: vec![Operation::RegisterFile(sample_register(file_id))],
            },
            NAMESPACE,
        )
        .unwrap();
    let stale_update = Operation::UpdateProgress(UpdateProgress {
        file_id,
        expected_committed_offset: 0,
        expected_file_epoch: 0, // stale: the real epoch is 1
        new_committed_offset: 100,
        new_committed_frontier_guard: zero_guard(100),
        new_framing_resume: FramingResume::Clean,
        new_last_seen_time_unix_nano: 2,
        finalize: false,
    });
    let err = table
        .apply_transaction(
            &Transaction {
                sequence: 2,
                operations: vec![stale_update],
            },
            NAMESPACE,
        )
        .unwrap_err();
    assert!(matches!(err, ApplyError::ImpossibleTransition { .. }));
}

/// Scenario: `update_progress` whose `new_committed_offset` is smaller than
/// the currently committed offset.
/// Guarantees: replay rejects an offset regression rather than silently
/// rewinding durable progress.
#[test]
fn update_progress_rejects_offset_regression() {
    let file_id = FileId([4; 16]);
    let mut table = CheckpointTable::new();
    let mut register = sample_register(file_id);
    register.committed_offset = 100;
    register.committed_frontier_guard = zero_guard(100);
    table
        .apply_transaction(
            &Transaction {
                sequence: 1,
                operations: vec![Operation::RegisterFile(register)],
            },
            NAMESPACE,
        )
        .unwrap();
    let regression = Operation::UpdateProgress(UpdateProgress {
        file_id,
        expected_committed_offset: 100,
        expected_file_epoch: 1,
        new_committed_offset: 50,
        new_committed_frontier_guard: zero_guard(50),
        new_framing_resume: FramingResume::Clean,
        new_last_seen_time_unix_nano: 2,
        finalize: false,
    });
    let err = table
        .apply_transaction(
            &Transaction {
                sequence: 2,
                operations: vec![regression],
            },
            NAMESPACE,
        )
        .unwrap_err();
    assert!(matches!(err, ApplyError::OffsetRegression { .. }));
}

/// Scenario: a zero-delta finalizing update targets an active record whose
/// durable framing resume is a split-record continuation.
/// Guarantees: replay cannot replace the continuation with `Clean` or finalize
/// the record without advancing through the remaining source bytes.
#[test]
fn zero_delta_finalization_cannot_discard_continuation() {
    let file_id = FileId([94; 16]);
    let mut table = CheckpointTable::new();
    let continuation = FramingResume::Continuation {
        record_start_offset: 0,
        record_end_offset: 20,
        next_fragment_index: 1,
    };
    table
        .apply_transaction(
            &Transaction {
                sequence: 1,
                operations: vec![Operation::RegisterFile(sample_register(file_id))],
            },
            NAMESPACE,
        )
        .unwrap();
    table
        .apply_transaction(
            &Transaction {
                sequence: 2,
                operations: vec![Operation::UpdateProgress(UpdateProgress {
                    file_id,
                    expected_committed_offset: 0,
                    expected_file_epoch: 1,
                    new_committed_offset: 10,
                    new_committed_frontier_guard: zero_guard(10),
                    new_framing_resume: continuation,
                    new_last_seen_time_unix_nano: 2,
                    finalize: false,
                })],
            },
            NAMESPACE,
        )
        .unwrap();
    let before = table
        .get(&file_id)
        .expect("continuation record is present")
        .clone();

    let err = table
        .apply_transaction(
            &Transaction {
                sequence: 3,
                operations: vec![Operation::UpdateProgress(UpdateProgress {
                    file_id,
                    expected_committed_offset: 10,
                    expected_file_epoch: 1,
                    new_committed_offset: 10,
                    new_committed_frontier_guard: zero_guard(10),
                    new_framing_resume: FramingResume::Clean,
                    new_last_seen_time_unix_nano: 3,
                    finalize: true,
                })],
            },
            NAMESPACE,
        )
        .unwrap_err();
    assert!(matches!(
        err,
        ApplyError::ImpossibleTransition {
            operation: "update_progress",
            reason: "a zero-delta update must repeat the stored framing resume exactly",
            ..
        }
    ));

    let record = table.get(&file_id).expect("record remains present");
    assert_eq!(record, &before);
}

/// Scenario: a zero-delta finalizing update targets an active record whose
/// durable framing resume is already `Clean`.
/// Guarantees: the valid zero-delta rotation-finalization path remains
/// accepted while the continuation-discarding transition is rejected.
#[test]
fn zero_delta_finalization_accepts_already_clean_state() {
    let file_id = FileId([95; 16]);
    let mut table = CheckpointTable::new();
    let mut register = sample_register(file_id);
    register.committed_offset = 10;
    register.committed_frontier_guard = zero_guard(10);
    table
        .apply_transaction(
            &Transaction {
                sequence: 1,
                operations: vec![Operation::RegisterFile(register)],
            },
            NAMESPACE,
        )
        .unwrap();

    table
        .apply_transaction(
            &Transaction {
                sequence: 2,
                operations: vec![Operation::UpdateProgress(UpdateProgress {
                    file_id,
                    expected_committed_offset: 10,
                    expected_file_epoch: 1,
                    new_committed_offset: 10,
                    new_committed_frontier_guard: zero_guard(10),
                    new_framing_resume: FramingResume::Clean,
                    new_last_seen_time_unix_nano: 2,
                    finalize: true,
                })],
            },
            NAMESPACE,
        )
        .unwrap();

    let record = table.get(&file_id).expect("record remains present");
    assert_eq!(record.committed_offset, 10);
    assert_eq!(record.framing_resume, FramingResume::Clean);
    assert_eq!(record.lifecycle_state, LifecycleState::RotatedFinalized);
    assert_eq!(record.last_seen_time_unix_nano, 2);
}

/// Scenario: advancing progress remains inside a known split record, reaches
/// its end, and then starts a later continuation.
/// Guarantees: replay requires the same continuation coordinates with a
/// strictly increasing fragment index before the known end, permits `Clean`
/// at the end, and refuses a later continuation that starts before it.
#[test]
fn advancing_progress_enforces_known_continuation_transitions() {
    let file_id = FileId([96; 16]);
    let mut table = CheckpointTable::new();
    table
        .apply_transaction(
            &Transaction {
                sequence: 1,
                operations: vec![Operation::RegisterFile(sample_register(file_id))],
            },
            NAMESPACE,
        )
        .unwrap();
    table
        .apply_transaction(
            &Transaction {
                sequence: 2,
                operations: vec![Operation::UpdateProgress(UpdateProgress {
                    file_id,
                    expected_committed_offset: 0,
                    expected_file_epoch: 1,
                    new_committed_offset: 10,
                    new_committed_frontier_guard: zero_guard(10),
                    new_framing_resume: FramingResume::Continuation {
                        record_start_offset: 0,
                        record_end_offset: 20,
                        next_fragment_index: 1,
                    },
                    new_last_seen_time_unix_nano: 2,
                    finalize: false,
                })],
            },
            NAMESPACE,
        )
        .unwrap();
    let before = table.get(&file_id).unwrap().clone();

    for invalid_resume in [
        FramingResume::Clean,
        FramingResume::Continuation {
            record_start_offset: 0,
            record_end_offset: 20,
            next_fragment_index: 1,
        },
        FramingResume::Continuation {
            record_start_offset: 1,
            record_end_offset: 20,
            next_fragment_index: 2,
        },
    ] {
        let error = table
            .apply_transaction(
                &Transaction {
                    sequence: 3,
                    operations: vec![Operation::UpdateProgress(UpdateProgress {
                        file_id,
                        expected_committed_offset: 10,
                        expected_file_epoch: 1,
                        new_committed_offset: 15,
                        new_committed_frontier_guard: zero_guard(15),
                        new_framing_resume: invalid_resume,
                        new_last_seen_time_unix_nano: 3,
                        finalize: false,
                    })],
                },
                NAMESPACE,
            )
            .unwrap_err();
        assert!(matches!(error, ApplyError::ImpossibleTransition { .. }));
        assert_eq!(table.get(&file_id), Some(&before));
    }

    table
        .apply_transaction(
            &Transaction {
                sequence: 3,
                operations: vec![Operation::UpdateProgress(UpdateProgress {
                    file_id,
                    expected_committed_offset: 10,
                    expected_file_epoch: 1,
                    new_committed_offset: 15,
                    new_committed_frontier_guard: zero_guard(15),
                    new_framing_resume: FramingResume::Continuation {
                        record_start_offset: 0,
                        record_end_offset: 20,
                        next_fragment_index: 2,
                    },
                    new_last_seen_time_unix_nano: 3,
                    finalize: false,
                })],
            },
            NAMESPACE,
        )
        .unwrap();
    table
        .apply_transaction(
            &Transaction {
                sequence: 4,
                operations: vec![Operation::UpdateProgress(UpdateProgress {
                    file_id,
                    expected_committed_offset: 15,
                    expected_file_epoch: 1,
                    new_committed_offset: 20,
                    new_committed_frontier_guard: zero_guard(20),
                    new_framing_resume: FramingResume::Clean,
                    new_last_seen_time_unix_nano: 4,
                    finalize: false,
                })],
            },
            NAMESPACE,
        )
        .unwrap();
    let error = table
        .apply_transaction(
            &Transaction {
                sequence: 5,
                operations: vec![Operation::UpdateProgress(UpdateProgress {
                    file_id,
                    expected_committed_offset: 20,
                    expected_file_epoch: 1,
                    new_committed_offset: 25,
                    new_committed_frontier_guard: zero_guard(25),
                    new_framing_resume: FramingResume::Continuation {
                        record_start_offset: 19,
                        record_end_offset: 30,
                        next_fragment_index: 1,
                    },
                    new_last_seen_time_unix_nano: 5,
                    finalize: false,
                })],
            },
            NAMESPACE,
        )
        .unwrap_err();
    assert!(matches!(error, ApplyError::ImpossibleTransition { .. }));
    assert_eq!(table.get(&file_id).unwrap().committed_offset, 20);
}

/// Scenario: progress advances an open-ended scan-to-LF continuation.
/// Guarantees: replay permits only an advanced instance of the same scan,
/// `Clean` after the runtime proves a boundary, or a later continuation that
/// starts at or after the prior committed offset.
#[test]
fn advancing_progress_enforces_open_ended_continuation_transitions() {
    let file_id = FileId([97; 16]);
    let mut table = CheckpointTable::from_snapshot_records(vec![SnapshotRecord {
        file_id,
        file_epoch: 1,
        committed_offset: 10,
        committed_frontier_guard: zero_guard(10),
        fingerprint: b"fp".to_vec(),
        ignored_header_bytes: 0,
        locator: Locator::PosixDevIno { dev: 1, ino: 2 },
        framing_profile_version: 1,
        framing_profile_digest: sample_digest(),
        framing_resume: FramingResume::Continuation {
            record_start_offset: 0,
            record_end_offset: 0,
            next_fragment_index: 1,
        },
        lifecycle_state: LifecycleState::Active,
        quarantine_evidence: None,
        last_seen_time_unix_nano: 1,
        advisory_path: AdvisoryPath::unavailable(),
    }])
    .unwrap();
    let error = table
        .apply_transaction(
            &Transaction {
                sequence: 2,
                operations: vec![Operation::UpdateProgress(UpdateProgress {
                    file_id,
                    expected_committed_offset: 10,
                    expected_file_epoch: 1,
                    new_committed_offset: 15,
                    new_committed_frontier_guard: zero_guard(15),
                    new_framing_resume: FramingResume::Continuation {
                        record_start_offset: 5,
                        record_end_offset: 0,
                        next_fragment_index: 2,
                    },
                    new_last_seen_time_unix_nano: 2,
                    finalize: false,
                })],
            },
            NAMESPACE,
        )
        .unwrap_err();
    assert!(matches!(error, ApplyError::ImpossibleTransition { .. }));

    table
        .apply_transaction(
            &Transaction {
                sequence: 2,
                operations: vec![Operation::UpdateProgress(UpdateProgress {
                    file_id,
                    expected_committed_offset: 10,
                    expected_file_epoch: 1,
                    new_committed_offset: 15,
                    new_committed_frontier_guard: zero_guard(15),
                    new_framing_resume: FramingResume::Continuation {
                        record_start_offset: 0,
                        record_end_offset: 0,
                        next_fragment_index: 2,
                    },
                    new_last_seen_time_unix_nano: 2,
                    finalize: false,
                })],
            },
            NAMESPACE,
        )
        .unwrap();
    table
        .apply_transaction(
            &Transaction {
                sequence: 3,
                operations: vec![Operation::UpdateProgress(UpdateProgress {
                    file_id,
                    expected_committed_offset: 15,
                    expected_file_epoch: 1,
                    new_committed_offset: 16,
                    new_committed_frontier_guard: zero_guard(16),
                    new_framing_resume: FramingResume::Clean,
                    new_last_seen_time_unix_nano: 3,
                    finalize: false,
                })],
            },
            NAMESPACE,
        )
        .unwrap();
    assert_eq!(
        table.get(&file_id).unwrap().framing_resume,
        FramingResume::Clean
    );
}

/// Scenario: `update_progress` with `finalize = true` transitions a record
/// to `RotatedFinalized`; a subsequent `update_progress` is then replayed
/// against that finalized record.
/// Guarantees: `RotatedFinalized` is terminal for `update_progress` -- a
/// later attempt to advance it fails closed rather than being treated as a
/// further Ack.
#[test]
fn update_progress_on_rotated_finalized_record_fails_closed() {
    let file_id = FileId([5; 16]);
    let mut table = CheckpointTable::new();
    table
        .apply_transaction(
            &Transaction {
                sequence: 1,
                operations: vec![Operation::RegisterFile(sample_register(file_id))],
            },
            NAMESPACE,
        )
        .unwrap();
    let finalize = Operation::UpdateProgress(UpdateProgress {
        file_id,
        expected_committed_offset: 0,
        expected_file_epoch: 1,
        new_committed_offset: 10,
        new_committed_frontier_guard: zero_guard(10),
        new_framing_resume: FramingResume::Clean,
        new_last_seen_time_unix_nano: 2,
        finalize: true,
    });
    table
        .apply_transaction(
            &Transaction {
                sequence: 2,
                operations: vec![finalize],
            },
            NAMESPACE,
        )
        .unwrap();
    assert_eq!(
        table.get(&file_id).unwrap().lifecycle_state,
        LifecycleState::RotatedFinalized
    );

    let further = Operation::UpdateProgress(UpdateProgress {
        file_id,
        expected_committed_offset: 10,
        expected_file_epoch: 1,
        new_committed_offset: 20,
        new_committed_frontier_guard: zero_guard(20),
        new_framing_resume: FramingResume::Clean,
        new_last_seen_time_unix_nano: 3,
        finalize: false,
    });
    let err = table
        .apply_transaction(
            &Transaction {
                sequence: 3,
                operations: vec![further],
            },
            NAMESPACE,
        )
        .unwrap_err();
    assert!(matches!(err, ApplyError::ImpossibleTransition { .. }));
}

/// Scenario: `reset_after_truncate` applied to an active record at epoch 1.
/// Guarantees: it is the only non-administrative operation that increments
/// `file_epoch`; the record ends at epoch 2, offset 0, `Clean` resume.
#[test]
fn reset_after_truncate_increments_epoch_and_resets_offset() {
    let file_id = FileId([6; 16]);
    let mut table = CheckpointTable::new();
    let mut register = sample_register(file_id);
    register.committed_offset = 500;
    register.committed_frontier_guard = zero_guard(500);
    table
        .apply_transaction(
            &Transaction {
                sequence: 1,
                operations: vec![Operation::RegisterFile(register)],
            },
            NAMESPACE,
        )
        .unwrap();
    let reset = Operation::ResetAfterTruncate(ResetAfterTruncate {
        file_id,
        expected_active_epoch: 1,
        observed_truncated_size: 10,
        resulting_epoch: 2,
        new_committed_offset: 0,
        new_framing_resume: FramingResume::Clean,
        new_fingerprint: b"replacement".to_vec(),
        reset_time_unix_nano: 99,
        reason_code: TRUNCATE_RESET_REASON_READ_NEW,
    });
    table
        .apply_transaction(
            &Transaction {
                sequence: 2,
                operations: vec![reset],
            },
            NAMESPACE,
        )
        .unwrap();
    let record = table.get(&file_id).unwrap();
    assert_eq!(record.file_epoch, 2);
    assert_eq!(record.committed_offset, 0);
    assert_eq!(record.fingerprint, b"replacement");
    assert_eq!(record.lifecycle_state, LifecycleState::Active);
}

/// Scenario: `reset_after_truncate` carrying a `reason_code` other than
/// `TRUNCATE_RESET_REASON_READ_NEW`.
/// Guarantees: the reason code is validated as an apply-time business rule
/// (not a decode-time structural check); an invalid reason fails replay
/// closed with `InvalidTruncateReason`.
#[test]
fn reset_after_truncate_rejects_invalid_reason_code() {
    let file_id = FileId([7; 16]);
    let mut table = CheckpointTable::new();
    table
        .apply_transaction(
            &Transaction {
                sequence: 1,
                operations: vec![Operation::RegisterFile(sample_register(file_id))],
            },
            NAMESPACE,
        )
        .unwrap();
    let reset = Operation::ResetAfterTruncate(ResetAfterTruncate {
        file_id,
        expected_active_epoch: 1,
        observed_truncated_size: 10,
        resulting_epoch: 2,
        new_committed_offset: 0,
        new_framing_resume: FramingResume::Clean,
        new_fingerprint: b"replacement".to_vec(),
        reset_time_unix_nano: 99,
        reason_code: 0x0002, // not TRUNCATE_RESET_REASON_READ_NEW
    });
    let err = table
        .apply_transaction(
            &Transaction {
                sequence: 2,
                operations: vec![reset],
            },
            NAMESPACE,
        )
        .unwrap_err();
    assert!(matches!(err, ApplyError::InvalidTruncateReason { .. }));
}

/// Scenario: `update_fingerprint` whose `expected_fingerprint` does not
/// match the currently stored fingerprint bytes.
/// Guarantees: replay rejects the update rather than blindly overwriting
/// matching evidence based on stale expectations.
#[test]
fn update_fingerprint_requires_matching_expected_bytes() {
    let file_id = FileId([8; 16]);
    let mut table = CheckpointTable::new();
    table
        .apply_transaction(
            &Transaction {
                sequence: 1,
                operations: vec![Operation::RegisterFile(sample_register(file_id))],
            },
            NAMESPACE,
        )
        .unwrap();
    let update = Operation::UpdateFingerprint(UpdateFingerprint {
        file_id,
        expected_file_epoch: 1,
        expected_fingerprint: b"wrong".to_vec(),
        new_fingerprint: b"new".to_vec(),
    });
    let err = table
        .apply_transaction(
            &Transaction {
                sequence: 2,
                operations: vec![update],
            },
            NAMESPACE,
        )
        .unwrap_err();
    assert!(matches!(err, ApplyError::ImpossibleTransition { .. }));
}

/// Scenario: same-epoch fingerprint updates attempt a no-op, shrink,
/// conflicting replacement, and a strict prefix extension.
/// Guarantees: only the strict same-stream extension is applied; every
/// rejected update leaves the original fingerprint unchanged.
#[test]
fn update_fingerprint_requires_strict_prefix_extension() {
    let file_id = FileId([98; 16]);
    assert!(matches!(
        Operation::UpdateFingerprint(UpdateFingerprint {
            file_id,
            expected_file_epoch: 1,
            expected_fingerprint: b"fp".to_vec(),
            new_fingerprint: b"fp".to_vec(),
        })
        .encode()
        .unwrap_err(),
        EncodeError::InvalidFieldValue {
            field: "update_fingerprint.new_fingerprint",
            ..
        }
    ));
    let mut table = CheckpointTable::new();
    table
        .apply_transaction(
            &Transaction {
                sequence: 1,
                operations: vec![Operation::RegisterFile(sample_register(file_id))],
            },
            NAMESPACE,
        )
        .unwrap();

    for new_fingerprint in [b"fp".to_vec(), b"f".to_vec(), b"fxp".to_vec()] {
        let error = table
            .apply_transaction(
                &Transaction {
                    sequence: 2,
                    operations: vec![Operation::UpdateFingerprint(UpdateFingerprint {
                        file_id,
                        expected_file_epoch: 1,
                        expected_fingerprint: b"fp".to_vec(),
                        new_fingerprint,
                    })],
                },
                NAMESPACE,
            )
            .unwrap_err();
        assert!(matches!(error, ApplyError::ImpossibleTransition { .. }));
        assert_eq!(table.get(&file_id).unwrap().fingerprint, b"fp");
    }

    table
        .apply_transaction(
            &Transaction {
                sequence: 2,
                operations: vec![Operation::UpdateFingerprint(UpdateFingerprint {
                    file_id,
                    expected_file_epoch: 1,
                    expected_fingerprint: b"fp".to_vec(),
                    new_fingerprint: b"fp-more".to_vec(),
                })],
            },
            NAMESPACE,
        )
        .unwrap();
    assert_eq!(table.get(&file_id).unwrap().fingerprint, b"fp-more");
}

/// Scenario: `update_metadata` is replayed against a `Quarantined` record.
/// Guarantees: `update_metadata` never carries a locator field on the wire
/// (the locator is immutable for a `file_id`), so the quarantined record's
/// immutable quarantine locator, lifecycle state, and failure evidence are
/// left untouched, while `last_seen_time_unix_nano` and `advisory_path`
/// still update.
#[test]
fn update_metadata_leaves_quarantine_locator_immutable() {
    let file_id = FileId([9; 16]);
    let mut table = CheckpointTable::new();
    table
        .apply_transaction(
            &Transaction {
                sequence: 1,
                operations: vec![Operation::RegisterFile(sample_register(file_id))],
            },
            NAMESPACE,
        )
        .unwrap();
    // `quarantine_file`'s locator MUST equal the stored (registered)
    // locator; it never rebinds identity at quarantine time.
    let quarantine_locator = Locator::PosixDevIno { dev: 1, ino: 2 };
    table
        .apply_transaction(
            &Transaction {
                sequence: 2,
                operations: vec![Operation::QuarantineFile(QuarantineFile {
                    file_id,
                    expected_file_epoch: 1,
                    reason_code: 0x0001,
                    locator: quarantine_locator,
                    observed_size: 10,
                    quarantine_epoch: 1,
                    quarantine_time_unix_nano: 5,
                })],
            },
            NAMESPACE,
        )
        .unwrap();

    let update = Operation::UpdateMetadata(UpdateMetadata {
        file_id,
        expected_prior_state: LifecycleState::Quarantined,
        expected_file_epoch: 1,
        last_seen_time_unix_nano: 42,
        advisory_path: Some(AdvisoryPath::from_unix_bytes(b"/new/path.log").unwrap()),
    });
    table
        .apply_transaction(
            &Transaction {
                sequence: 3,
                operations: vec![update],
            },
            NAMESPACE,
        )
        .unwrap();

    let record = table.get(&file_id).unwrap();
    assert_eq!(record.locator, quarantine_locator);
    assert_eq!(record.lifecycle_state, LifecycleState::Quarantined);
    assert_eq!(
        record.advisory_path,
        AdvisoryPath::from_unix_bytes(b"/new/path.log").unwrap()
    );
    assert_eq!(record.last_seen_time_unix_nano, 42);
}

/// Scenario: metadata updates carry a stale lifecycle or epoch across a
/// quarantine transition.
/// Guarantees: both guards are checked before advisory metadata changes and
/// the current quarantined record remains byte-identical on failure.
#[test]
fn update_metadata_requires_matching_lifecycle_and_epoch() {
    let file_id = FileId([99; 16]);
    let mut table = CheckpointTable::new();
    table
        .apply_transaction(
            &Transaction {
                sequence: 1,
                operations: vec![Operation::RegisterFile(sample_register(file_id))],
            },
            NAMESPACE,
        )
        .unwrap();
    table
        .apply_transaction(
            &Transaction {
                sequence: 2,
                operations: vec![Operation::QuarantineFile(QuarantineFile {
                    file_id,
                    expected_file_epoch: 1,
                    reason_code: 1,
                    locator: Locator::PosixDevIno { dev: 1, ino: 2 },
                    observed_size: 1,
                    quarantine_epoch: 1,
                    quarantine_time_unix_nano: 2,
                })],
            },
            NAMESPACE,
        )
        .unwrap();
    let before = table.get(&file_id).unwrap().clone();

    for (expected_prior_state, expected_file_epoch) in [
        (LifecycleState::Active, 1),
        (LifecycleState::Quarantined, 2),
    ] {
        let error = table
            .apply_transaction(
                &Transaction {
                    sequence: 3,
                    operations: vec![Operation::UpdateMetadata(UpdateMetadata {
                        file_id,
                        expected_prior_state,
                        expected_file_epoch,
                        last_seen_time_unix_nano: 99,
                        advisory_path: Some(AdvisoryPath::unavailable()),
                    })],
                },
                NAMESPACE,
            )
            .unwrap_err();
        assert!(matches!(error, ApplyError::ImpossibleTransition { .. }));
        assert_eq!(table.get(&file_id), Some(&before));
    }
}

/// Scenario: `quarantine_file` is applied against an `Active` record whose
/// operation `locator` differs from the record's stored locator.
/// Guarantees: replay fails closed with `ImpossibleTransition` and never
/// rebinds the stored locator to the operation's differing value; the
/// record remains `Active` at its original locator.
#[test]
fn quarantine_file_divergent_locator_fails_closed() {
    let file_id = FileId([12; 16]);
    let mut table = CheckpointTable::new();
    table
        .apply_transaction(
            &Transaction {
                sequence: 1,
                operations: vec![Operation::RegisterFile(sample_register(file_id))],
            },
            NAMESPACE,
        )
        .unwrap();

    let divergent_locator = Locator::PosixDevIno { dev: 99, ino: 99 };
    let err = table
        .apply_transaction(
            &Transaction {
                sequence: 2,
                operations: vec![Operation::QuarantineFile(QuarantineFile {
                    file_id,
                    expected_file_epoch: 1,
                    reason_code: 0x0001,
                    locator: divergent_locator,
                    observed_size: 10,
                    quarantine_epoch: 1,
                    quarantine_time_unix_nano: 5,
                })],
            },
            NAMESPACE,
        )
        .unwrap_err();
    assert!(matches!(err, ApplyError::ImpossibleTransition { .. }));

    let record = table.get(&file_id).unwrap();
    assert_eq!(record.lifecycle_state, LifecycleState::Active);
    assert_eq!(record.locator, Locator::PosixDevIno { dev: 1, ino: 2 });
}

/// Scenario: an identical `quarantine_file` operation is replayed twice
/// against the same record; a third replay changes the reason code.
/// Guarantees: the identical replay is idempotent; a conflicting replay
/// fails closed rather than silently overwriting quarantine evidence.
#[test]
fn quarantine_file_identical_replay_is_idempotent_but_conflict_fails_closed() {
    let file_id = FileId([11; 16]);
    let mut table = CheckpointTable::new();
    table
        .apply_transaction(
            &Transaction {
                sequence: 1,
                operations: vec![Operation::RegisterFile(sample_register(file_id))],
            },
            NAMESPACE,
        )
        .unwrap();
    let quarantine = QuarantineFile {
        file_id,
        expected_file_epoch: 1,
        reason_code: 0x0001,
        locator: Locator::PosixDevIno { dev: 1, ino: 2 },
        observed_size: 1,
        quarantine_epoch: 1,
        quarantine_time_unix_nano: 1,
    };
    table
        .apply_transaction(
            &Transaction {
                sequence: 2,
                operations: vec![Operation::QuarantineFile(quarantine.clone())],
            },
            NAMESPACE,
        )
        .unwrap();
    table
        .apply_transaction(
            &Transaction {
                sequence: 3,
                operations: vec![Operation::QuarantineFile(quarantine.clone())],
            },
            NAMESPACE,
        )
        .expect("identical quarantine replay is idempotent");

    let mut conflicting = quarantine;
    conflicting.reason_code = 0x0002;
    let err = table
        .apply_transaction(
            &Transaction {
                sequence: 4,
                operations: vec![Operation::QuarantineFile(conflicting)],
            },
            NAMESPACE,
        )
        .unwrap_err();
    assert!(matches!(err, ApplyError::ConflictingQuarantine { .. }));
}

/// Scenario: `reset_quarantined_file` with `action = reset_to_beginning`.
/// Guarantees: the record returns to `Active` at offset `0` with an
/// incremented epoch, and quarantine evidence is cleared.
#[test]
fn reset_quarantined_file_reset_to_beginning_returns_active_at_zero() {
    let file_id = FileId([12; 16]);
    let mut table = CheckpointTable::new();
    table
        .apply_transaction(
            &Transaction {
                sequence: 1,
                operations: vec![Operation::RegisterFile(sample_register(file_id))],
            },
            NAMESPACE,
        )
        .unwrap();
    table
        .apply_transaction(
            &Transaction {
                sequence: 2,
                operations: vec![Operation::QuarantineFile(QuarantineFile {
                    file_id,
                    expected_file_epoch: 1,
                    reason_code: 0x0001,
                    locator: Locator::PosixDevIno { dev: 1, ino: 2 },
                    observed_size: 1,
                    quarantine_epoch: 1,
                    quarantine_time_unix_nano: 1,
                })],
            },
            NAMESPACE,
        )
        .unwrap();
    table
        .apply_transaction(
            &Transaction {
                sequence: 3,
                operations: vec![Operation::ResetQuarantinedFile(ResetQuarantinedFile {
                    file_id,
                    expected_quarantine_epoch: 1,
                    action: ResetQuarantineAction::ResetToBeginning,
                    resulting_epoch: 2,
                    resulting_offset: 0,
                    new_committed_frontier_guard: zero_guard(0),
                    new_framing_resume: FramingResume::Clean,
                    new_fingerprint: b"replacement".to_vec(),
                    action_time_unix_nano: 7,
                    namespace_id: NAMESPACE.to_owned(),
                    audit_reason: "operator approved".to_owned(),
                })],
            },
            NAMESPACE,
        )
        .unwrap();
    let record = table.get(&file_id).unwrap();
    assert_eq!(record.lifecycle_state, LifecycleState::Active);
    assert_eq!(record.file_epoch, 2);
    assert_eq!(record.committed_offset, 0);
    assert_eq!(record.fingerprint, b"replacement");
    assert!(record.quarantine_evidence.is_none());
}

/// Scenario: `reset_quarantined_file` with `action = keep_failed` but a
/// `resulting_offset` that differs from the record's stored committed
/// offset.
/// Guarantees: `keep_failed` cannot be used to smuggle a silent state change
/// through a nominally no-op audit action.
#[test]
fn reset_quarantined_file_keep_failed_requires_unchanged_fields() {
    let file_id = FileId([13; 16]);
    let mut table = CheckpointTable::new();
    table
        .apply_transaction(
            &Transaction {
                sequence: 1,
                operations: vec![Operation::RegisterFile(sample_register(file_id))],
            },
            NAMESPACE,
        )
        .unwrap();
    table
        .apply_transaction(
            &Transaction {
                sequence: 2,
                operations: vec![Operation::QuarantineFile(QuarantineFile {
                    file_id,
                    expected_file_epoch: 1,
                    reason_code: 0x0001,
                    locator: Locator::PosixDevIno { dev: 1, ino: 2 },
                    observed_size: 1,
                    quarantine_epoch: 1,
                    quarantine_time_unix_nano: 1,
                })],
            },
            NAMESPACE,
        )
        .unwrap();
    let tampered_keep_failed = Operation::ResetQuarantinedFile(ResetQuarantinedFile {
        file_id,
        expected_quarantine_epoch: 1,
        action: ResetQuarantineAction::KeepFailed,
        resulting_epoch: 1,
        resulting_offset: 999, // differs from the stored committed_offset (0)
        new_committed_frontier_guard: zero_guard(999),
        new_framing_resume: FramingResume::Clean,
        new_fingerprint: b"fp".to_vec(),
        action_time_unix_nano: 7,
        namespace_id: NAMESPACE.to_owned(),
        audit_reason: "operator declined".to_owned(),
    });
    let err = table
        .apply_transaction(
            &Transaction {
                sequence: 3,
                operations: vec![tampered_keep_failed],
            },
            NAMESPACE,
        )
        .unwrap_err();
    assert!(matches!(err, ApplyError::ImpossibleTransition { .. }));
}

/// Scenario: `keep_failed` carries a replacement fingerprint that differs
/// from the quarantined record while every offset/epoch field still matches.
/// Guarantees: audit-only quarantine retention cannot change fingerprint or
/// any other operational state.
#[test]
fn reset_quarantined_file_keep_failed_preserves_fingerprint() {
    let file_id = FileId([100; 16]);
    let mut table = CheckpointTable::new();
    table
        .apply_transaction(
            &Transaction {
                sequence: 1,
                operations: vec![Operation::RegisterFile(sample_register(file_id))],
            },
            NAMESPACE,
        )
        .unwrap();
    table
        .apply_transaction(
            &Transaction {
                sequence: 2,
                operations: vec![Operation::QuarantineFile(QuarantineFile {
                    file_id,
                    expected_file_epoch: 1,
                    reason_code: 1,
                    locator: Locator::PosixDevIno { dev: 1, ino: 2 },
                    observed_size: 1,
                    quarantine_epoch: 1,
                    quarantine_time_unix_nano: 2,
                })],
            },
            NAMESPACE,
        )
        .unwrap();
    let before = table.get(&file_id).unwrap().clone();
    let error = table
        .apply_transaction(
            &Transaction {
                sequence: 3,
                operations: vec![Operation::ResetQuarantinedFile(ResetQuarantinedFile {
                    file_id,
                    expected_quarantine_epoch: 1,
                    action: ResetQuarantineAction::KeepFailed,
                    resulting_epoch: 1,
                    resulting_offset: 0,
                    new_committed_frontier_guard: CommittedFrontierGuard::empty(),
                    new_framing_resume: FramingResume::Clean,
                    new_fingerprint: b"different".to_vec(),
                    action_time_unix_nano: 3,
                    namespace_id: NAMESPACE.to_owned(),
                    audit_reason: "retain".to_owned(),
                })],
            },
            NAMESPACE,
        )
        .unwrap_err();
    assert!(matches!(error, ApplyError::KeepFailedStateChange { .. }));
    assert_eq!(table.get(&file_id), Some(&before));
}

/// Scenario: a quarantine reset names the wrong namespace for an existing
/// record and for an absent `file_id`.
/// Guarantees: namespace validation runs before record lookup or idempotency,
/// and neither operation changes the table.
#[test]
fn reset_quarantined_file_validates_namespace_before_lookup() {
    let file_id = FileId([101; 16]);
    let mut table = CheckpointTable::new();
    table
        .apply_transaction(
            &Transaction {
                sequence: 1,
                operations: vec![Operation::RegisterFile(sample_register(file_id))],
            },
            NAMESPACE,
        )
        .unwrap();
    table
        .apply_transaction(
            &Transaction {
                sequence: 2,
                operations: vec![Operation::QuarantineFile(QuarantineFile {
                    file_id,
                    expected_file_epoch: 1,
                    reason_code: 1,
                    locator: Locator::PosixDevIno { dev: 1, ino: 2 },
                    observed_size: 1,
                    quarantine_epoch: 1,
                    quarantine_time_unix_nano: 2,
                })],
            },
            NAMESPACE,
        )
        .unwrap();
    let before = table.clone();

    for target in [file_id, FileId([102; 16])] {
        let error = table
            .apply_transaction(
                &Transaction {
                    sequence: 3,
                    operations: vec![Operation::ResetQuarantinedFile(ResetQuarantinedFile {
                        file_id: target,
                        expected_quarantine_epoch: 1,
                        action: ResetQuarantineAction::KeepFailed,
                        resulting_epoch: 1,
                        resulting_offset: 0,
                        new_committed_frontier_guard: CommittedFrontierGuard::empty(),
                        new_framing_resume: FramingResume::Clean,
                        new_fingerprint: b"fp".to_vec(),
                        action_time_unix_nano: 3,
                        namespace_id: "wrong".to_owned(),
                        audit_reason: "retain".to_owned(),
                    })],
                },
                NAMESPACE,
            )
            .unwrap_err();
        assert!(matches!(error, ApplyError::NamespaceMismatch { .. }));
        assert_eq!(table, before);
    }
}

/// Scenario: `reset_quarantined_file` naming
/// `expected_quarantine_epoch = u32::MAX` with `action = reset_to_beginning`.
/// Guarantees: the epoch increment uses checked arithmetic and fails closed
/// on overflow rather than wrapping to a smaller epoch value.
#[test]
fn reset_quarantined_file_epoch_overflow_fails_closed() {
    let file_id = FileId([14; 16]);
    let mut table = CheckpointTable::new();
    let mut register = sample_register(file_id);
    register.file_epoch = 1;
    table
        .apply_transaction(
            &Transaction {
                sequence: 1,
                operations: vec![Operation::RegisterFile(register)],
            },
            NAMESPACE,
        )
        .unwrap();
    table
        .apply_transaction(
            &Transaction {
                sequence: 2,
                operations: vec![Operation::QuarantineFile(QuarantineFile {
                    file_id,
                    expected_file_epoch: 1,
                    reason_code: 0x0001,
                    locator: Locator::PosixDevIno { dev: 1, ino: 2 },
                    observed_size: 1,
                    quarantine_epoch: 1,
                    quarantine_time_unix_nano: 1,
                })],
            },
            NAMESPACE,
        )
        .unwrap();

    // Manually push the record's quarantine_epoch to u32::MAX to exercise
    // the overflow guard without needing u32::MAX truncate/reset cycles.
    {
        let records = table.snapshot_records();
        let mut record = records.into_iter().find(|r| r.file_id == file_id).unwrap();
        record.file_epoch = u32::MAX;
        record.quarantine_evidence = Some(QuarantineEvidence {
            reason_code: 0x0001,
            observed_size: 1,
            quarantine_epoch: u32::MAX,
            quarantine_time_unix_nano: 1,
        });
        table = CheckpointTable::from_snapshot_records(vec![record]).unwrap();
    }

    let overflow_reset = Operation::ResetQuarantinedFile(ResetQuarantinedFile {
        file_id,
        expected_quarantine_epoch: u32::MAX,
        action: ResetQuarantineAction::ResetToBeginning,
        resulting_epoch: 0, // irrelevant; overflow is detected before comparison
        resulting_offset: 0,
        new_committed_frontier_guard: zero_guard(0),
        new_framing_resume: FramingResume::Clean,
        new_fingerprint: b"replacement".to_vec(),
        action_time_unix_nano: 7,
        namespace_id: NAMESPACE.to_owned(),
        audit_reason: "operator approved".to_owned(),
    });
    let err = table
        .apply_transaction(
            &Transaction {
                sequence: 3,
                operations: vec![overflow_reset],
            },
            NAMESPACE,
        )
        .unwrap_err();
    assert!(matches!(err, ApplyError::EpochOverflow { .. }));
}

/// Scenario: administrative `remove_file` names the wrong namespace for an
/// existing quarantined record and an absent `file_id`.
/// Guarantees: exact namespace validation precedes lookup and absent-target
/// idempotency, so neither mismatch removes state.
#[test]
fn remove_file_administrative_namespace_mismatch_fails_closed() {
    let file_id = FileId([15; 16]);
    let mut table = CheckpointTable::new();
    table
        .apply_transaction(
            &Transaction {
                sequence: 1,
                operations: vec![Operation::RegisterFile(sample_register(file_id))],
            },
            NAMESPACE,
        )
        .unwrap();
    table
        .apply_transaction(
            &Transaction {
                sequence: 2,
                operations: vec![Operation::QuarantineFile(QuarantineFile {
                    file_id,
                    expected_file_epoch: 1,
                    reason_code: 0x0001,
                    locator: Locator::PosixDevIno { dev: 1, ino: 2 },
                    observed_size: 1,
                    quarantine_epoch: 1,
                    quarantine_time_unix_nano: 1,
                })],
            },
            NAMESPACE,
        )
        .unwrap();

    let remove = Operation::RemoveFile(RemoveFile {
        file_id,
        expected_file_epoch: 1,
        expected_prior_state: LifecycleState::Quarantined,
        removal_reason: 0x0001,
        removal_time_unix_nano: 9,
        administrative: true,
        namespace_id: Some("wrong-namespace".to_owned()),
        audit_reason: Some("operator cleanup".to_owned()),
    });
    let err = table
        .apply_transaction(
            &Transaction {
                sequence: 3,
                operations: vec![remove],
            },
            NAMESPACE,
        )
        .unwrap_err();
    assert!(matches!(err, ApplyError::NamespaceMismatch { .. }));
    assert!(table.get(&file_id).is_some());

    let absent = Operation::RemoveFile(RemoveFile {
        file_id: FileId([200; 16]),
        expected_file_epoch: 1,
        expected_prior_state: LifecycleState::Quarantined,
        removal_reason: 1,
        removal_time_unix_nano: 9,
        administrative: true,
        namespace_id: Some("wrong-namespace".to_owned()),
        audit_reason: Some("operator cleanup".to_owned()),
    });
    let err = table
        .apply_transaction(
            &Transaction {
                sequence: 3,
                operations: vec![absent],
            },
            NAMESPACE,
        )
        .unwrap_err();
    assert!(matches!(err, ApplyError::NamespaceMismatch { .. }));
    assert!(table.get(&file_id).is_some());
}

/// Scenario: a non-administrative `remove_file` targets a `Quarantined`
/// record.
/// Guarantees: ordinary retention can never remove quarantined state,
/// regardless of how the epoch/state fields otherwise match.
#[test]
fn remove_file_ordinary_retention_cannot_remove_quarantined_state() {
    let file_id = FileId([16; 16]);
    let mut table = CheckpointTable::new();
    table
        .apply_transaction(
            &Transaction {
                sequence: 1,
                operations: vec![Operation::RegisterFile(sample_register(file_id))],
            },
            NAMESPACE,
        )
        .unwrap();
    table
        .apply_transaction(
            &Transaction {
                sequence: 2,
                operations: vec![Operation::QuarantineFile(QuarantineFile {
                    file_id,
                    expected_file_epoch: 1,
                    reason_code: 0x0001,
                    locator: Locator::PosixDevIno { dev: 1, ino: 2 },
                    observed_size: 1,
                    quarantine_epoch: 1,
                    quarantine_time_unix_nano: 1,
                })],
            },
            NAMESPACE,
        )
        .unwrap();
    let ordinary_remove = Operation::RemoveFile(RemoveFile {
        file_id,
        expected_file_epoch: 1,
        expected_prior_state: LifecycleState::Quarantined,
        removal_reason: 0x0001,
        removal_time_unix_nano: 9,
        administrative: false,
        namespace_id: None,
        audit_reason: None,
    });
    let err = table
        .apply_transaction(
            &Transaction {
                sequence: 3,
                operations: vec![ordinary_remove],
            },
            NAMESPACE,
        )
        .unwrap_err();
    assert!(matches!(err, ApplyError::ImpossibleTransition { .. }));
}

/// Scenario: a non-administrative removal is attempted alone, then paired
/// atomically with one new registration for the removed record's exact
/// locator.
/// Guarantees: lone/absent removal fails closed, while the exact supersede
/// transaction transfers the single live locator claim without exposing a
/// duplicate or deleting unrelated state.
#[test]
fn non_administrative_remove_requires_exact_locator_supersede() {
    let old_file_id = FileId([103; 16]);
    let new_file_id = FileId([104; 16]);
    let locator = Locator::PosixDevIno { dev: 7, ino: 9 };
    let mut old_register = sample_register(old_file_id);
    old_register.locator = locator;
    let mut table = CheckpointTable::new();
    table
        .apply_transaction(
            &Transaction {
                sequence: 1,
                operations: vec![Operation::RegisterFile(old_register)],
            },
            NAMESPACE,
        )
        .unwrap();
    let removal = Operation::RemoveFile(RemoveFile {
        file_id: old_file_id,
        expected_file_epoch: 1,
        expected_prior_state: LifecycleState::Active,
        removal_reason: 1,
        removal_time_unix_nano: 2,
        administrative: false,
        namespace_id: None,
        audit_reason: None,
    });

    let error = table
        .apply_transaction(
            &Transaction {
                sequence: 2,
                operations: vec![removal.clone()],
            },
            NAMESPACE,
        )
        .unwrap_err();
    assert!(matches!(error, ApplyError::ImpossibleTransition { .. }));
    assert!(table.get(&old_file_id).is_some());

    let mut replacement = sample_register(new_file_id);
    replacement.locator = locator;
    replacement.fingerprint = b"replacement".to_vec();
    let remove_replacement = Operation::RemoveFile(RemoveFile {
        file_id: new_file_id,
        expected_file_epoch: 1,
        expected_prior_state: LifecycleState::Active,
        removal_reason: 2,
        removal_time_unix_nano: 2,
        administrative: true,
        namespace_id: Some(NAMESPACE.to_owned()),
        audit_reason: Some("remove replacement".to_owned()),
    });
    let error = table
        .apply_transaction(
            &Transaction {
                sequence: 2,
                operations: vec![
                    Operation::RegisterFile(replacement.clone()),
                    removal.clone(),
                    remove_replacement,
                ],
            },
            NAMESPACE,
        )
        .unwrap_err();
    assert!(matches!(error, ApplyError::ImpossibleTransition { .. }));
    assert!(table.get(&old_file_id).is_some());
    assert!(table.get(&new_file_id).is_none());

    table
        .apply_transaction(
            &Transaction {
                sequence: 2,
                operations: vec![Operation::RegisterFile(replacement), removal],
            },
            NAMESPACE,
        )
        .unwrap();
    assert!(table.get(&old_file_id).is_none());
    assert_eq!(table.get(&new_file_id).unwrap().fingerprint, b"replacement");

    let absent_removal = Operation::RemoveFile(RemoveFile {
        file_id: old_file_id,
        expected_file_epoch: 1,
        expected_prior_state: LifecycleState::Active,
        removal_reason: 1,
        removal_time_unix_nano: 3,
        administrative: false,
        namespace_id: None,
        audit_reason: None,
    });
    let error = table
        .apply_transaction(
            &Transaction {
                sequence: 3,
                operations: vec![absent_removal],
            },
            NAMESPACE,
        )
        .unwrap_err();
    assert!(matches!(error, ApplyError::ImpossibleTransition { .. }));
}

/// Scenario: an administrative `remove_file` naming the correct namespace
/// targets a `Quarantined` record.
/// Guarantees: this is the only path that can remove quarantined state, and
/// it succeeds when the namespace and evidence match exactly.
#[test]
fn remove_file_administrative_removes_quarantined_with_matching_namespace() {
    let file_id = FileId([17; 16]);
    let mut table = CheckpointTable::new();
    table
        .apply_transaction(
            &Transaction {
                sequence: 1,
                operations: vec![Operation::RegisterFile(sample_register(file_id))],
            },
            NAMESPACE,
        )
        .unwrap();
    table
        .apply_transaction(
            &Transaction {
                sequence: 2,
                operations: vec![Operation::QuarantineFile(QuarantineFile {
                    file_id,
                    expected_file_epoch: 1,
                    reason_code: 0x0001,
                    locator: Locator::PosixDevIno { dev: 1, ino: 2 },
                    observed_size: 1,
                    quarantine_epoch: 1,
                    quarantine_time_unix_nano: 1,
                })],
            },
            NAMESPACE,
        )
        .unwrap();
    let remove = Operation::RemoveFile(RemoveFile {
        file_id,
        expected_file_epoch: 1,
        expected_prior_state: LifecycleState::Quarantined,
        removal_reason: 0x0001,
        removal_time_unix_nano: 9,
        administrative: true,
        namespace_id: Some(NAMESPACE.to_owned()),
        audit_reason: Some("operator cleanup".to_owned()),
    });
    table
        .apply_transaction(
            &Transaction {
                sequence: 3,
                operations: vec![remove],
            },
            NAMESPACE,
        )
        .unwrap();
    assert!(table.get(&file_id).is_none());
}

/// Scenario: an administrative `remove_file` is replayed twice against the
/// same `file_id`; the second replay runs after the record is already absent.
/// Guarantees: namespace-validated administrative replay against an absent
/// `file_id` is idempotent rather than failing.
#[test]
fn remove_file_replay_against_absent_file_id_is_idempotent() {
    let file_id = FileId([18; 16]);
    let mut table = CheckpointTable::new();
    table
        .apply_transaction(
            &Transaction {
                sequence: 1,
                operations: vec![Operation::RegisterFile(sample_register(file_id))],
            },
            NAMESPACE,
        )
        .unwrap();
    let remove = || {
        Operation::RemoveFile(RemoveFile {
            file_id,
            expected_file_epoch: 1,
            expected_prior_state: LifecycleState::Active,
            removal_reason: 0x0001,
            removal_time_unix_nano: 9,
            administrative: true,
            namespace_id: Some(NAMESPACE.to_owned()),
            audit_reason: Some("operator removal".to_owned()),
        })
    };
    table
        .apply_transaction(
            &Transaction {
                sequence: 2,
                operations: vec![remove()],
            },
            NAMESPACE,
        )
        .unwrap();
    assert!(table.get(&file_id).is_none());
    table
        .apply_transaction(
            &Transaction {
                sequence: 3,
                operations: vec![remove()],
            },
            NAMESPACE,
        )
        .expect("replay against an already absent file_id is idempotent");
}

/// Scenario: a snapshot record and its equivalent standalone `SnapshotRecord`
/// value are compared after a round trip through `CheckpointTable`.
/// Guarantees: `CheckpointTable::snapshot_records` reproduces exactly the
/// records a snapshot would persist, so compaction can encode
/// `table.snapshot_records()` directly with `encode_snapshot`.
#[test]
fn checkpoint_table_snapshot_records_round_trip_through_encode_snapshot() {
    let file_id = FileId([19; 16]);
    let mut table = CheckpointTable::new();
    table
        .apply_transaction(
            &Transaction {
                sequence: 1,
                operations: vec![Operation::RegisterFile(sample_register(file_id))],
            },
            NAMESPACE,
        )
        .unwrap();
    let records = table.snapshot_records();
    let encoded = snapshot::encode_snapshot(1, TEST_NAMESPACE_ID, &records).unwrap();
    let decoded = snapshot::decode_snapshot(&encoded, &TEST_NAMESPACE_DIGEST).unwrap();
    assert_eq!(decoded.generation, 1);
    assert_eq!(decoded.records, records);
    assert_eq!(decoded.records[0].file_id, file_id);
}

/// Scenario: an `expected_prior_state` on `remove_file` that does not match
/// the record's actual current state.
/// Guarantees: `remove_file` only removes a matching record; a stale or
/// incorrect expected state fails closed rather than removing regardless.
#[test]
fn remove_file_rejects_mismatched_expected_prior_state() {
    let file_id = FileId([20; 16]);
    let mut table = CheckpointTable::new();
    table
        .apply_transaction(
            &Transaction {
                sequence: 1,
                operations: vec![Operation::RegisterFile(sample_register(file_id))],
            },
            NAMESPACE,
        )
        .unwrap();
    let remove = Operation::RemoveFile(RemoveFile {
        file_id,
        expected_file_epoch: 1,
        expected_prior_state: LifecycleState::RotatedFinalized, // actually Active
        removal_reason: 0x0001,
        removal_time_unix_nano: 9,
        administrative: false,
        namespace_id: None,
        audit_reason: None,
    });
    let err = table
        .apply_transaction(
            &Transaction {
                sequence: 2,
                operations: vec![remove],
            },
            NAMESPACE,
        )
        .unwrap_err();
    assert!(matches!(err, ApplyError::ImpossibleTransition { .. }));
    assert!(table.get(&file_id).is_some());
}

// ---------------------------------------------------------------------
// Negative / fail-closed conformance tests (unknown discriminants,
// reserved bits, structural bounds, and atomicity).
// ---------------------------------------------------------------------

/// Scenario: a snapshot record whose `locator.kind` byte has been
/// overwritten with an unrecognized value (`0xFF`), with the record's own
/// CRC-32C recomputed so the corruption is isolated to the discriminant.
/// Guarantees: decoding fails closed with `UnknownDiscriminant` for
/// `locator.kind`; an unrecognized structural discriminant is never
/// skipped or guessed at.
#[test]
fn unknown_locator_kind_fails_closed() {
    let record = sample_snapshot_record(FileId([100; 16]));
    let mut frame = record.encode().unwrap();
    frame[72] = 0xFF; // locator.kind (see sample_snapshot_record's fixed layout)
    refresh_frame_crc32c(&mut frame);
    let err = SnapshotRecord::decode(&frame).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::UnknownDiscriminant {
            field: "locator.kind",
            ..
        }
    ));
}

/// Scenario: a snapshot record whose `framing_resume.kind` byte has been
/// overwritten with an unrecognized value, with the record's own CRC-32C
/// recomputed so the corruption is isolated to the discriminant.
/// Guarantees: decoding fails closed with `UnknownDiscriminant` for
/// `framing_resume.kind`.
#[test]
fn unknown_framing_resume_kind_fails_closed() {
    let record = sample_snapshot_record(FileId([101; 16]));
    let mut frame = record.encode().unwrap();
    frame[123] = 0xFF; // framing_resume.kind (see sample_snapshot_record's fixed layout)
    refresh_frame_crc32c(&mut frame);
    let err = SnapshotRecord::decode(&frame).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::UnknownDiscriminant {
            field: "framing_resume.kind",
            ..
        }
    ));
}

/// Scenario: a snapshot record whose `lifecycle_state` byte has been
/// overwritten with the reserved, invalid value `0x00`, with the record's
/// own CRC-32C recomputed so the corruption is isolated to the
/// discriminant.
/// Guarantees: decoding fails closed with `UnknownDiscriminant` for
/// `snapshot_record.lifecycle_state`; there is no `Absent` lifecycle value.
#[test]
fn unknown_lifecycle_state_fails_closed() {
    let record = sample_snapshot_record(FileId([102; 16]));
    let mut frame = record.encode().unwrap();
    frame[124] = 0x00; // lifecycle_state (see sample_snapshot_record's fixed layout)
    refresh_frame_crc32c(&mut frame);
    let err = SnapshotRecord::decode(&frame).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::UnknownDiscriminant {
            field: "snapshot_record.lifecycle_state",
            ..
        }
    ));
}

/// Scenario: a WAL operation frame whose `op_code` byte has been
/// overwritten with an unrecognized value, with the operation's own
/// CRC-32C recomputed so the corruption is isolated to the discriminant.
/// Guarantees: decoding fails closed with `UnknownDiscriminant` for
/// `operation.op_code`.
#[test]
fn unknown_op_code_fails_closed() {
    let mut frame = Operation::RegisterFile(sample_register(FileId([103; 16])))
        .encode()
        .unwrap();
    frame[4] = 0xFF; // op_code is the first payload byte, right after op_len
    refresh_frame_crc32c(&mut frame);
    let err = Operation::decode(&frame).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::UnknownDiscriminant {
            field: "operation.op_code",
            ..
        }
    ));
}

/// Scenario: a `reset_quarantined_file` operation whose `action` byte has
/// been overwritten with an unrecognized value, with the operation's own
/// CRC-32C recomputed so the corruption is isolated to the discriminant.
/// Guarantees: decoding fails closed with `UnknownDiscriminant` for
/// `reset_quarantined_file.action`.
#[test]
fn unknown_reset_quarantine_action_fails_closed() {
    let op = ResetQuarantinedFile {
        file_id: FileId([104; 16]),
        expected_quarantine_epoch: 1,
        action: ResetQuarantineAction::ResetToBeginning,
        resulting_epoch: 2,
        resulting_offset: 0,
        new_committed_frontier_guard: zero_guard(0),
        new_framing_resume: FramingResume::Clean,
        new_fingerprint: b"replacement".to_vec(),
        action_time_unix_nano: 1,
        namespace_id: NAMESPACE.to_owned(),
        audit_reason: "operator approved".to_owned(),
    };
    let mut frame = Operation::ResetQuarantinedFile(op).encode().unwrap();
    // `action` follows op_code(1) + file_id(16) + expected_quarantine_epoch(4)
    // in the payload, plus the 4-byte op_len prefix: frame offset 25.
    frame[25] = 0xFF;
    refresh_frame_crc32c(&mut frame);
    let err = Operation::decode(&frame).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::UnknownDiscriminant {
            field: "reset_quarantined_file.action",
            ..
        }
    ));
}

/// Scenario: a `remove_file` operation whose `expected_prior_state` byte
/// has been overwritten with an unrecognized value, with the operation's
/// own CRC-32C recomputed so the corruption is isolated to the
/// discriminant.
/// Guarantees: decoding fails closed with `UnknownDiscriminant` for
/// `remove_file.expected_prior_state`.
#[test]
fn unknown_remove_file_expected_prior_state_fails_closed() {
    let op = RemoveFile {
        file_id: FileId([105; 16]),
        expected_file_epoch: 1,
        expected_prior_state: LifecycleState::Active,
        removal_reason: 1,
        removal_time_unix_nano: 1,
        administrative: false,
        namespace_id: None,
        audit_reason: None,
    };
    let mut frame = Operation::RemoveFile(op).encode().unwrap();
    // `expected_prior_state` follows op_code(1) + file_id(16) +
    // expected_file_epoch(4) in the payload, plus the 4-byte op_len prefix:
    // frame offset 25.
    frame[25] = 0xFF;
    refresh_frame_crc32c(&mut frame);
    let err = Operation::decode(&frame).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::UnknownDiscriminant {
            field: "remove_file.expected_prior_state",
            ..
        }
    ));
}

/// Scenario: a snapshot header whose reserved `flags` field has been set to
/// a nonzero value, with the header's own CRC-32C recomputed so the
/// corruption is isolated to `flags`.
/// Guarantees: decoding fails closed with `ReservedFieldNonZero` for
/// `snapshot_header.flags`; v1 defines no flag bits.
#[test]
fn snapshot_header_nonzero_flags_fails_closed() {
    let record = sample_snapshot_record(FileId([106; 16]));
    let mut bytes =
        snapshot::encode_snapshot(1, TEST_NAMESPACE_ID, std::slice::from_ref(&record)).unwrap();
    bytes[10..12].copy_from_slice(&1u16.to_be_bytes());
    let crc = crc32c(&bytes[0..56]);
    bytes[56..60].copy_from_slice(&crc.to_be_bytes());
    let err = snapshot::decode_snapshot(&bytes, &TEST_NAMESPACE_DIGEST).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::ReservedFieldNonZero {
            field: "snapshot_header.flags",
            ..
        }
    ));
}

/// Scenario: a WAL header whose reserved `flags` field has been set to a
/// nonzero value, with the header's own CRC-32C recomputed so the
/// corruption is isolated to `flags`.
/// Guarantees: decoding fails closed with `ReservedFieldNonZero` for
/// `wal_header.flags`; v1 defines no flag bits.
#[test]
fn wal_header_nonzero_flags_fails_closed() {
    let transaction = Transaction {
        sequence: 1,
        operations: vec![Operation::RegisterFile(sample_register(FileId([107; 16])))],
    };
    let mut bytes =
        wal::encode_wal(1, TEST_NAMESPACE_ID, std::slice::from_ref(&transaction)).unwrap();
    bytes[10..12].copy_from_slice(&1u16.to_be_bytes());
    let crc = crc32c(&bytes[0..20]);
    bytes[20..24].copy_from_slice(&crc.to_be_bytes());
    let err = wal::decode_wal(&bytes, &TEST_NAMESPACE_DIGEST).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::ReservedFieldNonZero {
            field: "wal_header.flags",
            ..
        }
    ));
}

/// Scenario: an `update_metadata` operation whose presence-flags byte has a
/// reserved bit set (`0x04`, outside `PATH_PRESENT`), with the operation's
/// own CRC-32C recomputed so the corruption is isolated to the presence
/// byte.
/// Guarantees: decoding fails closed with `ReservedFieldNonZero` for
/// `update_metadata.presence_flags` rather than silently ignoring an
/// unassigned presence bit.
#[test]
fn update_metadata_reserved_presence_bit_fails_closed() {
    let op = UpdateMetadata {
        file_id: FileId([108; 16]),
        expected_prior_state: LifecycleState::Active,
        expected_file_epoch: 1,
        last_seen_time_unix_nano: 1,
        advisory_path: None,
    };
    let mut frame = Operation::UpdateMetadata(op).encode().unwrap();
    // presence follows op_code(1) + file_id(16) + expected_prior_state(1)
    // + expected_file_epoch(4), plus the 4-byte op_len prefix.
    frame[26] = 0x04;
    refresh_frame_crc32c(&mut frame);
    let err = Operation::decode(&frame).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::ReservedFieldNonZero {
            field: "update_metadata.presence_flags",
            ..
        }
    ));
}

/// Scenario: `update_metadata.expected_prior_state` is constructed or decoded
/// as `RotatedFinalized`, which v1 does not permit for metadata mutation.
/// Guarantees: the encoder and decoder both fail at the structural boundary
/// rather than producing a metadata operation that replay could reinterpret.
#[test]
fn update_metadata_rejects_finalized_expected_state() {
    let operation = Operation::UpdateMetadata(UpdateMetadata {
        file_id: FileId([112; 16]),
        expected_prior_state: LifecycleState::RotatedFinalized,
        expected_file_epoch: 1,
        last_seen_time_unix_nano: 1,
        advisory_path: None,
    });
    assert!(matches!(
        operation.encode().unwrap_err(),
        EncodeError::InvalidFieldValue {
            field: "update_metadata.expected_prior_state",
            ..
        }
    ));

    let mut frame = Operation::UpdateMetadata(UpdateMetadata {
        file_id: FileId([112; 16]),
        expected_prior_state: LifecycleState::Active,
        expected_file_epoch: 1,
        last_seen_time_unix_nano: 1,
        advisory_path: None,
    })
    .encode()
    .unwrap();
    frame[21] = LifecycleState::RotatedFinalized.to_wire();
    refresh_frame_crc32c(&mut frame);
    assert!(matches!(
        Operation::decode(&frame).unwrap_err(),
        DecodeError::UnknownDiscriminant {
            field: "update_metadata.expected_prior_state",
            ..
        }
    ));
}

/// Scenario: constructing a `Transaction` with zero operations and encoding
/// it.
/// Guarantees: encoding fails closed with `EncodeError::EmptyTransaction`
/// rather than writing a wire-invalid `op_count = 0` transaction.
#[test]
fn transaction_encode_rejects_empty_operations() {
    let transaction = Transaction {
        sequence: 5,
        operations: vec![],
    };
    let err = transaction.encode().unwrap_err();
    assert!(matches!(err, EncodeError::EmptyTransaction { sequence: 5 }));
}

/// Scenario: constructing a progress-only `Transaction` (every operation is
/// `update_progress`) with more operations than `WAL_MAX_OPS_PER_TX` and
/// encoding it.
/// Guarantees: encoding fails closed with `EncodeError::TooManyOperations`
/// naming the progress-only class maximum, rather than silently truncating
/// `operations.len()` through a lossy `as u16` cast when writing
/// `op_count`.
#[test]
fn transaction_encode_rejects_too_many_progress_operations() {
    let op = Operation::UpdateProgress(UpdateProgress {
        file_id: FileId([109; 16]),
        expected_committed_offset: 0,
        expected_file_epoch: 1,
        new_committed_offset: 0,
        new_committed_frontier_guard: CommittedFrontierGuard::empty(),
        new_framing_resume: FramingResume::Clean,
        new_last_seen_time_unix_nano: 1,
        finalize: false,
    });
    let op_count = WAL_MAX_OPS_PER_TX as usize + 1;
    let transaction = Transaction {
        sequence: 1,
        operations: vec![op; op_count],
    };
    let err = transaction.encode().unwrap_err();
    assert!(matches!(
        err,
        EncodeError::TooManyOperations {
            sequence: 1,
            op_count: found,
            max,
        } if found == op_count && max == WAL_MAX_OPS_PER_TX
    ));
}

/// Scenario: constructing a non-progress `Transaction` (no operation is
/// `update_progress`) with more operations than
/// `WAL_MAX_NON_PROGRESS_OPS_PER_TX` and encoding it.
/// Guarantees: encoding fails closed with `EncodeError::TooManyOperations`
/// naming the non-progress class maximum (`256`), distinct from the wider
/// progress-only maximum (`4096`).
#[test]
fn transaction_encode_rejects_too_many_non_progress_operations() {
    let op = Operation::RegisterFile(sample_register(FileId([109; 16])));
    let op_count = WAL_MAX_NON_PROGRESS_OPS_PER_TX as usize + 1;
    let transaction = Transaction {
        sequence: 1,
        operations: vec![op; op_count],
    };
    let err = transaction.encode().unwrap_err();
    assert!(matches!(
        err,
        EncodeError::TooManyOperations {
            sequence: 1,
            op_count: found,
            max,
        } if found == op_count && max == WAL_MAX_NON_PROGRESS_OPS_PER_TX
    ));
}

/// Scenario: a progress-only transaction repeats one `file_id`, both through
/// the typed encoder and in a checksum-valid hand-crafted WAL frame.
/// Guarantees: duplicate progress keys fail at encode and decode boundaries
/// before any transaction can reach table application.
#[test]
fn progress_transaction_rejects_duplicate_file_ids() {
    let progress = |file_id| {
        Operation::UpdateProgress(UpdateProgress {
            file_id,
            expected_committed_offset: 0,
            expected_file_epoch: 1,
            new_committed_offset: 1,
            new_committed_frontier_guard: zero_guard(1),
            new_framing_resume: FramingResume::Clean,
            new_last_seen_time_unix_nano: 1,
            finalize: false,
        })
    };
    let duplicate_id = FileId([109; 16]);
    let error = Transaction {
        sequence: 1,
        operations: vec![progress(duplicate_id), progress(duplicate_id)],
    }
    .encode()
    .unwrap_err();
    assert!(matches!(
        error,
        EncodeError::DuplicateProgressFileId {
            sequence: 1,
            file_id,
        } if file_id == duplicate_id
    ));

    let first_id = FileId([110; 16]);
    let mut transaction = Transaction {
        sequence: 1,
        operations: vec![progress(first_id), progress(FileId([111; 16]))],
    }
    .encode()
    .unwrap();
    let first_op_len = u32::from_be_bytes(transaction[36..40].try_into().unwrap()) as usize;
    let second_start = 36 + 4 + first_op_len + 4;
    transaction[second_start + 5..second_start + 21].copy_from_slice(&first_id.0);
    let second_op_len = u32::from_be_bytes(
        transaction[second_start..second_start + 4]
            .try_into()
            .unwrap(),
    ) as usize;
    let second_crc_at = second_start + 4 + second_op_len;
    let second_crc = crc32c(&transaction[second_start..second_crc_at]);
    transaction[second_crc_at..second_crc_at + 4].copy_from_slice(&second_crc.to_be_bytes());
    refresh_frame_crc32c(&mut transaction);

    let mut wal = wal::encode_wal(1, TEST_NAMESPACE_ID, &[]).unwrap();
    wal.extend_from_slice(&transaction);
    let error = wal::decode_wal(&wal, &TEST_NAMESPACE_DIGEST).unwrap_err();
    assert!(matches!(
        error,
        DecodeError::DuplicateProgressFileId {
            sequence: 1,
            file_id,
        } if file_id == first_id
    ));
}

/// Scenario: a hand-crafted WAL transaction frame whose `op_count` field has
/// been overwritten to `0`, with the transaction header's own CRC-32C and
/// the transaction's trailing `frame_crc32c` both recomputed.
/// Guarantees: decoding fails closed with `DecodeError::EmptyTransaction`;
/// this decode-time check runs before any attempt to parse operations, so
/// stale trailing operation bytes past the (now-zero) declared count are
/// never touched.
#[test]
fn decode_wal_rejects_empty_transaction() {
    let transaction = Transaction {
        sequence: 1,
        operations: vec![Operation::RegisterFile(sample_register(FileId([110; 16])))],
    };
    let mut tx_frame = transaction.encode().unwrap();
    // `op_count` is a 2-byte field at fixed transaction-header offset 28.
    tx_frame[28..30].copy_from_slice(&0u16.to_be_bytes());
    let header_crc = crc32c(&tx_frame[0..32]);
    tx_frame[32..36].copy_from_slice(&header_crc.to_be_bytes());
    refresh_frame_crc32c(&mut tx_frame);
    let mut bytes = wal::encode_wal(1, TEST_NAMESPACE_ID, &[]).unwrap();
    bytes.extend_from_slice(&tx_frame);
    let err = wal::decode_wal(&bytes, &TEST_NAMESPACE_DIGEST).unwrap_err();
    assert!(matches!(err, DecodeError::EmptyTransaction { sequence: 1 }));
}

/// Scenario: a hand-crafted WAL transaction frame whose `op_count` field has
/// been overwritten to `WAL_MAX_OPS_PER_TX + 1`, with the transaction
/// header's own CRC-32C and the transaction's trailing `frame_crc32c` both
/// recomputed.
/// Guarantees: decoding fails closed with `DecodeError::TooManyOperations`
/// before any attempt to parse that many operations out of a buffer that
/// does not actually contain them.
#[test]
fn decode_wal_rejects_too_many_operations() {
    let transaction = Transaction {
        sequence: 1,
        operations: vec![Operation::RegisterFile(sample_register(FileId([111; 16])))],
    };
    let mut tx_frame = transaction.encode().unwrap();
    let too_many = WAL_MAX_OPS_PER_TX + 1;
    tx_frame[28..30].copy_from_slice(&too_many.to_be_bytes());
    let header_crc = crc32c(&tx_frame[0..32]);
    tx_frame[32..36].copy_from_slice(&header_crc.to_be_bytes());
    refresh_frame_crc32c(&mut tx_frame);
    let mut bytes = wal::encode_wal(1, TEST_NAMESPACE_ID, &[]).unwrap();
    bytes.extend_from_slice(&tx_frame);
    let err = wal::decode_wal(&bytes, &TEST_NAMESPACE_DIGEST).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::TooManyOperations {
            sequence: 1,
            op_count,
            max,
        } if op_count == too_many && max == WAL_MAX_OPS_PER_TX
    ));
}

/// Scenario: a WAL transaction containing one operation whose payload has
/// been corrupted (a `file_id` byte flipped) without updating that
/// operation's own trailing CRC-32C, but the *enclosing transaction's*
/// trailing CRC-32C has been recomputed over the now-corrupted bytes, so
/// the outer transaction frame is fully self-consistent.
/// Guarantees: decoding still fails closed with a `ChecksumMismatch` for
/// the nested `wal_operation`; a valid outer transaction checksum can never
/// substitute for validating each operation's own checksum.
#[test]
fn nested_operation_checksum_is_validated_even_when_outer_transaction_checksum_is_valid() {
    let file_id = FileId([112; 16]);
    let transaction = Transaction {
        sequence: 1,
        operations: vec![Operation::RegisterFile(sample_register(file_id))],
    };
    let mut bytes =
        wal::encode_wal(1, TEST_NAMESPACE_ID, std::slice::from_ref(&transaction)).unwrap();

    // Flip the first byte of the operation's `file_id` field: WAL header
    // (56) + 36-byte tx header + op_len (4) + op_code (1) = 97.
    let corrupt_at = WAL_HEADER_LEN + 36 + 4 + 1;
    bytes[corrupt_at] ^= 0xFF;

    // Recompute only the *outer* transaction frame's own trailing
    // `frame_crc32c` so it is valid again despite the corrupted nested
    // operation; the WAL header's CRC and the transaction header's own
    // `header_crc32c` are untouched (neither covers operation-body bytes).
    let tx_start = WAL_HEADER_LEN;
    let body_len =
        u32::from_be_bytes(bytes[tx_start + 20..tx_start + 24].try_into().unwrap()) as usize;
    let tx_end = tx_start + 36 + body_len + 4;
    refresh_frame_crc32c(&mut bytes[tx_start..tx_end]);

    let err = wal::decode_wal(&bytes, &TEST_NAMESPACE_DIGEST).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::ChecksumMismatch {
            context: "wal_operation",
            ..
        }
    ));
}

/// Scenario: a WAL operation frame with one extra byte inserted into its
/// payload beyond what its defined fields consume, with `op_len` and the
/// operation's own CRC-32C both updated to reflect the (now longer, but
/// still internally consistent) frame.
/// Guarantees: decoding fails closed with `UnconsumedBytes`; v1 has no
/// extension-bytes mechanism, so a declared length that exceeds what the
/// defined fields actually consumed is always an error, never silently
/// ignored trailing data.
#[test]
fn operation_unconsumed_extension_bytes_fail_closed() {
    let mut frame = Operation::RegisterFile(sample_register(FileId([113; 16])))
        .encode()
        .unwrap();
    let insert_at = frame.len() - 4; // just before the operation's own crc32c
    insert_bytes_and_refresh(&mut frame, insert_at, &[0x00]);
    let err = Operation::decode(&frame).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::UnconsumedBytes {
            context: "operation",
            ..
        }
    ));
}

/// Scenario: a snapshot record frame with one extra byte inserted into its
/// payload beyond what its defined fields consume, with `record_len` and
/// the record's own CRC-32C both updated to reflect the (now longer, but
/// still internally consistent) frame.
/// Guarantees: decoding fails closed with `UnconsumedBytes`; the same
/// no-extension-bytes rule applies to snapshot records as to WAL
/// operations.
#[test]
fn snapshot_record_unconsumed_extension_bytes_fail_closed() {
    let record = sample_snapshot_record(FileId([114; 16]));
    let mut frame = record.encode().unwrap();
    let insert_at = frame.len() - 4; // just before the record's own crc32c
    insert_bytes_and_refresh(&mut frame, insert_at, &[0x00]);
    let err = SnapshotRecord::decode(&frame).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::UnconsumedBytes {
            context: "snapshot_record",
            ..
        }
    ));
}

/// Scenario: two WAL transactions with sequence numbers `1` and `3` (a gap
/// at `2`).
/// Guarantees: decoding fails closed with `SequenceOutOfOrder`; a WAL
/// transaction sequence must be exactly one greater than the previous
/// transaction's sequence.
#[test]
fn wal_sequence_gap_fails_closed() {
    let tx1 = Transaction {
        sequence: 1,
        operations: vec![Operation::RegisterFile(sample_register(FileId([115; 16])))],
    };
    let tx3 = Transaction {
        sequence: 3,
        operations: vec![Operation::RegisterFile(sample_register(FileId([116; 16])))],
    };
    let bytes = wal::encode_wal(1, TEST_NAMESPACE_ID, &[tx1, tx3]).unwrap();
    let err = wal::decode_wal(&bytes, &TEST_NAMESPACE_DIGEST).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::SequenceOutOfOrder {
            expected: 2,
            found: 3,
        }
    ));
}

/// Scenario: a hand-crafted `reset_quarantined_file` operation whose
/// `audit_reason` has been zeroed out to length `0` (the type-safe encoder
/// refuses to construct this: it requires a non-empty `audit_reason`).
/// Guarantees: decoding fails closed with `EmptyRequiredField` for
/// `reset_quarantined_file.audit_reason`; every quarantine reset is by
/// definition an operator-authorized administrative action and must carry
/// a real audit trail.
#[test]
fn reset_quarantined_file_empty_audit_reason_fails_closed() {
    let op = ResetQuarantinedFile {
        file_id: FileId([117; 16]),
        expected_quarantine_epoch: 1,
        action: ResetQuarantineAction::KeepFailed,
        resulting_epoch: 1,
        resulting_offset: 0,
        new_committed_frontier_guard: zero_guard(0),
        new_framing_resume: FramingResume::Clean,
        new_fingerprint: b"fp".to_vec(),
        action_time_unix_nano: 1,
        namespace_id: NAMESPACE.to_owned(),
        audit_reason: "operator note".to_owned(),
    };
    let mut frame = Operation::ResetQuarantinedFile(op).encode().unwrap();
    let audit_reason_len = "operator note".len();
    let audit_reason_offset = frame.len() - 4 - audit_reason_len - 2;
    zero_out_var_field(&mut frame, audit_reason_offset);
    let err = Operation::decode(&frame).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::EmptyRequiredField {
            field: "reset_quarantined_file.audit_reason",
        }
    ));
}

/// Scenario: a quarantine reset omits its mandatory raw checkpoint namespace
/// ID through the typed encoder and a hand-crafted checksum-valid frame.
/// Guarantees: both boundaries reject the empty namespace before replay can
/// look up a coincidentally matching `file_id`.
#[test]
fn reset_quarantined_file_requires_nonempty_namespace_id() {
    let mut operation = ResetQuarantinedFile {
        file_id: FileId([118; 16]),
        expected_quarantine_epoch: 1,
        action: ResetQuarantineAction::KeepFailed,
        resulting_epoch: 1,
        resulting_offset: 0,
        new_committed_frontier_guard: zero_guard(0),
        new_framing_resume: FramingResume::Clean,
        new_fingerprint: b"fp".to_vec(),
        action_time_unix_nano: 1,
        namespace_id: String::new(),
        audit_reason: "operator note".to_owned(),
    };
    assert!(matches!(
        Operation::ResetQuarantinedFile(operation.clone())
            .encode()
            .unwrap_err(),
        EncodeError::RequiredFieldEmpty {
            field: "reset_quarantined_file.namespace_id",
        }
    ));

    operation.namespace_id = NAMESPACE.to_owned();
    let namespace_len = operation.namespace_id.len();
    let audit_reason_len = operation.audit_reason.len();
    let mut frame = Operation::ResetQuarantinedFile(operation).encode().unwrap();
    let namespace_offset = frame.len() - 4 - (2 + audit_reason_len) - (2 + namespace_len);
    zero_out_var_field(&mut frame, namespace_offset);
    assert!(matches!(
        Operation::decode(&frame).unwrap_err(),
        DecodeError::EmptyRequiredField {
            field: "reset_quarantined_file.namespace_id",
        }
    ));
}

/// Scenario: a hand-crafted `remove_file` operation with
/// `administrative == 0` but a nonzero `namespace_id_len` (the type-safe
/// encoder refuses to construct this combination).
/// Guarantees: decoding fails closed with `UnexpectedPresentField` for
/// `remove_file.namespace_id`; a namespace_id is only ever meaningful
/// alongside an administrative removal.
#[test]
fn remove_file_forbidden_namespace_present_fails_closed() {
    let op = RemoveFile {
        file_id: FileId([118; 16]),
        expected_file_epoch: 1,
        expected_prior_state: LifecycleState::Active,
        removal_reason: 1,
        removal_time_unix_nano: 1,
        administrative: false,
        namespace_id: None,
        audit_reason: None,
    };
    let mut frame = Operation::RemoveFile(op).encode().unwrap();
    // namespace_id_len is at frame offset 37 (see the frame layout note in
    // remove_file_required_namespace_missing_fails_closed below).
    frame[37..39].copy_from_slice(&1u16.to_be_bytes());
    insert_bytes_and_refresh(&mut frame, 39, b"x");
    let err = Operation::decode(&frame).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::UnexpectedPresentField {
            field: "remove_file.namespace_id",
        }
    ));
}

/// Scenario: a hand-crafted, administrative `remove_file` operation whose
/// `namespace_id` has been zeroed out to length `0` (the type-safe encoder
/// refuses to construct this: it requires a non-empty `namespace_id`
/// whenever `administrative == 1`).
/// Guarantees: decoding fails closed with `EmptyRequiredField` for
/// `remove_file.namespace_id`.
#[test]
fn remove_file_required_namespace_missing_fails_closed() {
    let op = RemoveFile {
        file_id: FileId([119; 16]),
        expected_file_epoch: 1,
        expected_prior_state: LifecycleState::Quarantined,
        removal_reason: 1,
        removal_time_unix_nano: 1,
        administrative: true,
        namespace_id: Some(NAMESPACE.to_owned()),
        audit_reason: Some("operator cleanup".to_owned()),
    };
    let mut frame = Operation::RemoveFile(op).encode().unwrap();
    // namespace_id_len follows op_code(1) + file_id(16) +
    // expected_file_epoch(4) + expected_prior_state(1) + removal_reason(2)
    // + removal_time_unix_nano(8) + administrative(1) = 33 in the payload,
    // plus the 4-byte op_len prefix: frame offset 37.
    zero_out_var_field(&mut frame, 37);
    let err = Operation::decode(&frame).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::EmptyRequiredField {
            field: "remove_file.namespace_id",
        }
    ));
}

/// Scenario: an administrative `remove_file` operation whose
/// `namespace_id` bytes have been corrupted to an invalid UTF-8 sequence,
/// with the operation's own CRC-32C recomputed so the corruption is
/// isolated to the field's content (the declared length is unchanged).
/// Guarantees: decoding fails closed with `InvalidUtf8` for
/// `remove_file.namespace_id` rather than accepting non-UTF-8 bytes in a
/// field this format documents as UTF-8.
#[test]
fn remove_file_invalid_utf8_namespace_fails_closed() {
    let op = RemoveFile {
        file_id: FileId([120; 16]),
        expected_file_epoch: 1,
        expected_prior_state: LifecycleState::Quarantined,
        removal_reason: 1,
        removal_time_unix_nano: 1,
        administrative: true,
        namespace_id: Some("ns".to_owned()),
        audit_reason: Some("operator cleanup".to_owned()),
    };
    let mut frame = Operation::RemoveFile(op).encode().unwrap();
    // namespace_id_bytes start right after namespace_id_len (frame offset
    // 37..39): frame offset 39.
    frame[39] = 0xFF; // 0xFF is never a valid UTF-8 byte
    refresh_frame_crc32c(&mut frame);
    let err = Operation::decode(&frame).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::InvalidUtf8 {
            field: "remove_file.namespace_id",
        }
    ));
}

/// Scenario: a valid, one-record snapshot whose footer `record_count_echo`
/// has been overwritten to a value other than the header's `record_count`,
/// with the footer's own CRC-32C recomputed.
/// Guarantees: decoding fails closed rather than trusting a footer echo
/// that disagrees with what was actually parsed.
#[test]
fn snapshot_footer_record_count_echo_mismatch_fails_closed() {
    let record = sample_snapshot_record(FileId([121; 16]));
    let mut bytes =
        snapshot::encode_snapshot(1, TEST_NAMESPACE_ID, std::slice::from_ref(&record)).unwrap();
    let footer_start = bytes.len() - SNAPSHOT_FOOTER_LEN;
    let count_echo_at = footer_start + 8 + 8; // footer_magic(8) + total_record_bytes(8)
    bytes[count_echo_at..count_echo_at + 4].copy_from_slice(&2u32.to_be_bytes());
    let crc = crc32c(&bytes[footer_start..footer_start + 20]);
    bytes[footer_start + 20..footer_start + 24].copy_from_slice(&crc.to_be_bytes());
    let err = snapshot::decode_snapshot(&bytes, &TEST_NAMESPACE_DIGEST).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::UnconsumedBytes {
            context: "snapshot_footer.record_count_echo",
            ..
        }
    ));
}

/// Scenario: a valid, one-record snapshot whose footer `total_record_bytes`
/// has been overwritten to a value that disagrees with the bytes actually
/// parsed, with the footer's own CRC-32C recomputed.
/// Guarantees: decoding fails closed rather than trusting a footer byte
/// count that disagrees with what was actually parsed.
#[test]
fn snapshot_footer_total_record_bytes_mismatch_fails_closed() {
    let record = sample_snapshot_record(FileId([122; 16]));
    let mut bytes =
        snapshot::encode_snapshot(1, TEST_NAMESPACE_ID, std::slice::from_ref(&record)).unwrap();
    let footer_start = bytes.len() - SNAPSHOT_FOOTER_LEN;
    let total_bytes_at = footer_start + 8; // footer_magic(8)
    bytes[total_bytes_at..total_bytes_at + 8].copy_from_slice(&999u64.to_be_bytes());
    let crc = crc32c(&bytes[footer_start..footer_start + 20]);
    bytes[footer_start + 20..footer_start + 24].copy_from_slice(&crc.to_be_bytes());
    let err = snapshot::decode_snapshot(&bytes, &TEST_NAMESPACE_DIGEST).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::UnconsumedBytes {
            context: "snapshot_footer.total_record_bytes",
            ..
        }
    ));
}

/// Scenario: a transaction whose first operation (registering file A)
/// succeeds and whose second operation (an `update_progress` against
/// never-registered file B) fails.
/// Guarantees: the whole transaction is rejected and file A's registration
/// is not durably visible afterward; `apply_transaction`'s bounded,
/// touched-key scratch map never partially commits a failed transaction,
/// even though it no longer clones the entire table to achieve this.
#[test]
fn apply_transaction_rolls_back_all_operations_on_later_failure() {
    let file_a = FileId([123; 16]);
    let file_b = FileId([124; 16]);
    let mut table = CheckpointTable::new();

    let register_a = Operation::RegisterFile(sample_register(file_a));
    let update_b = Operation::UpdateProgress(UpdateProgress {
        file_id: file_b,
        expected_committed_offset: 0,
        expected_file_epoch: 1,
        new_committed_offset: 10,
        new_committed_frontier_guard: zero_guard(10),
        new_framing_resume: FramingResume::Clean,
        new_last_seen_time_unix_nano: 2,
        finalize: false,
    });
    let err = table
        .apply_transaction(
            &Transaction {
                sequence: 1,
                operations: vec![register_a, update_b],
            },
            NAMESPACE,
        )
        .unwrap_err();
    assert!(matches!(err, ApplyError::ImpossibleTransition { .. }));
    assert!(
        table.get(&file_a).is_none(),
        "the first operation's effect must not persist after the transaction as a whole failed"
    );
    assert!(table.get(&file_b).is_none());
    assert!(table.is_empty());
}

/// Scenario: one progress-only transaction contains two updates for the same
/// `file_id`.
/// Guarantees: the duplicate progress key fails before staged application and
/// the complete table remains unchanged.
#[test]
fn apply_transaction_rejects_duplicate_progress_keys() {
    let file_id = FileId([125; 16]);
    let mut table = CheckpointTable::new();
    table
        .apply_transaction(
            &Transaction {
                sequence: 1,
                operations: vec![Operation::RegisterFile(sample_register(file_id))],
            },
            NAMESPACE,
        )
        .unwrap();

    let advance = Operation::UpdateProgress(UpdateProgress {
        file_id,
        expected_committed_offset: 0,
        expected_file_epoch: 1,
        new_committed_offset: 100,
        new_committed_frontier_guard: zero_guard(100),
        new_framing_resume: FramingResume::Clean,
        new_last_seen_time_unix_nano: 2,
        finalize: false,
    });
    let stale_second_update = Operation::UpdateProgress(UpdateProgress {
        file_id,
        expected_committed_offset: 0, // stale: this transaction's own first op just staged 100
        expected_file_epoch: 1,
        new_committed_offset: 5,
        new_committed_frontier_guard: zero_guard(5),
        new_framing_resume: FramingResume::Clean,
        new_last_seen_time_unix_nano: 3,
        finalize: false,
    });
    let err = table
        .apply_transaction(
            &Transaction {
                sequence: 2,
                operations: vec![advance, stale_second_update],
            },
            NAMESPACE,
        )
        .unwrap_err();
    assert!(matches!(
        err,
        ApplyError::DuplicateProgressFileId { file_id: found } if found == file_id
    ));
    assert_eq!(
        table.get(&file_id).unwrap().committed_offset,
        0,
        "neither operation in the failed transaction may persist, including the first op's own staged change"
    );
}

// ---------------------------------------------------------------------
// Duplicate file_id and quarantine-evidence-invariant fail-closed tests.
// ---------------------------------------------------------------------

/// Scenario: `encode_snapshot` is given two records that share a `file_id`.
/// Guarantees: encoding fails closed with `EncodeError::DuplicateFileId`
/// rather than writing a snapshot with two records under the same key,
/// which no decoder-side table (keyed by `file_id`) could faithfully
/// represent.
#[test]
fn encode_snapshot_rejects_duplicate_file_id() {
    let file_id = FileId([130; 16]);
    let first = sample_snapshot_record(file_id);
    let mut second = sample_snapshot_record(file_id);
    second.committed_offset = 999; // differs, but the key is what matters
    let err = snapshot::encode_snapshot(1, TEST_NAMESPACE_ID, &[first, second]).unwrap_err();
    assert!(matches!(err, EncodeError::DuplicateFileId { file_id: found } if found == file_id));
}

/// Scenario: two distinct snapshot records claim the same locator while both
/// are live, including a checksum-valid byte stream crafted from an otherwise
/// valid Active plus RotatedFinalized pair.
/// Guarantees: snapshot encoding, direct table seeding, and byte decoding all
/// reject the duplicate live locator before WAL replay.
#[test]
fn snapshot_and_table_reject_duplicate_live_locators() {
    let first_file_id = FileId([131; 16]);
    let second_file_id = FileId([132; 16]);
    let first = sample_snapshot_record(first_file_id);
    let second = sample_snapshot_record(second_file_id);
    let error =
        snapshot::encode_snapshot(1, NAMESPACE, &[first.clone(), second.clone()]).unwrap_err();
    assert!(matches!(
        error,
        EncodeError::InvalidSnapshotState {
            file_id: found,
            ..
        } if found == second_file_id
    ));
    let error =
        CheckpointTable::from_snapshot_records(vec![first.clone(), second.clone()]).unwrap_err();
    assert!(matches!(
        error,
        DecodeError::InvalidSnapshotState {
            file_id: found,
            ..
        } if found == second_file_id
    ));

    let mut finalized = second;
    finalized.lifecycle_state = LifecycleState::RotatedFinalized;
    let mut encoded =
        snapshot::encode_snapshot(1, NAMESPACE, &[first, finalized]).expect("pair is valid");
    let first_len = u32::from_be_bytes(encoded[60..64].try_into().unwrap()) as usize;
    let second_start = 60 + 4 + first_len + 4;
    let second_len =
        u32::from_be_bytes(encoded[second_start..second_start + 4].try_into().unwrap()) as usize;
    // With the minimal fixture, lifecycle follows 120 payload bytes.
    encoded[second_start + 4 + 120] = LifecycleState::Active.to_wire();
    let second_crc_at = second_start + 4 + second_len;
    let second_crc = crc32c(&encoded[second_start..second_crc_at]);
    encoded[second_crc_at..second_crc_at + 4].copy_from_slice(&second_crc.to_be_bytes());

    let error =
        snapshot::decode_snapshot(&encoded, &super::primitives::namespace_digest(NAMESPACE))
            .unwrap_err();
    assert!(matches!(
        error,
        DecodeError::InvalidSnapshotState {
            file_id: found,
            ..
        } if found == second_file_id
    ));
}

/// Scenario: a hand-assembled snapshot byte stream (header + two identical
/// per-frame-valid records sharing a `file_id` + footer, all counts/CRCs
/// self-consistent) that could never have been produced by this codec's
/// own `encode_snapshot` (which now refuses to write it).
/// Guarantees: decoding still fails closed with
/// `DecodeError::DuplicateFileId`; the duplicate-key check does not rely
/// solely on the encoder refusing to produce such a file.
#[test]
fn decode_snapshot_rejects_duplicate_file_id() {
    let file_id = FileId([131; 16]);
    let record = sample_snapshot_record(file_id);
    let record_frame = record.encode().unwrap();

    // Assemble header + two copies of the same record frame + footer by
    // hand, matching `encode_snapshot`'s own layout exactly.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(super::primitives::SNAPSHOT_MAGIC);
    bytes.extend_from_slice(&super::primitives::FORMAT_VERSION.to_be_bytes());
    bytes.extend_from_slice(&0u16.to_be_bytes()); // flags
    bytes.extend_from_slice(&1u64.to_be_bytes()); // generation
    bytes.extend_from_slice(&TEST_NAMESPACE_DIGEST);
    bytes.extend_from_slice(&2u32.to_be_bytes()); // record_count
    let header_crc = crc32c(&bytes[0..56]);
    bytes.extend_from_slice(&header_crc.to_be_bytes());

    let footer_start = bytes.len() + record_frame.len() * 2;
    bytes.extend_from_slice(&record_frame);
    bytes.extend_from_slice(&record_frame);

    bytes.extend_from_slice(super::primitives::SNAPSHOT_FOOTER_MAGIC);
    let total_record_bytes = (record_frame.len() * 2) as u64;
    bytes.extend_from_slice(&total_record_bytes.to_be_bytes());
    bytes.extend_from_slice(&2u32.to_be_bytes()); // record_count_echo
    let footer_crc = crc32c(&bytes[footer_start..]);
    bytes.extend_from_slice(&footer_crc.to_be_bytes());

    let err = snapshot::decode_snapshot(&bytes, &TEST_NAMESPACE_DIGEST).unwrap_err();
    assert!(
        matches!(err, DecodeError::DuplicateFileId { file_id: found, context: "snapshot" } if found == file_id)
    );
}

/// Scenario: `CheckpointTable::from_snapshot_records` is given two records
/// that share a `file_id`, bypassing the byte-level codec entirely.
/// Guarantees: seeding the table fails closed with
/// `DecodeError::DuplicateFileId` rather than silently keeping only the
/// last record for that key.
#[test]
fn from_snapshot_records_rejects_duplicate_file_id() {
    let file_id = FileId([132; 16]);
    let first = sample_snapshot_record(file_id);
    let second = sample_snapshot_record(file_id);
    let err = CheckpointTable::from_snapshot_records(vec![first, second]).unwrap_err();
    assert!(matches!(err, DecodeError::DuplicateFileId { file_id: found, .. } if found == file_id));
}

/// Scenario: encoding a `SnapshotRecord` whose `lifecycle_state` is
/// `Quarantined` but whose `quarantine_evidence` is `None`.
/// Guarantees: encoding fails closed with
/// `EncodeError::MissingQuarantineEvidence` instead of silently emitting an
/// ambiguous encoding (this codec no longer relies on a debug-only
/// assertion for this invariant).
#[test]
fn encode_quarantined_record_without_evidence_fails_closed() {
    let mut record = sample_snapshot_record(FileId([133; 16]));
    record.lifecycle_state = LifecycleState::Quarantined;
    record.quarantine_evidence = None;
    let err = record.encode().unwrap_err();
    assert!(matches!(err, EncodeError::MissingQuarantineEvidence { .. }));
}

/// Scenario: encoding a `SnapshotRecord` whose `lifecycle_state` is
/// `Active` but whose `quarantine_evidence` is `Some(..)`.
/// Guarantees: encoding fails closed with
/// `EncodeError::UnexpectedQuarantineEvidence`; quarantine evidence is
/// defined to be present iff the state is `Quarantined`.
#[test]
fn encode_non_quarantined_record_with_evidence_fails_closed() {
    let mut record = sample_snapshot_record(FileId([134; 16]));
    record.lifecycle_state = LifecycleState::Active;
    record.quarantine_evidence = Some(QuarantineEvidence {
        reason_code: 1,
        observed_size: 1,
        quarantine_epoch: 1,
        quarantine_time_unix_nano: 1,
    });
    let err = record.encode().unwrap_err();
    assert!(matches!(
        err,
        EncodeError::UnexpectedQuarantineEvidence { .. }
    ));
}

/// Scenario: a quarantined snapshot record carries either version-1 reserved
/// reason code with otherwise complete quarantine evidence.
/// Guarantees: the snapshot encoder rejects `0x0000` and `0x0004` at its own
/// public boundary instead of relying on store-level append validation.
#[test]
fn snapshot_encoder_rejects_reserved_quarantine_reason() {
    for reason_code in [REASON_CODE_RESERVED, 0x0004] {
        let mut record = sample_snapshot_record(FileId([135; 16]));
        record.lifecycle_state = LifecycleState::Quarantined;
        record.quarantine_evidence = Some(QuarantineEvidence {
            reason_code,
            observed_size: 1,
            quarantine_epoch: 1,
            quarantine_time_unix_nano: 1,
        });

        assert!(matches!(
            record.encode().expect_err("reserved reason must fail"),
            EncodeError::ReservedReasonCode {
                field: "quarantine_evidence.reason_code",
                reason_code: found,
            } if found == reason_code
        ));
    }
}

/// Scenario: a `quarantine_file` WAL operation carries either version-1
/// reserved reason code.
/// Guarantees: operation encoding rejects `0x0000` and `0x0004` before
/// producing a frame, including direct codec callers.
#[test]
fn quarantine_operation_encoder_rejects_reserved_reason() {
    for reason_code in [REASON_CODE_RESERVED, 0x0004] {
        let operation = Operation::QuarantineFile(QuarantineFile {
            file_id: FileId([136; 16]),
            expected_file_epoch: 1,
            reason_code,
            locator: Locator::Unspecified,
            observed_size: 1,
            quarantine_epoch: 1,
            quarantine_time_unix_nano: 1,
        });

        assert!(matches!(
            operation.encode().expect_err("reserved reason must fail"),
            EncodeError::ReservedReasonCode {
                field: "quarantine_file.reason_code",
                reason_code: found,
            } if found == reason_code
        ));
    }
}

/// Scenario: a `remove_file` WAL operation carries reserved removal reason
/// `0x0000`.
/// Guarantees: operation encoding rejects the reserved value for ordinary
/// and administrative callers before any frame bytes are returned.
#[test]
fn remove_operation_encoder_rejects_reserved_reason() {
    let operation = Operation::RemoveFile(RemoveFile {
        file_id: FileId([137; 16]),
        expected_file_epoch: 1,
        expected_prior_state: LifecycleState::Active,
        removal_reason: REASON_CODE_RESERVED,
        removal_time_unix_nano: 1,
        administrative: false,
        namespace_id: None,
        audit_reason: None,
    });

    assert!(matches!(
        operation.encode().expect_err("reserved reason must fail"),
        EncodeError::ReservedReasonCode {
            field: "remove_file.removal_reason",
            reason_code: REASON_CODE_RESERVED,
        }
    ));
}

/// Scenario: a caller seeds `CheckpointTable` directly with a record marked
/// `Quarantined` but carrying no quarantine evidence.
/// Guarantees: direct table construction enforces the same reachable-state
/// invariant as snapshot decoding and fails closed before replay.
#[test]
fn direct_table_seed_rejects_inconsistent_quarantined_record() {
    let file_id = FileId([135; 16]);
    let mut record = sample_snapshot_record(file_id);
    record.lifecycle_state = LifecycleState::Quarantined;
    record.quarantine_evidence = None; // inconsistent, bypassing encode_snapshot's check
    let err = CheckpointTable::from_snapshot_records(vec![record]).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::InvalidSnapshotState {
            file_id: found,
            ..
        } if found == file_id
    ));
}
