// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Replay, state-transition, and codec-integration compatibility tests for
//! filelog checkpoints.
//!
//! The standalone checkpoint crate owns codec conformance coverage. These
//! tests retain core-nodes replay invariants and focused checks that old,
//! independently generated checkpoint bytes remain compatible at the
//! integration boundary.

use super::DecodeError;
use super::apply::CheckpointTable;
use super::current_marker;
use super::error::ApplyError;
use super::primitives::{
    AdvisoryPath, CommittedFrontierGuard, FileId, FramingResume, LifecycleState, Locator,
    TRUNCATE_RESET_REASON_READ_NEW, crc32c,
};
use super::snapshot::{self, QuarantineEvidence, SnapshotRecord};
use super::test_vectors::*;
use super::wal::{
    Operation, QuarantineFile, RegisterFile, RemoveFile, ResetAfterTruncate, ResetQuarantineAction,
    ResetQuarantinedFile, Transaction, TransactionScan, UpdateFingerprint, UpdateMetadata,
    UpdateProgress, WAL_HEADER_LEN, decode_wal_header, encode_transaction, encode_wal_header,
    scan_next_transaction,
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

/// A deterministic `Active` snapshot record used to seed replay tables.
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

fn decode_complete_wal(
    bytes: &[u8],
    expected_namespace_digest: &[u8; 32],
) -> (u64, Vec<Transaction>) {
    let header = decode_wal_header(
        bytes
            .get(..WAL_HEADER_LEN)
            .expect("the complete compatibility WAL has a header"),
    )
    .expect("the compatibility WAL header decodes");
    assert_eq!(&header.namespace_digest, expected_namespace_digest);

    let mut suffix = &bytes[WAL_HEADER_LEN..];
    let mut expected_sequence = 1u64;
    let mut transactions = Vec::new();
    while !suffix.is_empty() {
        match scan_next_transaction(suffix, expected_sequence)
            .expect("the compatibility transaction decodes")
            .expect("the nonempty suffix produces a scan result")
        {
            TransactionScan::Complete {
                transaction,
                consumed,
            } => {
                suffix = &suffix[consumed..];
                transactions.push(transaction);
                expected_sequence += 1;
            }
            TransactionScan::Incomplete { bytes } => {
                panic!("the complete compatibility WAL ended with {bytes} incomplete bytes")
            }
        }
    }
    (header.generation, transactions)
}

fn encode_complete_wal(
    generation: u64,
    checkpoint_id: &str,
    transactions: &[Transaction],
) -> Vec<u8> {
    let mut bytes =
        encode_wal_header(generation, checkpoint_id).expect("the compatibility WAL header encodes");
    for transaction in transactions {
        bytes.extend_from_slice(
            &encode_transaction(transaction).expect("the compatibility transaction encodes"),
        );
    }
    bytes
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
    let snapshot = snapshot::decode_snapshot_with_limit(
        SNAPSHOT_EMPTY_GENERATION_0,
        &TEST_NAMESPACE_DIGEST,
        u32::MAX,
    )
    .unwrap();
    assert_eq!(snapshot.generation, 0);
    assert!(snapshot.records.is_empty());
    assert_eq!(
        snapshot::encode_snapshot(0, TEST_NAMESPACE_ID, &[]).unwrap(),
        SNAPSHOT_EMPTY_GENERATION_0
    );

    let (generation, transactions) =
        decode_complete_wal(WAL_HEADER_GENERATION_0, &TEST_NAMESPACE_DIGEST);
    assert_eq!(generation, 0);
    assert!(transactions.is_empty());
    assert_eq!(
        encode_complete_wal(0, TEST_NAMESPACE_ID, &[]),
        WAL_HEADER_GENERATION_0
    );
}

/// Scenario: replaying the golden WAL vector's transactions against a fresh
/// checkpoint table, in order.
/// Guarantees: `register_file` followed by `update_progress` produces the
/// expected end state (`Active`, `committed_offset == 4096`), matching the
/// realistic register-then-Ack sequence the vector represents.
#[test]
fn replay_wal_valid_two_tx_updates_table() {
    let (_, transactions) = decode_complete_wal(WAL_VALID_TWO_TX, &TEST_NAMESPACE_DIGEST);
    let mut table = CheckpointTable::new();
    table.replay(&transactions, NAMESPACE).unwrap();
    let record = table.get(&FileId::from_bytes(WAL_VECTOR_FILE_ID)).unwrap();
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
    let (generation, transactions) =
        decode_complete_wal(WAL_ALL_OPERATIONS, &TEST_NAMESPACE_DIGEST);
    assert_eq!(generation, 5);
    assert_eq!(transactions.len(), 8);
    for (index, transaction) in transactions.iter().enumerate() {
        assert_eq!(transaction.sequence, index as u64 + 1);
        assert_eq!(transaction.operations.len(), 1);
    }
    assert!(matches!(
        transactions[0].operations[0],
        Operation::RegisterFile(_)
    ));
    assert!(matches!(
        transactions[1].operations[0],
        Operation::UpdateProgress(_)
    ));
    assert!(matches!(
        transactions[2].operations[0],
        Operation::UpdateFingerprint(_)
    ));
    assert!(matches!(
        transactions[3].operations[0],
        Operation::UpdateMetadata(_)
    ));
    assert!(matches!(
        transactions[4].operations[0],
        Operation::ResetAfterTruncate(_)
    ));
    assert!(matches!(
        transactions[5].operations[0],
        Operation::QuarantineFile(_)
    ));
    assert!(matches!(
        transactions[6].operations[0],
        Operation::ResetQuarantinedFile(_)
    ));
    assert!(matches!(
        transactions[7].operations[0],
        Operation::RemoveFile(_)
    ));

    let Operation::UpdateMetadata(metadata) = &transactions[3].operations[0] else {
        unreachable!("operation kind checked above");
    };
    assert_eq!(metadata.expected_prior_state, LifecycleState::Active);
    assert_eq!(metadata.expected_file_epoch, 1);
    assert_eq!(
        metadata.advisory_path,
        Some(AdvisoryPath::from_unix_bytes(b"/var/log/new.log").unwrap())
    );
    let Operation::ResetAfterTruncate(reset) = &transactions[4].operations[0] else {
        unreachable!("operation kind checked above");
    };
    assert_eq!(reset.new_fingerprint, b"new");
    let Operation::ResetQuarantinedFile(reset) = &transactions[6].operations[0] else {
        unreachable!("operation kind checked above");
    };
    assert_eq!(reset.action, ResetQuarantineAction::KeepFailed);
    assert_eq!(reset.new_fingerprint, b"new");
    assert_eq!(reset.action_time_unix_nano, 105);
    assert_eq!(reset.namespace_id, TEST_NAMESPACE_ID);
    assert_eq!(reset.audit_reason, "keep failed");
    assert_eq!(
        encode_complete_wal(5, TEST_NAMESPACE_ID, &transactions),
        WAL_ALL_OPERATIONS
    );

    let mut table = CheckpointTable::new();
    table.replay(&transactions[..7], TEST_NAMESPACE_ID).unwrap();
    let preserved = table.get(&FileId::from_bytes(WAL_VECTOR_FILE_ID)).unwrap();
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
        .apply_transaction(&transactions[7], TEST_NAMESPACE_ID)
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

    let (_, transactions) = decode_complete_wal(&wal_bytes, &TEST_NAMESPACE_DIGEST);
    let mut table = CheckpointTable::new();
    let error = table
        .replay(&transactions[..7], TEST_NAMESPACE_ID)
        .unwrap_err();
    assert!(matches!(error, ApplyError::KeepFailedStateChange { .. }));
    let record = table.get(&FileId::from_bytes(WAL_VECTOR_FILE_ID)).unwrap();
    assert_eq!(record.lifecycle_state, LifecycleState::Quarantined);
    assert_eq!(record.fingerprint, b"new");
}

/// Scenario: core replay receives the standalone codec's independently
/// generated keep_failed transaction whose resulting epoch is 99 while the
/// quarantined record's epoch is 1.
/// Guarantees: structural decoding remains permissive for PR1B, while the
/// replay table rejects the state change as `KeepFailedStateChange`.
#[test]
fn standalone_keep_failed_epoch_mutation_is_rejected_by_replay() {
    let bytes = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../filelog-checkpoint/tests/fixtures/transaction-keep-failed-mutation.bin"
    ));
    let Some(TransactionScan::Complete {
        mut transaction, ..
    }) = scan_next_transaction(bytes, 1).expect("the standalone fixture decodes")
    else {
        panic!("the standalone fixture must contain one complete transaction");
    };
    let [Operation::ResetQuarantinedFile(reset)] = transaction.operations.as_slice() else {
        panic!("the standalone fixture must contain one keep_failed operation");
    };
    assert_eq!(reset.action, ResetQuarantineAction::KeepFailed);
    assert_eq!(reset.expected_quarantine_epoch, 1);
    assert_eq!(reset.resulting_epoch, 99);

    let file_id = reset.file_id;
    let locator = Locator::PosixDevIno { dev: 1, ino: 2 };
    let guard = CommittedFrontierGuard::compute(9, b"123456789").unwrap();
    let mut table = CheckpointTable::new();
    table
        .apply_transaction(
            &Transaction {
                sequence: 1,
                operations: vec![Operation::RegisterFile(RegisterFile {
                    file_id,
                    file_epoch: 1,
                    committed_offset: 9,
                    committed_frontier_guard: guard,
                    fingerprint: b"mutated-state".to_vec(),
                    ignored_header_bytes: 0,
                    locator,
                    framing_profile_version: 1,
                    framing_profile_digest: sample_digest(),
                    framing_resume: FramingResume::Clean,
                    last_seen_time_unix_nano: 1,
                    advisory_path: AdvisoryPath::unavailable(),
                })],
            },
            "app-logs",
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
                    locator,
                    observed_size: 9,
                    quarantine_epoch: 1,
                    quarantine_time_unix_nano: 2,
                })],
            },
            "app-logs",
        )
        .unwrap();

    transaction.sequence = 3;
    let error = table
        .apply_transaction(&transaction, "app-logs")
        .expect_err("the impossible keep_failed epoch must fail replay");
    assert!(matches!(
        error,
        ApplyError::KeepFailedStateChange {
            file_id: found,
            ..
        } if found == file_id
    ));
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
    let mut op = sample_register(FileId::from_bytes([1; 16]));
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
    let mut op = sample_register(FileId::from_bytes([15; 16]));
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
    let mut op = sample_register(FileId::from_bytes([16; 16]));
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
    let mut op = sample_register(FileId::from_bytes([17; 16]));
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

/// Scenario: an identical `register_file` is replayed, followed by a
/// conflicting registration for the same `file_id`.
/// Guarantees: identical replay is idempotent, while conflicting identity
/// data fails closed instead of overwriting the durable record.
#[test]
fn register_file_identical_replay_is_idempotent_but_conflict_fails_closed() {
    let file_id = FileId::from_bytes([2; 16]);
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
    let file_id = FileId::from_bytes([3; 16]);
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
    let file_id = FileId::from_bytes([4; 16]);
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
    let file_id = FileId::from_bytes([94; 16]);
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
    let file_id = FileId::from_bytes([95; 16]);
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
    let file_id = FileId::from_bytes([96; 16]);
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
    let file_id = FileId::from_bytes([97; 16]);
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
    let file_id = FileId::from_bytes([5; 16]);
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
    let file_id = FileId::from_bytes([6; 16]);
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
    let file_id = FileId::from_bytes([7; 16]);
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
    let file_id = FileId::from_bytes([8; 16]);
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
    let file_id = FileId::from_bytes([98; 16]);
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
    let file_id = FileId::from_bytes([9; 16]);
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
    let file_id = FileId::from_bytes([99; 16]);
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
    let file_id = FileId::from_bytes([12; 16]);
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
    let file_id = FileId::from_bytes([11; 16]);
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
    let file_id = FileId::from_bytes([12; 16]);
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
    let file_id = FileId::from_bytes([13; 16]);
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
    assert!(matches!(err, ApplyError::KeepFailedStateChange { .. }));
}

/// Scenario: `keep_failed` carries a replacement fingerprint that differs
/// from the quarantined record while every offset/epoch field still matches.
/// Guarantees: audit-only quarantine retention cannot change fingerprint or
/// any other operational state.
#[test]
fn reset_quarantined_file_keep_failed_preserves_fingerprint() {
    let file_id = FileId::from_bytes([100; 16]);
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
    let file_id = FileId::from_bytes([101; 16]);
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

    for target in [file_id, FileId::from_bytes([102; 16])] {
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
    let file_id = FileId::from_bytes([14; 16]);
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
    let file_id = FileId::from_bytes([15; 16]);
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
        file_id: FileId::from_bytes([200; 16]),
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
    let file_id = FileId::from_bytes([16; 16]);
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
    let old_file_id = FileId::from_bytes([103; 16]);
    let new_file_id = FileId::from_bytes([104; 16]);
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
    let file_id = FileId::from_bytes([17; 16]);
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
    let file_id = FileId::from_bytes([18; 16]);
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
    let file_id = FileId::from_bytes([19; 16]);
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
    let decoded =
        snapshot::decode_snapshot_with_limit(&encoded, &TEST_NAMESPACE_DIGEST, u32::MAX).unwrap();
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
    let file_id = FileId::from_bytes([20; 16]);
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

/// Scenario: a transaction whose first operation (registering file A)
/// succeeds and whose second operation (an `update_progress` against
/// never-registered file B) fails.
/// Guarantees: the whole transaction is rejected and file A's registration
/// is not durably visible afterward; `apply_transaction`'s bounded,
/// touched-key scratch map never partially commits a failed transaction,
/// even though it no longer clones the entire table to achieve this.
#[test]
fn apply_transaction_rolls_back_all_operations_on_later_failure() {
    let file_a = FileId::from_bytes([123; 16]);
    let file_b = FileId::from_bytes([124; 16]);
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
    let file_id = FileId::from_bytes([125; 16]);
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

/// Scenario: two distinct decoded snapshot records claim the same locator
/// while both are live.
/// Guarantees: direct replay-table seeding rejects the duplicate live locator
/// before any WAL transaction can be applied.
#[test]
fn table_rejects_duplicate_live_locators() {
    let first_file_id = FileId::from_bytes([131; 16]);
    let second_file_id = FileId::from_bytes([132; 16]);
    let first = sample_snapshot_record(first_file_id);
    let second = sample_snapshot_record(second_file_id);
    let error = CheckpointTable::from_snapshot_records(vec![first, second]).unwrap_err();
    assert!(matches!(
        error,
        DecodeError::InvalidSnapshotState {
            file_id: found,
            ..
        } if found == second_file_id
    ));
}

/// Scenario: `CheckpointTable::from_snapshot_records` is given two records
/// that share a `file_id`, bypassing the byte-level codec entirely.
/// Guarantees: seeding the table fails closed with
/// `DecodeError::DuplicateFileId` rather than silently keeping only the
/// last record for that key.
#[test]
fn from_snapshot_records_rejects_duplicate_file_id() {
    let file_id = FileId::from_bytes([132; 16]);
    let first = sample_snapshot_record(file_id);
    let second = sample_snapshot_record(file_id);
    let err = CheckpointTable::from_snapshot_records(vec![first, second]).unwrap_err();
    assert!(matches!(err, DecodeError::DuplicateFileId { file_id: found, .. } if found == file_id));
}

/// Scenario: a caller seeds `CheckpointTable` directly with a record marked
/// `Quarantined` but carrying no quarantine evidence.
/// Guarantees: direct table construction enforces the same reachable-state
/// invariant as snapshot decoding and fails closed before replay.
#[test]
fn direct_table_seed_rejects_inconsistent_quarantined_record() {
    let file_id = FileId::from_bytes([135; 16]);
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
